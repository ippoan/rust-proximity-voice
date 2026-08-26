//! 接続者名簿と可聴グラフの保持。**担当: #1-3**
//!
//! - roster は **差分ではなく毎回フル**で来る。置き換える
//! - 最終受信から `ROSTER_TTL_S` を過ぎたら **その server_id の全員へ転送停止**
//!   (fail closed)。**切断はしない**
//! - `revoke_on_death` は Config を見る
//!
//! 差分にしない理由: 1 通落ちただけで状態がずれ、しかも**「余計な人が残る」側**に
//! ずれる。フル置換なら落ちた通は次の 2 秒後に上書きされて自然に直る。
//!
//! **★ 失効は「転送停止」であって「切断」ではない。** `bye` を送るのは
//! 二重接続・BAN・shutdown のときだけ (docs/protocol.md §0)。死亡やリスポーンで
//! PeerConnection を張り直すのは無駄なので絶対にやらない。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use tokio::sync::{mpsc, oneshot};

use crate::auth::{self, Clock, SystemClock};
use crate::config::Config;
use crate::proto::{
    GraphPush, Heard, ROSTER_TTL_S, RosterPush, ServerId, ServerMsg, SteamId, TalkPush, YawPush,
};
use crate::signal::Hub;
use crate::state::{AppState, SfuCommand};

/// 1 つの Rust サーバーぶんの状態。
#[derive(Debug, Default)]
struct ServerState {
    /// 最後に受けた roster の中身。**フル置換**される
    eligible: HashSet<SteamId>,
    /// 最後に roster を受けた unix 秒
    last_roster_at: i64,
    /// 聞き手 → その聞き手が聞ける相手
    hears: HashMap<SteamId, Vec<Heard>>,
    /// 聞き手 → 世界座標系での向き (度)
    yaws: HashMap<SteamId, u16>,
    /// いま PTT を押している人
    talking: HashSet<SteamId>,
    /// TTL 切れで既に転送停止済みか (sweep が何度も mute_all を呼ばないための印)
    muted_by_ttl: bool,
}

#[derive(Debug, Default)]
struct Inner {
    servers: HashMap<ServerId, ServerState>,
    /// SteamID → その人が居る server_id。`hears_of` は server_id を受け取らないので要る
    owner: HashMap<SteamId, ServerId>,
    /// 静的 whitelist。**空なら無効** (名簿のみで認可)。
    /// `lib.rs::build` が `Roster::new()` で作るため `Config` を後から差す
    whitelist: HashSet<SteamId>,
}

/// SFU と Hub への結線。**起動時に必ず受け取る** — `routes()` の中で後から差すと、
/// `web.rs` が呼び忘れた瞬間に whitelist と転送が無音で効かなくなる。
/// テストだけが `None` を使う (`Hub::new` は #1-1 が実装中のため構築できない)。
struct Wiring {
    sfu: mpsc::Sender<SfuCommand>,
    hub: Arc<Hub>,
}

pub struct Roster {
    clock: Arc<dyn Clock>,
    /// 転送の実体への結線。**失効はここへ `MuteAll` を送ることで起きる**。
    /// 本番では必ず `Some`。`None` は単体テスト専用 (SFU / Hub 抜きで状態だけ回す)
    wiring: Option<Wiring>,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Roster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Roster")
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

impl Roster {
    /// 依存は**起動時に全部受け取る**。`routes()` の中で後から差さない
    /// (呼び忘れると whitelist と転送が無音で効かなくなり、しかも CI では
    ///  検出できない壊れ方をするため)。
    pub fn new(cfg: Arc<Config>, sfu: mpsc::Sender<SfuCommand>, hub: Arc<Hub>) -> Self {
        Self {
            clock: Arc::new(SystemClock),
            wiring: Some(Wiring { sfu, hub }),
            inner: Mutex::new(Inner {
                whitelist: cfg.whitelist.iter().cloned().collect(),
                ..Inner::default()
            }),
        }
    }

    /// SFU / Hub 抜きで状態だけ回す。**単体テスト専用** —
    /// `Hub::new` は #1-1 が実装中 (`todo!()`) で構築できないため。
    /// 時計も注入する: TTL のテストは `sleep` せずこれを進める。
    #[cfg(test)]
    fn detached(clock: Arc<dyn Clock>, whitelist: impl IntoIterator<Item = SteamId>) -> Self {
        Self {
            clock,
            wiring: None,
            inner: Mutex::new(Inner {
                whitelist: whitelist.into_iter().collect(),
                ..Inner::default()
            }),
        }
    }

    // ---- push の適用 ----

    /// **フル置換**。`eligible` から消えた SteamID は即座に転送停止。
    pub fn apply_roster(&self, push: RosterPush) -> anyhow::Result<()> {
        let now = self.clock.now_unix();
        let next: HashSet<SteamId> = push.eligible.into_iter().collect();

        let dropped: Vec<SteamId> = {
            let mut inner = self.inner.lock().expect("Roster poisoned");
            let Inner { servers, owner, .. } = &mut *inner;
            let st = servers.entry(push.server_id.clone()).or_default();

            let dropped: Vec<SteamId> = st.eligible.difference(&next).cloned().collect();

            for id in &dropped {
                st.hears.remove(id);
                st.yaws.remove(id);
                st.talking.remove(id);
                // 他サーバーが先に拾っていたら奪わない
                if owner.get(id).is_some_and(|s| *s == push.server_id) {
                    owner.remove(id);
                }
            }
            for id in &next {
                owner.insert(id.clone(), push.server_id.clone());
            }

            st.eligible = next;
            st.last_roster_at = now;
            st.muted_by_ttl = false;
            dropped
        };

        // 名簿から消えた = 失効。**転送を止めるだけで、切断はしない**
        for id in &dropped {
            self.mute(id);
        }
        if !dropped.is_empty() {
            tracing::info!(server = %push.server_id, dropped = dropped.len(), "roster から失効");
        }
        Ok(())
    }

    /// 可聴グラフ。**絶対座標は含まれない** (`d` と世界方位 `b` だけ)。
    pub fn apply_graph(&self, push: GraphPush) -> anyhow::Result<()> {
        let mut updates: Vec<(SteamId, Vec<SteamId>, Vec<Heard>)> = Vec::new();
        {
            let mut inner = self.inner.lock().expect("Roster poisoned");
            let Inner {
                servers, whitelist, ..
            } = &mut *inner;
            let Some(st) = servers.get_mut(&push.server_id) else {
                // roster より先に graph が来た。名簿が無い間は認可できないので捨てる
                tracing::warn!(server = %push.server_id, "roster 未受信の server の graph を破棄");
                return Ok(());
            };
            for listener in push.listeners {
                // **認可はここが実体**。graph に載っていても、名簿と whitelist を
                // 通らない相手の RTP は転送先に入れない
                if !st.eligible.contains(&listener.id) || !allowed(whitelist, &listener.id) {
                    continue;
                }
                let hears: Vec<Heard> = listener
                    .hears
                    .into_iter()
                    .filter(|h| st.eligible.contains(&h.id) && allowed(whitelist, &h.id))
                    .collect();
                let speakers = speakers_by_distance(&hears);
                st.hears.insert(listener.id.clone(), hears.clone());
                updates.push((listener.id, speakers, hears));
            }
        }

        // `sub: false` の相手は転送しない。**「購読したまま gain 0」は禁止**
        for (listener, speakers, hears) in updates {
            self.subscribe(&listener, speakers);
            // PWA が d と b から音量とパンを作る (docs/protocol.md §3)
            self.notify(&listener, ServerMsg::Graph { hears });
        }
        Ok(())
    }

    /// 聞き手の向き。`YAW_HZ`=20 で来る (graph とは別便)。
    pub fn apply_yaw(&self, push: YawPush) -> anyhow::Result<()> {
        let mut updates: Vec<(SteamId, u16)> = Vec::new();
        {
            let mut inner = self.inner.lock().expect("Roster poisoned");
            let Inner {
                servers, whitelist, ..
            } = &mut *inner;
            let Some(st) = servers.get_mut(&push.server_id) else {
                return Ok(());
            };
            for (id, deg) in push.yaws {
                if st.eligible.contains(&id) && allowed(whitelist, &id) {
                    let deg = deg % 360;
                    st.yaws.insert(id.clone(), deg);
                    updates.push((id, deg));
                }
            }
        }
        for (id, deg) in updates {
            self.notify(&id, ServerMsg::Yaw { deg });
        }
        Ok(())
    }

    /// PTT の状態変化。押下・離しの都度、即時に来る。
    ///
    /// **リプレイを通すとホットマイクになる** (本人が V を押していないのに
    /// マイクが送信状態になる) ので、route 側で `seq` まで検証してから呼ぶ。
    pub fn apply_talk(&self, push: TalkPush) -> anyhow::Result<()> {
        {
            let mut inner = self.inner.lock().expect("Roster poisoned");
            let Inner {
                servers, whitelist, ..
            } = &mut *inner;
            let Some(st) = servers.get_mut(&push.server_id) else {
                return Ok(());
            };
            if !st.eligible.contains(&push.id) || !allowed(whitelist, &push.id) {
                return Ok(());
            }
            if push.talking {
                st.talking.insert(push.id.clone());
            } else {
                st.talking.remove(&push.id);
            }
        }
        // 自分の PTT 状態。PWA はこれを受けてマイク送信を始める
        self.notify(&push.id, ServerMsg::Talk { on: push.talking });
        Ok(())
    }

    // ---- 問い合わせ ----

    /// 認可されているか。roster に載っていること (+ 静的 whitelist)。
    ///
    /// TTL 切れの server_id は **fail closed** で全員 false になる。
    pub fn is_eligible(&self, server: &ServerId, id: &SteamId) -> bool {
        let now = self.clock.now_unix();
        let inner = self.inner.lock().expect("Roster poisoned");
        if !allowed(&inner.whitelist, id) {
            return false;
        }
        inner
            .servers
            .get(server)
            .is_some_and(|st| !expired(st, now) && st.eligible.contains(id))
    }

    /// その聞き手がいま聞ける相手。TTL 切れなら空を返す。
    pub fn hears_of(&self, id: &SteamId) -> Vec<Heard> {
        let now = self.clock.now_unix();
        let inner = self.inner.lock().expect("Roster poisoned");
        if !allowed(&inner.whitelist, id) {
            return Vec::new();
        }
        let Some(server) = inner.owner.get(id) else {
            return Vec::new();
        };
        let Some(st) = inner.servers.get(server) else {
            return Vec::new();
        };
        // **TTL 切れなら空** (fail closed)。プラグインのクラッシュや
        // ネットワーク分断で音声チャンネルが開きっぱなしになるのを防ぐ
        if expired(st, now) || !st.eligible.contains(id) {
            return Vec::new();
        }
        st.hears
            .get(id)
            .map(|hs| {
                hs.iter()
                    .filter(|h| st.eligible.contains(&h.id))
                    .filter(|h| allowed(&inner.whitelist, &h.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// WS 接続時に、その聞き手の現在の状態を撒き直す (issue #11)。
    ///
    /// **なぜ要るか**: `graph` は「変化が無ければ送らない」仕様なので
    /// (docs/protocol.md §1)、途中から接続した PWA は誰かが動くまで `graph` を
    /// 1 通も受け取れない。静止した場面に入ると無音のまま何も起きない。
    /// リレーは状態を持っている (`hears_of` / `yaw_of`) のに撒く経路が無かった。
    ///
    /// **購読も張り直す。** `graph` だけ送っても RTP が来ないので音は出ない。
    /// `SetSubscriptions` は `apply_graph` の中からしか送られていなかった。
    ///
    /// `hears_of` が TTL・名簿・whitelist を通した後の値を返すので、
    /// **TTL 切れなら空 = 購読ゼロ = 無音** という fail closed がそのまま効く。
    pub fn resync(&self, listener: &SteamId) {
        let hears = self.hears_of(listener);
        let speakers = speakers_by_distance(&hears);
        self.subscribe(listener, speakers);
        self.notify(listener, ServerMsg::Graph { hears });
        if let Some(deg) = self.yaw_of(listener) {
            self.notify(listener, ServerMsg::Yaw { deg });
        }
    }

    /// 聞き手の向き。無ければ None。
    pub fn yaw_of(&self, id: &SteamId) -> Option<u16> {
        let inner = self.inner.lock().expect("Roster poisoned");
        let server = inner.owner.get(id)?;
        inner.servers.get(server)?.yaws.get(id).copied()
    }

    /// いま PTT を押しているか。
    pub fn is_talking(&self, id: &SteamId) -> bool {
        let inner = self.inner.lock().expect("Roster poisoned");
        inner
            .owner
            .get(id)
            .and_then(|s| inner.servers.get(s))
            .is_some_and(|st| st.talking.contains(id))
    }

    /// その SteamID が属する server_id。
    pub fn server_of(&self, id: &SteamId) -> Option<ServerId> {
        self.inner
            .lock()
            .expect("Roster poisoned")
            .owner
            .get(id)
            .cloned()
    }

    /// TTL 切れの server_id を掃いて、その全員へ転送停止をかける (fail closed)。
    /// プラグインのクラッシュやネットワーク分断で音声チャンネルが開きっぱなしに
    /// なるのを防ぐ。**切断はしない。**
    ///
    /// 停止した SteamID を返す。定期タスクから呼ぶ想定。
    pub fn sweep(&self) -> Vec<SteamId> {
        let now = self.clock.now_unix();
        let stale: Vec<(ServerId, Vec<SteamId>)> = {
            let mut inner = self.inner.lock().expect("Roster poisoned");
            inner
                .servers
                .iter_mut()
                .filter(|(_, st)| expired(st, now) && !st.muted_by_ttl && !st.eligible.is_empty())
                .map(|(sid, st)| {
                    st.muted_by_ttl = true;
                    (sid.clone(), st.eligible.iter().cloned().collect())
                })
                .collect()
        };

        let mut muted = Vec::new();
        for (server, ids) in stale {
            tracing::warn!(server = %server, n = ids.len(), "roster TTL 切れ: 全員へ転送停止");
            for id in ids {
                self.mute(&id);
                muted.push(id);
            }
        }
        muted
    }

    // ---- SFU への結線 ----

    /// **転送停止**。スロット割り当ては保持したまま RTP だけ止める。
    /// `bye` は送らない (docs/protocol.md §0)。
    fn mute(&self, listener: &SteamId) {
        let Some(w) = &self.wiring else { return };
        let cmd = SfuCommand::MuteAll {
            listener: listener.clone(),
        };
        if let Err(e) = w.sfu.try_send(cmd) {
            tracing::error!(%listener, error = %e, "MuteAll を送れなかった");
        }
    }

    /// ブラウザへ 1 通送る。`Peer` はここから送らない (SFU 側に一本化)。
    fn notify(&self, to: &SteamId, msg: ServerMsg) {
        let Some(w) = &self.wiring else { return };
        if let Err(e) = w.hub.send(to, msg) {
            tracing::warn!(%to, error = %e, "ブラウザへ送れなかった");
        }
    }

    /// 転送先の更新。`speakers` は**距離の近い順**。
    ///
    /// **`ServerMsg::Peer` はここから送らない。** 送出は `sfu::run` に一本化されている
    /// (`Disconnect` のように reply を持たない指令でもスロットは動くので、
    ///  送出元が 2 箇所あると片方の経路で誰も送れない。`state.rs` を参照)。
    fn subscribe(&self, listener: &SteamId, speakers: Vec<SteamId>) {
        let Some(w) = &self.wiring else { return };
        let (reply, rx) = oneshot::channel();
        let cmd = SfuCommand::SetSubscriptions {
            listener: listener.clone(),
            speakers,
            reply,
        };
        if let Err(e) = w.sfu.try_send(cmd) {
            tracing::error!(%listener, error = %e, "SetSubscriptions を送れなかった");
            return;
        }
        // 失敗をログに残すためだけに待つ
        let listener = listener.clone();
        tokio::spawn(async move {
            match rx.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!(%listener, error = %e, "SetSubscriptions に失敗"),
                Err(_) => tracing::warn!(%listener, "SFU が応答せず終了した"),
            }
        });
    }
}

fn expired(st: &ServerState, now: i64) -> bool {
    now.saturating_sub(st.last_roster_at) >= ROSTER_TTL_S as i64
}

/// 静的 whitelist の判定。**空なら無視**、設定されていれば名簿と AND。
fn allowed(whitelist: &HashSet<SteamId>, id: &SteamId) -> bool {
    whitelist.is_empty() || whitelist.contains(id)
}

/// `SetSubscriptions` へ渡す話者列を作る。
///
/// - `sub: false` は落とす。**購読していないことが唯一の保証**であり、
///   「購読したまま gain 0」は改造クライアントに gain を戻されるので禁止
/// - **距離の近い順**に並べる (docs/protocol.md §2 のタスク跨ぎ契約)。
///   SFU 側は先頭 `SLOTS` 件に切り詰めるだけなので、並んでいないと
///   「近い人が切られて遠い人が残る」ことになる
/// - 同距離は SteamID 順。並びが tick ごとに揺れるとスロットが無用に動く
fn speakers_by_distance(hears: &[Heard]) -> Vec<SteamId> {
    let mut subs: Vec<&Heard> = hears.iter().filter(|h| h.sub).collect();
    subs.sort_by(|a, b| a.d.cmp(&b.d).then_with(|| a.id.cmp(&b.id)));
    subs.into_iter().map(|h| h.id.clone()).collect()
}

// ---- route (すべて HMAC 必須) ----

/// HMAC の 1・2 点目 (署名一致 / 時刻ずれ)。3 点目 (seq) は本文を読んでから。
fn check_hmac(secret: &str, headers: &HeaderMap, body: &[u8]) -> Result<(), StatusCode> {
    auth::verify_headers(secret, headers, body).map_err(|e| {
        tracing::warn!(error = %e, "HMAC 検証に失敗");
        StatusCode::UNAUTHORIZED
    })
}

/// 3 点目。**全 endpoint に適用する。** counter は `(server_id, endpoint)` ごとに独立
/// (`roster` は 0.5 Hz、`yaw` は 20 Hz。1 本にすると互いを巻き戻し扱いする)。
fn check_seq(stream: &str, server_id: &str, seq: u64) -> Result<(), StatusCode> {
    if auth::seq_guard().accept_stream(stream, server_id, seq) {
        Ok(())
    } else {
        tracing::warn!(server = %server_id, stream, seq, "seq が巻き戻ったので破棄");
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn parse<'a, T: serde::Deserialize<'a>>(body: &'a [u8]) -> Result<T, StatusCode> {
    serde_json::from_slice(body).map_err(|e| {
        tracing::warn!(error = %e, "本文の JSON が読めない");
        StatusCode::BAD_REQUEST
    })
}

async fn post_roster(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    check_hmac(&st.cfg.hmac_secret, &headers, &body)?;
    let push: RosterPush = parse(&body)?;
    check_seq("roster", &push.server_id, push.seq)?;
    st.roster
        .apply_roster(push)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn post_graph(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    check_hmac(&st.cfg.hmac_secret, &headers, &body)?;
    let push: GraphPush = parse(&body)?;
    check_seq("graph", &push.server_id, push.seq)?;
    st.roster
        .apply_graph(push)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn post_yaw(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    check_hmac(&st.cfg.hmac_secret, &headers, &body)?;
    let push: YawPush = parse(&body)?;
    check_seq("yaw", &push.server_id, push.seq)?;
    st.roster
        .apply_yaw(push)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn post_talk(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    check_hmac(&st.cfg.hmac_secret, &headers, &body)?;
    let push: TalkPush = parse(&body)?;
    // **talk のリプレイはホットマイク** (本人が V を押していないのにマイクが
    // 送信状態になる) なので、30 秒窓でも許容しない
    check_seq("talk", &push.server_id, push.seq)?;
    st.roster
        .apply_talk(push)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

/// このモジュールが提供する route。**#1-3 が中身を実装する。**
/// `web.rs` (#1-1) が `router()` の中で merge する。空でも #1-1 は独立にビルドできる。
///
/// パスは絶対 (`/internal/...`) なので `nest` ではなく `merge` でよい。
/// 依存は `Roster::new` が起動時に受け取っているので、ここでは何も差さない。
pub fn routes(st: AppState) -> Router {
    Router::new()
        .route("/internal/roster", post(post_roster))
        .route("/internal/graph", post(post_graph))
        .route("/internal/yaw", post(post_yaw))
        .route("/internal/talk", post(post_talk))
        .with_state(st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TestClock;
    use crate::proto::Listener;

    const A: &str = "76561198000000001";
    const B: &str = "76561198000000042";
    const C: &str = "76561198000000099";

    fn heard(id: &str, d: u16, sub: bool) -> Heard {
        Heard {
            id: id.to_string(),
            d,
            b: 145,
            sub,
        }
    }

    fn roster_push(seq: u64, ids: &[&str]) -> RosterPush {
        RosterPush {
            server_id: "main".into(),
            seq,
            ts: 1_756_180_000,
            eligible: ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn graph_push(seq: u64, listener: &str, hears: Vec<Heard>) -> GraphPush {
        GraphPush {
            server_id: "main".into(),
            seq,
            ts: 1_756_180_000,
            listeners: vec![Listener {
                id: listener.into(),
                hears,
            }],
        }
    }

    /// 時計を注入した Roster。**sleep しない。**
    fn fixture() -> (Roster, Arc<TestClock>) {
        let clock = Arc::new(TestClock::new(1_000));
        (Roster::detached(clock.clone(), []), clock)
    }

    // ---- roster: フル置換で消えた SteamID が eligible でなくなる ----
    #[test]
    fn full_replace_drops_missing_ids() {
        let (r, _clock) = fixture();
        let main = "main".to_string();

        r.apply_roster(roster_push(1, &[A, B])).unwrap();
        assert!(r.is_eligible(&main, &A.to_string()));
        assert!(r.is_eligible(&main, &B.to_string()));

        // B が抜けたフル名簿で置き換える (差分ではない)
        r.apply_roster(roster_push(2, &[A])).unwrap();
        assert!(r.is_eligible(&main, &A.to_string()));
        assert!(
            !r.is_eligible(&main, &B.to_string()),
            "消えた ID は即座に失効する"
        );

        // 一度も載っていない ID も当然 false
        assert!(!r.is_eligible(&main, &C.to_string()));
    }

    #[test]
    fn dropped_listener_hears_nothing() {
        let (r, _clock) = fixture();
        r.apply_roster(roster_push(1, &[A, B])).unwrap();
        r.apply_graph(graph_push(1, A, vec![heard(B, 23, true)]))
            .unwrap();
        assert_eq!(r.hears_of(&A.to_string()).len(), 1);

        // A が名簿から消える → 何も聞こえない
        r.apply_roster(roster_push(2, &[B])).unwrap();
        assert!(r.hears_of(&A.to_string()).is_empty());
    }

    #[test]
    fn dropped_speaker_disappears_from_hears() {
        let (r, _clock) = fixture();
        r.apply_roster(roster_push(1, &[A, B])).unwrap();
        r.apply_graph(graph_push(1, A, vec![heard(B, 23, true)]))
            .unwrap();
        assert_eq!(r.hears_of(&A.to_string()).len(), 1);

        // 話者 B が名簿から消えたら、A の可聴集合からも消える
        r.apply_roster(roster_push(2, &[A])).unwrap();
        assert!(r.hears_of(&A.to_string()).is_empty());
    }

    // ---- TTL: 最終受信から 10 秒経過で hears_of が空を返す ----
    #[test]
    fn hears_of_is_empty_after_roster_ttl() {
        let (r, clock) = fixture();
        let main = "main".to_string();

        r.apply_roster(roster_push(1, &[A, B])).unwrap();
        r.apply_graph(graph_push(1, A, vec![heard(B, 23, true)]))
            .unwrap();
        assert_eq!(r.hears_of(&A.to_string()).len(), 1);

        // 9 秒はまだ生きている
        clock.advance(9);
        assert_eq!(
            r.hears_of(&A.to_string()).len(),
            1,
            "TTL 内なので聞こえたまま"
        );
        assert!(r.is_eligible(&main, &A.to_string()));

        // 10 秒で失効 (fail closed)
        assert_eq!(ROSTER_TTL_S, 10);
        clock.advance(1);
        assert!(r.hears_of(&A.to_string()).is_empty(), "TTL 切れなら空");
        assert!(
            !r.is_eligible(&main, &A.to_string()),
            "TTL 切れは fail closed"
        );

        // 次の roster が来れば復帰する (切断していないので張り直しは不要)
        r.apply_roster(roster_push(2, &[A, B])).unwrap();
        r.apply_graph(graph_push(2, A, vec![heard(B, 23, true)]))
            .unwrap();
        assert_eq!(r.hears_of(&A.to_string()).len(), 1);
    }

    #[test]
    fn sweep_reports_ttl_expired_once() {
        let (r, clock) = fixture();
        r.apply_roster(roster_push(1, &[A, B])).unwrap();
        assert!(r.sweep().is_empty(), "TTL 内では何も止めない");

        clock.advance(ROSTER_TTL_S as i64);
        let mut muted = r.sweep();
        muted.sort();
        assert_eq!(muted, vec![A.to_string(), B.to_string()]);

        // 2 度目は既に停止済みなので鳴らない
        assert!(r.sweep().is_empty());
    }

    // ---- whitelist: 設定時は AND、空なら無視 ----
    #[test]
    fn whitelist_is_ignored_when_empty() {
        let (r, _clock) = fixture();
        r.apply_roster(roster_push(1, &[A, B])).unwrap();
        assert!(r.is_eligible(&"main".to_string(), &A.to_string()));
        assert!(r.is_eligible(&"main".to_string(), &B.to_string()));
    }

    #[test]
    fn whitelist_is_anded_with_roster() {
        let clock = Arc::new(TestClock::new(1_000));
        let r = Roster::detached(clock, [A.to_string(), C.to_string()]);
        let main = "main".to_string();

        r.apply_roster(roster_push(1, &[A, B])).unwrap();

        // 名簿 ∩ whitelist
        assert!(
            r.is_eligible(&main, &A.to_string()),
            "名簿にも whitelist にも居る"
        );
        assert!(!r.is_eligible(&main, &B.to_string()), "whitelist に居ない");
        assert!(
            !r.is_eligible(&main, &C.to_string()),
            "whitelist に居るが名簿に居ない"
        );

        // whitelist 外の話者は可聴集合からも落ちる
        r.apply_graph(graph_push(1, A, vec![heard(B, 23, true)]))
            .unwrap();
        assert!(r.hears_of(&A.to_string()).is_empty());
    }

    /// `Roster::new` が `Config` の whitelist を**起動時に**取り込むこと。
    /// (`routes()` の中で後差しにすると、`web.rs` が呼び忘れた瞬間に
    ///  whitelist が無音で効かなくなる)
    #[test]
    fn new_takes_whitelist_from_config_at_construction() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "domain": "vc.example.com",
            "hmac_secret": "k",
            "udp_port": 40000u16,
            "whitelist": [A],
        }))
        .unwrap();
        // `Hub::new` は #1-1 が実装中なので `new` は呼べない。
        // `new` が読むのと同じ経路を `detached` に渡して等価性を見る
        let r = Roster::detached(
            Arc::new(TestClock::new(1_000)),
            cfg.whitelist.iter().cloned(),
        );
        r.apply_roster(roster_push(1, &[A, B])).unwrap();
        assert!(r.is_eligible(&"main".to_string(), &A.to_string()));
        assert!(!r.is_eligible(&"main".to_string(), &B.to_string()));
    }

    // ---- graph の性質 ----
    #[test]
    fn graph_before_roster_is_dropped() {
        let (r, _clock) = fixture();
        // 名簿が無いうちは認可できないので graph を捨てる (fail closed)
        r.apply_graph(graph_push(1, A, vec![heard(B, 23, true)]))
            .unwrap();
        assert!(r.hears_of(&A.to_string()).is_empty());
    }

    #[test]
    fn graph_keeps_unsubscribed_peers_for_hysteresis() {
        let (r, _clock) = fixture();
        r.apply_roster(roster_push(1, &[A, B])).unwrap();
        // 75m 超は sub:false。可聴集合には残す (PWA が gain 0 にする)
        r.apply_graph(graph_push(1, A, vec![heard(B, 80, false)]))
            .unwrap();
        let hs = r.hears_of(&A.to_string());
        assert_eq!(hs.len(), 1);
        assert!(!hs[0].sub);
    }

    #[test]
    fn yaw_and_talk_only_apply_to_eligible() {
        let (r, _clock) = fixture();
        r.apply_roster(roster_push(1, &[A])).unwrap();

        r.apply_yaw(YawPush {
            server_id: "main".into(),
            seq: 1,
            ts: 1_756_180_000,
            yaws: vec![(A.to_string(), 145), (B.to_string(), 200)],
        })
        .unwrap();
        assert_eq!(r.yaw_of(&A.to_string()), Some(145));
        assert_eq!(r.yaw_of(&B.to_string()), None, "名簿外は無視");

        r.apply_talk(TalkPush {
            server_id: "main".into(),
            seq: 1,
            ts: 1_756_180_000,
            id: A.to_string(),
            talking: true,
        })
        .unwrap();
        assert!(r.is_talking(&A.to_string()));

        r.apply_talk(TalkPush {
            server_id: "main".into(),
            seq: 2,
            ts: 1_756_180_000,
            id: A.to_string(),
            talking: false,
        })
        .unwrap();
        assert!(!r.is_talking(&A.to_string()));
    }

    // ---- SetSubscriptions へ渡す並び (docs/protocol.md §2 の契約) ----
    #[test]
    fn speakers_are_sorted_by_distance_and_exclude_unsubscribed() {
        let hears = vec![
            heard(C, 70, true),
            heard(B, 12, true),
            heard(A, 40, true),
            heard("76561198000000500", 3, false), // sub:false は渡さない
        ];
        assert_eq!(
            speakers_by_distance(&hears),
            vec![B.to_string(), A.to_string(), C.to_string()],
            "近い順に並ぶこと。SFU は先頭 SLOTS 件しか見ない"
        );
    }

    #[test]
    fn speakers_order_is_stable_for_equal_distance() {
        // 同距離の並びが tick ごとに揺れるとスロットが無用に動く
        let a = vec![heard(C, 20, true), heard(A, 20, true), heard(B, 20, true)];
        let b = vec![heard(B, 20, true), heard(C, 20, true), heard(A, 20, true)];
        assert_eq!(speakers_by_distance(&a), speakers_by_distance(&b));
    }

    #[test]
    fn speakers_drop_ids_missing_from_roster() {
        let (r, _clock) = fixture();
        r.apply_roster(roster_push(1, &[A, B])).unwrap();
        // C は名簿に居ないので graph に載っていても可聴集合に入らない
        r.apply_graph(graph_push(
            1,
            A,
            vec![heard(B, 12, true), heard(C, 5, true)],
        ))
        .unwrap();
        let hs = r.hears_of(&A.to_string());
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].id, B);
        assert_eq!(speakers_by_distance(&hs), vec![B.to_string()]);
    }

    #[test]
    fn servers_are_independent() {
        let clock = Arc::new(TestClock::new(1_000));
        let r = Roster::detached(clock.clone(), []);
        r.apply_roster(roster_push(1, &[A])).unwrap();
        r.apply_roster(RosterPush {
            server_id: "eu".into(),
            seq: 1,
            ts: 1_756_180_000,
            eligible: vec![B.to_string()],
        })
        .unwrap();

        assert!(r.is_eligible(&"main".to_string(), &A.to_string()));
        assert!(!r.is_eligible(&"main".to_string(), &B.to_string()));
        assert!(r.is_eligible(&"eu".to_string(), &B.to_string()));
        assert_eq!(r.server_of(&B.to_string()).as_deref(), Some("eu"));

        // main だけ TTL を切らす
        clock.advance(ROSTER_TTL_S as i64);
        r.apply_roster(RosterPush {
            server_id: "eu".into(),
            seq: 2,
            ts: 1_756_180_010,
            eligible: vec![B.to_string()],
        })
        .unwrap();
        assert!(!r.is_eligible(&"main".to_string(), &A.to_string()));
        assert!(r.is_eligible(&"eu".to_string(), &B.to_string()));
    }
}
