//! WebRTC SFU。**担当: #1-1**
//!
//! str0m を使い、RTP を宛先表に従って転送する。**音声をデコードしない。**
//! スロットプール方式 (docs/protocol.md) により、可聴集合が変わっても
//! 再ネゴシエーションを起こさないこと。
//!
//! # 設計の骨
//!
//! - **UDP は 1 ソケットだけ** bind する。str0m は STUN / DTLS / SRTP を 1 ポートに
//!   多重化でき、送信元アドレスでどの `Rtc` 宛かを判別する (`Rtc::accepts`)。
//!   ファイアウォールを 1 ポートで済ませるための設計要件。
//! - **スロットプールは接続時に 1 回だけ張る。** ブラウザは offer の中に
//!   `SLOTS` 本の recvonly 音声 m-line と 1 本の送信用 m-line (マイク) を入れて来る。
//!   answer では m-line を増やせない (RFC 3264) ので、**本数を決めるのはブラウザ側**。
//!   以後スロットの mid ↔ SteamID の対応を差し替えるだけで、再ネゴシエーションは起きない。
//! - **失効 = 転送の停止**であって切断ではない (docs/protocol.md §0)。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::media::{MediaData, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::proto::{SLOTS, ServerMsg, SteamId};
use crate::signal::Hub;
use crate::state::SfuCommand;

/// run loop が 1 周で待つ上限。str0m 側の timeout がこれより早ければそちらが勝つ。
const MAX_TICK: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// スロットプール
// ---------------------------------------------------------------------------

/// 1 本の音声受信スロット。`mid` は接続時に固定され、以後変わらない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// SDP の `a=mid:`。`ServerMsg::Peer.mid` に載る値
    pub mid: String,
    /// いま誰の声を流しているか。`None` は空き
    pub id: Option<SteamId>,
}

/// 聞き手 1 人ぶんのスロット表。
///
/// **ここが「再ネゴシエーションを起こさない」の実体**。可聴集合が変わっても
/// 変えるのは `Slot::id` だけで、`Slot::mid` は接続の寿命のあいだ不変。
#[derive(Debug, Clone, Default)]
pub struct SlotPool {
    slots: Vec<Slot>,
}

impl SlotPool {
    /// offer から読んだ受信用 m-line の mid を、出てきた順にスロットへ充てる。
    /// `SLOTS` 本を超えるぶんは使わない。
    pub fn new(mids: impl IntoIterator<Item = String>) -> Self {
        let slots = mids
            .into_iter()
            .take(SLOTS)
            .map(|mid| Slot { mid, id: None })
            .collect();
        Self { slots }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// その話者に割り当てられている mid。未割り当てなら `None`。
    pub fn mid_of(&self, id: &str) -> Option<&str> {
        self.slots
            .iter()
            .find(|s| s.id.as_deref() == Some(id))
            .map(|s| s.mid.as_str())
    }

    /// 特定の話者のスロットだけを解放する (その話者が落ちたとき)。
    pub fn release(&mut self, speaker: &str) -> Vec<ServerMsg> {
        let mut msgs = Vec::new();
        for slot in &mut self.slots {
            if slot.id.as_deref() != Some(speaker) {
                continue;
            }
            slot.id = None;
            msgs.push(ServerMsg::Peer {
                mid: slot.mid.clone(),
                id: None,
            });
        }
        msgs
    }

    /// 割り当てを `speakers` に合わせ、**変化ぶんだけ** `ServerMsg::Peer` で返す。
    ///
    /// `speakers` は**距離の近い順に並んでいること**。スロット数を超えたぶんは
    /// 後ろから捨てる (= 遠い相手が弾かれる)。距離そのものはこの層に無いので、
    /// 並べる責任は呼び出し側 (可聴グラフを持つ側) にある。
    ///
    /// 既に割り当て済みの相手の mid は**動かさない**。動かすと PWA 側で
    /// 音の出所が入れ替わり、音が飛ぶ。
    pub fn assign(&mut self, speakers: &[SteamId]) -> Vec<ServerMsg> {
        let capacity = self.slots.len();

        // 近い順に capacity 件。重複は先勝ちで落とす。
        let mut wanted: Vec<&SteamId> = Vec::with_capacity(capacity);
        for s in speakers {
            if wanted.len() == capacity {
                break;
            }
            if !wanted.contains(&s) {
                wanted.push(s);
            }
        }

        let mut msgs = Vec::new();

        // 1. 範囲外に出た相手を解放する。先にやると、空いた枠を同じ呼び出しで再利用できる。
        for slot in &mut self.slots {
            let Some(cur) = slot.id.as_ref() else {
                continue;
            };
            if wanted.contains(&cur) {
                continue;
            }
            slot.id = None;
            msgs.push(ServerMsg::Peer {
                mid: slot.mid.clone(),
                id: None,
            });
        }

        // 2. 新しく入って来た相手を空きスロットへ。既に居る相手には触れない。
        for want in wanted {
            if self.slots.iter().any(|s| s.id.as_ref() == Some(want)) {
                continue;
            }
            let Some(slot) = self.slots.iter_mut().find(|s| s.id.is_none()) else {
                break;
            };
            slot.id = Some(want.clone());
            msgs.push(ServerMsg::Peer {
                mid: slot.mid.clone(),
                id: Some(want.clone()),
            });
        }

        msgs
    }
}

// ---------------------------------------------------------------------------
// offer の解釈
// ---------------------------------------------------------------------------

/// offer に入っていた音声 m-line の割り振り。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AudioMLines {
    /// ブラウザが送信する m-line = このセッションのマイク
    pub mic: Option<String>,
    /// ブラウザが受信する m-line = 受信スロット
    pub slots: Vec<String>,
}

/// offer の SDP を読んで、音声 m-line を「マイク」と「受信スロット」に振り分ける。
///
/// answer で m-line を増やすことはできないので、`SLOTS` 本を張れるかどうかは
/// offer の中身で決まる。ここで数えておくと、足りないときに接続を張る前に断れる。
///
/// 方向属性が無い m-line は RFC 4566 に従い `sendrecv` とみなす。
/// 送信できる最初の音声 m-line をマイク、受信できる残りをスロットとする。
pub fn parse_audio_mlines(sdp: &str) -> AudioMLines {
    #[derive(Clone)]
    struct Section {
        audio: bool,
        mid: Option<String>,
        send: bool,
        recv: bool,
    }

    let mut sections: Vec<Section> = Vec::new();
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("m=") {
            sections.push(Section {
                audio: rest.split_whitespace().next() == Some("audio"),
                mid: None,
                // 方向属性が無ければ sendrecv
                send: true,
                recv: true,
            });
            continue;
        }
        let Some(cur) = sections.last_mut() else {
            continue; // セッションレベルの行
        };
        match line {
            _ if line.starts_with("a=mid:") => {
                cur.mid = Some(line["a=mid:".len()..].trim().to_string());
            }
            "a=sendrecv" => {
                cur.send = true;
                cur.recv = true;
            }
            "a=sendonly" => {
                cur.send = true;
                cur.recv = false;
            }
            "a=recvonly" => {
                cur.send = false;
                cur.recv = true;
            }
            "a=inactive" => {
                cur.send = false;
                cur.recv = false;
            }
            _ => {}
        }
    }

    let mut out = AudioMLines::default();
    for s in sections {
        if !s.audio {
            continue;
        }
        let Some(mid) = s.mid else { continue };
        if out.mic.is_none() && s.send {
            out.mic = Some(mid);
        } else if s.recv {
            out.slots.push(mid);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// セッション
// ---------------------------------------------------------------------------

/// 1 セッション = 1 ブラウザ。接続時に `SLOTS` 本の受信スロットを張る。
pub struct Session {
    pub steam_id: SteamId,
    rtc: Rtc,
    /// 受信スロット。mid は接続の寿命のあいだ不変
    pool: SlotPool,
    /// このブラウザのマイクが載る m-line
    mic_mid: Option<Mid>,
    /// **転送だけ止めている状態。** 切断ではない (docs/protocol.md §0)。
    /// スロットの割り当ては保持したままなので、解除しても再ネゴシエーションは起きない
    muted: bool,
}

impl Session {
    pub fn slots(&self) -> &SlotPool {
        &self.pool
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }
}

// ---------------------------------------------------------------------------
// SFU
// ---------------------------------------------------------------------------

pub struct Sfu {
    socket: Arc<UdpSocket>,
    /// answer に載せる host candidate。全セッションが**この 1 ソケット**を共有する
    candidates: Vec<SocketAddr>,
    sessions: HashMap<SteamId, Session>,
    /// 送信元 IP → その相手から見た「こちらの host candidate」。
    /// `destination_for` の説明を見よ
    routes: HashMap<IpAddr, SocketAddr>,
}

impl Sfu {
    /// **UDP を 1 本だけ** bind する。全 WebRTC トラフィックはここに多重化される。
    pub fn new(udp_port: u16) -> anyhow::Result<Self> {
        // str0m は暗号プロバイダをプロセス既定として要求する。二度目以降は無視される。
        str0m::crypto::from_feature_flags().install_process_default();

        let std_socket = std::net::UdpSocket::bind(("0.0.0.0", udp_port))?;
        std_socket.set_nonblocking(true)?;
        let bound = std_socket.local_addr()?;
        let socket = UdpSocket::from_std(std_socket)?;

        // **host candidate は既定で 1 つだけ。**
        //
        // ソケットは `0.0.0.0` に bind しているので、送信元 IP を選ぶのは
        // カーネルの経路表。宣言した candidate と実際の送信元がずれると、
        // ブラウザは返ってきた STUN を別ホストからのものとみなして捨てる。
        // 単一 IP のホスト (通常の配置) なら、外向き IP を 1 つ宣言するのが正しい。
        //
        // NAT の外側の IP など、別の宛先を足したいときは `add_host_candidate`。
        let host_ip = primary_ipv4()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let candidates = vec![SocketAddr::new(host_ip, bound.port())];

        tracing::info!(?bound, ?candidates, "sfu udp socket bound");

        Ok(Self {
            socket: Arc::new(socket),
            candidates,
            sessions: HashMap::new(),
            routes: HashMap::new(),
        })
    }

    pub fn local_port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// answer に載せる host candidate を足す (NAT 越しのグローバル IP など)。
    pub fn add_host_candidate(&mut self, addr: SocketAddr) {
        if !self.candidates.contains(&addr) {
            self.candidates.push(addr);
        }
    }

    pub fn has_session(&self, steam_id: &str) -> bool {
        self.sessions.contains_key(steam_id)
    }

    pub fn session(&self, steam_id: &str) -> Option<&Session> {
        self.sessions.get(steam_id)
    }

    /// SDP offer を受けて answer を返す。**ここで `SLOTS` 本のスロットを確定する。**
    ///
    /// 以後このセッションでは再ネゴシエーションを行わない。可聴集合の変化は
    /// `set_subscriptions` がスロットの中身を差し替えるだけで吸収する。
    pub fn accept_offer(&mut self, steam_id: &SteamId, sdp: &str) -> anyhow::Result<String> {
        let mlines = parse_audio_mlines(sdp);
        if mlines.slots.is_empty() {
            anyhow::bail!(
                "offer に受信用の音声 m-line が無い。ブラウザ側で SLOTS ({SLOTS}) 本の \
                 recvonly 音声 transceiver を offer に入れること (answer では増やせない)"
            );
        }
        if mlines.slots.len() < SLOTS {
            // 足りなくても動く (可聴人数がその本数までに制限されるだけ) ので警告に留める
            tracing::warn!(
                steam_id = %steam_id,
                got = mlines.slots.len(),
                want = SLOTS,
                "offer の受信スロットが SLOTS に足りない"
            );
        }

        let offer = SdpOffer::from_sdp_string(sdp)
            .map_err(|e| anyhow::anyhow!("offer の解析に失敗: {e}"))?;

        let mut rtc = Rtc::builder().build(Instant::now());
        for addr in &self.candidates {
            let candidate = Candidate::host(*addr, "udp")
                .map_err(|e| anyhow::anyhow!("host candidate を作れない: {e}"))?;
            rtc.add_local_candidate(candidate);
        }

        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| anyhow::anyhow!("offer を受理できない: {e}"))?;

        let session = Session {
            steam_id: steam_id.clone(),
            rtc,
            pool: SlotPool::new(mlines.slots),
            mic_mid: mlines.mic.as_deref().map(Mid::from),
            muted: false,
        };

        tracing::info!(
            steam_id = %steam_id,
            slots = session.pool.len(),
            mic = ?session.mic_mid,
            "session accepted"
        );

        // 同一 SteamID の張り直しは後勝ち (Hub 側で前のセッションに bye を送っている)
        self.sessions.insert(steam_id.clone(), session);

        Ok(answer.to_sdp_string())
    }

    /// ブラウザからの trickle ICE candidate。
    pub fn add_remote_candidate(
        &mut self,
        steam_id: &SteamId,
        candidate: &str,
    ) -> anyhow::Result<()> {
        let Some(session) = self.sessions.get_mut(steam_id) else {
            anyhow::bail!("セッションが無い: {steam_id}");
        };
        let c = Candidate::from_sdp_string(candidate)
            .map_err(|e| anyhow::anyhow!("candidate の解析に失敗: {e}"))?;
        session.rtc.add_remote_candidate(c);
        Ok(())
    }

    /// 転送先の更新。**ここが失効の実体** — 集合から外れた相手の RTP は流さない。
    /// 再ネゴシエーションは起こさず、スロットの割り当てを差し替えて
    /// `ServerMsg::Peer` を返す。
    ///
    /// `speakers` は**距離の近い順**。`SLOTS` を超えたぶんは遠い側から弾かれる。
    ///
    /// 呼ぶと `mute_all` の停止は解除される (= docs/protocol.md の「転送停止 / 再開」)。
    pub fn set_subscriptions(
        &mut self,
        listener: &SteamId,
        speakers: &[SteamId],
    ) -> anyhow::Result<Vec<ServerMsg>> {
        let Some(session) = self.sessions.get_mut(listener) else {
            anyhow::bail!("セッションが無い: {listener}");
        };
        session.muted = false;
        Ok(session.pool.assign(speakers))
    }

    /// 全転送を止める (roster TTL 切れ・死亡など)。**切断はしない。**
    ///
    /// スロットの割り当ては保持する。解放して `Peer{id:null}` を撒くと、
    /// 死亡・リスポーンのたびに PWA 側のスロットが動いてしまうため。
    /// 再開は `set_subscriptions` を呼ぶだけでよく、再ネゴシエーションは起きない。
    pub fn mute_all(&mut self, listener: &SteamId) -> anyhow::Result<()> {
        let Some(session) = self.sessions.get_mut(listener) else {
            anyhow::bail!("セッションが無い: {listener}");
        };
        session.muted = true;
        Ok(())
    }

    /// トランスポートごと畳む。**二重接続・BAN・shutdown のときだけ** (docs/protocol.md §0)。
    pub fn remove_session(&mut self, steam_id: &SteamId) {
        if let Some(mut session) = self.sessions.remove(steam_id) {
            session.rtc.disconnect();
            tracing::info!(steam_id = %steam_id, "session removed");
        }
    }

    // -- run loop の中身 ----------------------------------------------------

    /// 1 個の UDP データグラムを、受け取るべき `Rtc` へ渡す。
    /// どの `Rtc` 宛かは `Rtc::accepts` が判定する (これが 1 ソケット多重化の要)。
    fn handle_datagram(&mut self, now: Instant, buf: &[u8], source: SocketAddr) {
        let Ok(contents) = buf.try_into() else { return };
        let destination = self.destination_for(source);
        let input = Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source,
                destination,
                contents,
            },
        );

        let Some(session) = self.sessions.values_mut().find(|s| s.rtc.accepts(&input)) else {
            // ブラウザの最初の STUN が offer の受理より先に来ることがある。よくある
            tracing::trace!(?source, "どのセッションも受け取らない UDP");
            return;
        };
        if let Err(e) = session.rtc.handle_input(input) {
            tracing::warn!(steam_id = %session.steam_id, error = %e, "handle_input 失敗");
            session.rtc.disconnect();
        }
    }

    /// この相手から見たときの「こちらの host candidate」。
    ///
    /// str0m の ICE は、STUN が**登録済みの local candidate 宛に来たか**で受理を決める
    /// (`v.addr() == req.destination`)。ところが `0.0.0.0` に bind したソケットの
    /// `recv_from` は宛先 IP を教えてくれず、`local_addr()` はワイルドカードのまま。
    /// そのまま渡すと全部 "unknown interface" で捨てられ、DTLS がハンドシェイクに
    /// 到達しない。
    ///
    /// 1 ポートで済ませるという設計要件は崩したくないので、**経路表を引いて**
    /// 「この相手へ出ていくときの自分の IP」を求め、それを宛先とみなす。
    /// 相手の IP ごとに 1 回だけ引いて覚える。
    fn destination_for(&mut self, source: SocketAddr) -> SocketAddr {
        let port = self.socket.local_addr().map(|a| a.port()).unwrap_or(0);

        // candidate が 1 つしか無いなら引くまでもない
        if let [only] = self.candidates[..] {
            return only;
        }
        if let Some(d) = self.routes.get(&source.ip()) {
            return *d;
        }
        // 素性の知れない相手からの UDP で無限に太らせない
        if self.routes.len() > 1024 {
            self.routes.clear();
        }

        let picked = local_ip_toward(source)
            .map(|ip| SocketAddr::new(ip, port))
            .filter(|a| self.candidates.contains(a))
            .or_else(|| self.candidates.first().copied())
            .unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));

        self.routes.insert(source.ip(), picked);
        picked
    }

    /// 全セッションの出力を吐き切り、次に起こしてほしい時刻を返す。
    ///
    /// 音声の転送はここで起きる。**デコードはしない** — `MediaData` をそのまま
    /// 相手の writer へ流す。
    fn poll_all(&mut self, now: Instant, hub: &Hub) -> Instant {
        let socket = self.socket.clone();
        let mut next = now + MAX_TICK;
        // (話者, データ)。sessions を二重に借りられないので一度貯める
        let mut forward: Vec<(SteamId, MediaData)> = Vec::new();
        let mut dead: Vec<SteamId> = Vec::new();

        for session in self.sessions.values_mut() {
            loop {
                if !session.rtc.is_alive() {
                    dead.push(session.steam_id.clone());
                    break;
                }
                match session.rtc.poll_output() {
                    Ok(Output::Transmit(t)) => {
                        // 送れなければ落とす。RTP は再送しない
                        let _ = socket.try_send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Timeout(t)) => {
                        next = next.min(t);
                        break;
                    }
                    Ok(Output::Event(event)) => match event {
                        Event::MediaData(data) => {
                            // マイク以外から来たものは転送しない
                            if session.mic_mid == Some(data.mid) {
                                forward.push((session.steam_id.clone(), data));
                            }
                        }
                        Event::MediaAdded(added) => {
                            tracing::debug!(
                                steam_id = %session.steam_id,
                                mid = %added.mid,
                                dir = ?added.direction,
                                "media added"
                            );
                        }
                        Event::IceConnectionStateChange(state) => {
                            tracing::debug!(steam_id = %session.steam_id, ?state, "ice state");
                        }
                        _ => {}
                    },
                    Err(e) => {
                        tracing::warn!(steam_id = %session.steam_id, error = %e, "poll_output 失敗");
                        session.rtc.disconnect();
                        break;
                    }
                }
            }
        }

        for (speaker, data) in &forward {
            self.forward_to_listeners(speaker, data);
        }

        for id in dead {
            tracing::info!(steam_id = %id, "rtc が死んだので回収");
            self.sessions.remove(&id);
            // 切断は「即座に転送停止」(docs/protocol.md §0)。graph は 2 Hz でしか
            // 来ないので、次の SetSubscriptions を待たずにここで枠を解放する
            self.release_speaker(&id, hub);
        }

        next
    }

    /// `state.rs` の指令を捌く。返り値が要るものは `oneshot` で返す。
    fn handle_command(&mut self, cmd: SfuCommand, hub: &Hub) {
        match cmd {
            SfuCommand::AcceptOffer {
                steam_id,
                sdp,
                reply,
            } => {
                let _ = reply.send(self.accept_offer(&steam_id, &sdp));
            }
            SfuCommand::Ice {
                steam_id,
                candidate,
            } => {
                // ブラウザは {"candidate": "candidate:...", "sdpMid": ...} の形で送る。
                // 空文字は「もう無い」の合図なので黙って捨てる
                let Some(s) = candidate.get("candidate").and_then(|v| v.as_str()) else {
                    tracing::debug!(steam_id = %steam_id, "candidate フィールドが無い");
                    return;
                };
                if s.is_empty() {
                    return;
                }
                if let Err(e) = self.add_remote_candidate(&steam_id, s) {
                    tracing::debug!(steam_id = %steam_id, error = %e, "candidate を足せない");
                }
            }
            SfuCommand::SetSubscriptions {
                listener,
                speakers,
                reply,
            } => {
                // ★ `Peer` の送出は SFU が一手に引き受ける (state.rs の契約)。
                // `Disconnect` のように reply の無い指令でもスロットは動くので、
                // 呼び出し側に流させると、その経路で誰も送れなくなる
                let result = self
                    .set_subscriptions(&listener, &speakers)
                    .and_then(|msgs| {
                        hub.send_all(&listener, msgs)
                            .map_err(|e| anyhow::anyhow!("peer を送れない: {e}"))
                    });
                let _ = reply.send(result);
            }
            SfuCommand::MuteAll { listener } => {
                // 聞き手が居ないのは珍しくない (WS だけ張って offer 前など)
                if let Err(e) = self.mute_all(&listener) {
                    tracing::debug!(listener = %listener, error = %e, "mute できない");
                }
            }
            SfuCommand::Disconnect { steam_id, reason } => {
                self.remove_session(&steam_id);
                // 落ちた本人の声を聞いていた全員の枠も、その場で解放する
                self.release_speaker(&steam_id, hub);
                // **`Bye` を送って WS を閉じるのは受け手の責務** (state.rs の契約)。
                // `Peer` と同じく送出元を 1 箇所に寄せる
                hub.kick(&steam_id, reason);
            }
        }
    }

    /// 時間を進める。str0m は sans-I/O なので `Instant::now()` を自分では呼ばない。
    fn advance(&mut self, now: Instant) {
        for session in self.sessions.values_mut() {
            if session.rtc.is_alive() {
                let _ = session.rtc.handle_input(Input::Timeout(now));
            }
        }
    }

    /// 落ちた話者のスロットを、全聞き手からその場で解放する。
    ///
    /// 転送はスロットを引けなくなった時点で止まる。`Peer{id: null}` は
    /// PWA 側の表示を合わせるためのもので、安全性はサーバーが流さないことで既に満たされている。
    fn release_speaker(&mut self, speaker: &SteamId, hub: &Hub) {
        for session in self.sessions.values_mut() {
            let msgs = session.pool.release(speaker);
            if msgs.is_empty() {
                continue;
            }
            if let Err(e) = hub.send_all(&session.steam_id, msgs) {
                tracing::debug!(steam_id = %session.steam_id, error = %e, "peer を送れない");
            }
        }
    }

    /// 1 人の話者の音声を、その人にスロットを割り当てている聞き手全員へ配る。
    fn forward_to_listeners(&mut self, speaker: &SteamId, data: &MediaData) {
        for session in self.sessions.values_mut() {
            if &session.steam_id == speaker {
                continue; // 自分の声は返さない
            }
            if session.muted {
                continue; // ★ 失効の実体。購読があっても流さない
            }
            let Some(mid) = session.pool.mid_of(speaker) else {
                continue; // 可聴集合の外
            };
            let mid = Mid::from(mid);
            let Some(writer) = session.rtc.writer(mid) else {
                continue;
            };
            let Some(pt) = writer.match_params(data.params) else {
                continue;
            };
            if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
                tracing::warn!(steam_id = %session.steam_id, error = %e, "RTP の書き出しに失敗");
            }
        }
    }
}

/// その相手へ出ていくときの自分の IP。パケットは送らず、経路表を引くだけ。
fn local_ip_toward(target: SocketAddr) -> Option<IpAddr> {
    let bind = if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect(target).ok()?;
    Some(s.local_addr().ok()?.ip())
}

/// このホストの外向き IPv4。パケットは送らず、経路表を引くだけ。
fn primary_ipv4() -> Option<Ipv4Addr> {
    let s = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    s.connect(("8.8.8.8", 80)).ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) => Some(*a.ip()),
        SocketAddr::V6(_) => None,
    }
}

/// SFU のループ。**`Sfu` はここが単独で所有する** (Mutex で包まない)。
/// 指令は `rx` から受け、クライアントへ送るメッセージは `hub` 経由で出す。
///
/// **担当: #1-1。** シグネチャは親所有 (`state.rs` と対) なので変更しない。
///
/// 1 周でやること:
///   1. 全 `Rtc` の出力を吐き切る (UDP 送信 / RTP 転送) → 次に起きる時刻を得る
///   2. UDP の到着・指令・その時刻、のいずれか早いものを待つ
///   3. 時間を進める (str0m は自分では `Instant::now()` を呼ばない)
///
/// ICE candidate をこちらから送ることはない。host candidate は
/// `accept_offer` の時点で answer の SDP に載るため、trickle が要らない。
pub async fn run(
    udp_port: u16,
    mut rx: mpsc::Receiver<SfuCommand>,
    hub: Arc<Hub>,
) -> anyhow::Result<()> {
    let mut sfu = Sfu::new(udp_port)?;
    let socket = sfu.socket.clone();
    let mut buf = vec![0u8; 2000];

    loop {
        let next = sfu.poll_all(Instant::now(), &hub);
        let wait = next
            .saturating_duration_since(Instant::now())
            .clamp(Duration::from_millis(1), MAX_TICK);

        tokio::select! {
            r = socket.recv_from(&mut buf) => {
                match r {
                    Ok((n, source)) => sfu.handle_datagram(Instant::now(), &buf[..n], source),
                    Err(e) => tracing::warn!(error = %e, "UDP の読みに失敗"),
                }
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else {
                    // 送り手が全員居なくなった = shutdown
                    tracing::info!("sfu への指令チャネルが閉じた。ループを畳む");
                    return Ok(());
                };
                sfu.handle_command(cmd, &hub);
            }
            _ = tokio::time::sleep(wait) => {}
        }

        sfu.advance(Instant::now());
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<SteamId> {
        // 近い順に並んでいる前提。添字が小さいほど近い
        (0..n).map(|i| format!("7656119800000{i:04}")).collect()
    }

    fn pool(n: usize) -> SlotPool {
        SlotPool::new((0..n).map(|i| i.to_string()))
    }

    /// `Peer` を (mid, id) に潰して比較しやすくする。
    fn peers(msgs: &[ServerMsg]) -> Vec<(String, Option<String>)> {
        msgs.iter()
            .map(|m| match m {
                ServerMsg::Peer { mid, id } => (mid.clone(), id.clone()),
                other => panic!("Peer 以外が出た: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn プールは_slots_本までしか張らない() {
        assert_eq!(pool(SLOTS + 8).len(), SLOTS);
        assert_eq!(pool(4).len(), 4);
    }

    #[test]
    fn 十六人までは全員にスロットが付く() {
        let mut p = pool(SLOTS);
        let speakers = ids(SLOTS);
        let msgs = p.assign(&speakers);

        assert_eq!(msgs.len(), SLOTS, "16 人ぶんの Peer が出る");
        for s in &speakers {
            assert!(p.mid_of(s).is_some(), "{s} にスロットが付いていない");
        }
        assert!(p.slots().iter().all(|s| s.id.is_some()), "空きが残っている");
    }

    #[test]
    fn 十七人目は距離の近い順に弾かれる() {
        let mut p = pool(SLOTS);
        let speakers = ids(SLOTS + 1);
        let msgs = p.assign(&speakers);

        assert_eq!(msgs.len(), SLOTS);
        for near in &speakers[..SLOTS] {
            assert!(p.mid_of(near).is_some(), "近い {near} が弾かれた");
        }
        let farthest = &speakers[SLOTS];
        assert!(
            p.mid_of(farthest).is_none(),
            "遠い {farthest} が入ってしまった"
        );
    }

    #[test]
    fn 範囲外に出た相手のスロットが解放される() {
        let mut p = pool(SLOTS);
        let speakers = ids(3);
        p.assign(&speakers);
        let mid_of_1 = p.mid_of(&speakers[1]).unwrap().to_string();

        // 真ん中の 1 人だけが範囲外へ
        let msgs = p.assign(&[speakers[0].clone(), speakers[2].clone()]);

        assert_eq!(
            peers(&msgs),
            vec![(mid_of_1.clone(), None)],
            "解放は id: None の Peer 1 通だけ"
        );
        assert!(p.mid_of(&speakers[1]).is_none());
        // 残る 2 人は動かない
        assert!(p.mid_of(&speakers[0]).is_some());
        assert!(p.mid_of(&speakers[2]).is_some());
    }

    #[test]
    fn 連続で呼んでも割り当て済みの相手の_mid_は変わらない() {
        let mut p = pool(SLOTS);
        let speakers = ids(5);
        p.assign(&speakers);
        let before: Vec<String> = speakers
            .iter()
            .map(|s| p.mid_of(s).unwrap().to_string())
            .collect();

        // 同じ集合をもう一度。何も起きてはいけない
        let msgs = p.assign(&speakers);
        assert!(msgs.is_empty(), "変化が無いのに Peer が出た: {msgs:?}");

        // 順番を入れ替えても mid は動かない (距離が入れ替わっただけで音は飛ばない)
        let mut shuffled = speakers.clone();
        shuffled.reverse();
        let msgs = p.assign(&shuffled);
        assert!(msgs.is_empty(), "並べ替えだけで Peer が出た: {msgs:?}");

        let after: Vec<String> = speakers
            .iter()
            .map(|s| p.mid_of(s).unwrap().to_string())
            .collect();
        assert_eq!(before, after, "mid が動いた");
    }

    #[test]
    fn 出入りが同時に起きても空いた枠を再利用する() {
        let mut p = pool(2);
        let a = "a".to_string();
        let b = "b".to_string();
        let c = "c".to_string();

        p.assign(&[a.clone(), b.clone()]);
        let mid_b = p.mid_of(&b).unwrap().to_string();

        // b が出て c が入る
        let msgs = p.assign(&[a.clone(), c.clone()]);
        assert_eq!(
            peers(&msgs),
            vec![(mid_b.clone(), None), (mid_b.clone(), Some(c.clone()))],
            "解放してから同じ枠へ割り当てる"
        );
        assert_eq!(p.mid_of(&a).unwrap(), "0", "a の mid は動かない");
        assert_eq!(p.mid_of(&c).unwrap(), mid_b);
    }

    #[test]
    fn 満員のときに近い相手が来たら遠い相手を追い出す() {
        let mut p = pool(2);
        let far = "far".to_string();
        let near = "near".to_string();
        let newcomer = "newcomer".to_string();

        p.assign(&[near.clone(), far.clone()]);
        // newcomer が near の次に近い位置へ割り込み、far が 3 番目に落ちる
        let msgs = p.assign(&[near.clone(), newcomer.clone(), far.clone()]);

        assert!(p.mid_of(&near).is_some());
        assert!(p.mid_of(&newcomer).is_some());
        assert!(p.mid_of(&far).is_none());
        assert_eq!(msgs.len(), 2, "far の解放と newcomer の割り当てだけ");
    }

    #[test]
    fn 重複した話者は一度しか入らない() {
        let mut p = pool(SLOTS);
        let a = "a".to_string();
        let msgs = p.assign(&[a.clone(), a.clone(), a.clone()]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(p.slots().iter().filter(|s| s.id.is_some()).count(), 1);
    }

    #[test]
    fn 落ちた話者のスロットだけが解放される() {
        let mut p = pool(SLOTS);
        let speakers = ids(3);
        p.assign(&speakers);
        let mid = p.mid_of(&speakers[1]).unwrap().to_string();

        let msgs = p.release(&speakers[1]);
        assert_eq!(peers(&msgs), vec![(mid, None)]);
        assert!(p.mid_of(&speakers[1]).is_none());
        assert!(p.mid_of(&speakers[0]).is_some());
        assert!(p.mid_of(&speakers[2]).is_some());

        // 二度目は何も起きない
        assert!(p.release(&speakers[1]).is_empty());
    }

    #[test]
    fn 掴んでいない話者の解放は何もしない() {
        let mut p = pool(SLOTS);
        p.assign(&ids(2));
        assert!(p.release("よそ者").is_empty());
    }

    #[test]
    fn 空のプールでは何も割り当てない() {
        let mut p = SlotPool::default();
        assert!(p.assign(&ids(3)).is_empty());
    }

    // -- offer の解釈 -------------------------------------------------------

    const OFFER: &str = "\
v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:0\r\n\
a=sendonly\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:1\r\n\
a=recvonly\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:2\r\n\
a=recvonly\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
a=mid:3\r\n";

    #[test]
    fn offer_からマイクと受信スロットを振り分ける() {
        let m = parse_audio_mlines(OFFER);
        assert_eq!(m.mic.as_deref(), Some("0"));
        assert_eq!(m.slots, vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn 方向属性が無い_m_line_は_sendrecv_とみなす() {
        let sdp = "m=audio 9 x 111\r\na=mid:0\r\nm=audio 9 x 111\r\na=mid:1\r\n";
        let m = parse_audio_mlines(sdp);
        // 最初の送れる m-line がマイク、残りの受け取れる m-line がスロット
        assert_eq!(m.mic.as_deref(), Some("0"));
        assert_eq!(m.slots, vec!["1".to_string()]);
    }

    #[test]
    fn 音声以外の_m_line_は無視する() {
        let sdp = "m=video 9 x 96\r\na=mid:0\r\na=recvonly\r\nm=application 9 x c\r\na=mid:1\r\n";
        let m = parse_audio_mlines(sdp);
        assert_eq!(m.mic, None);
        assert!(m.slots.is_empty());
    }

    // -- Sfu 越し -----------------------------------------------------------

    /// str0m を通さずにスロット表だけを持つセッションを差し込む。
    fn insert_fake_session(sfu: &mut Sfu, steam_id: &str, slots: usize) {
        sfu.sessions.insert(
            steam_id.to_string(),
            Session {
                steam_id: steam_id.to_string(),
                rtc: Rtc::new(Instant::now()),
                pool: pool(slots),
                mic_mid: Some(Mid::from("0")),
                muted: false,
            },
        );
    }

    #[tokio::test]
    async fn set_subscriptions_は再ネゴシエーションを起こさず_peer_だけ返す() {
        let mut sfu = Sfu::new(0).unwrap();
        let listener = "listener".to_string();
        insert_fake_session(&mut sfu, &listener, SLOTS);

        let speakers = ids(SLOTS + 1);
        let msgs = sfu.set_subscriptions(&listener, &speakers).unwrap();
        assert_eq!(msgs.len(), SLOTS);

        let mids_before: Vec<String> = speakers[..SLOTS]
            .iter()
            .map(|s| {
                sfu.session(&listener)
                    .unwrap()
                    .slots()
                    .mid_of(s)
                    .unwrap()
                    .to_string()
            })
            .collect();

        // 2 回目。集合が同じなら 1 通も出ない
        assert!(
            sfu.set_subscriptions(&listener, &speakers)
                .unwrap()
                .is_empty()
        );

        let mids_after: Vec<String> = speakers[..SLOTS]
            .iter()
            .map(|s| {
                sfu.session(&listener)
                    .unwrap()
                    .slots()
                    .mid_of(s)
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(mids_before, mids_after);
    }

    #[tokio::test]
    async fn mute_all_は切断せずスロットも保つ() {
        let mut sfu = Sfu::new(0).unwrap();
        let listener = "listener".to_string();
        insert_fake_session(&mut sfu, &listener, SLOTS);
        let speakers = ids(3);
        sfu.set_subscriptions(&listener, &speakers).unwrap();

        sfu.mute_all(&listener).unwrap();

        let session = sfu.session(&listener).unwrap();
        assert!(session.is_muted(), "転送が止まっていない");
        assert!(sfu.has_session(&listener), "切断してしまった");
        for s in &speakers {
            assert!(
                session.slots().mid_of(s).is_some(),
                "mute でスロットが解放された ({s})"
            );
        }

        // 再開しても Peer は飛ばない = PWA 側のスロットは動かない
        let msgs = sfu.set_subscriptions(&listener, &speakers).unwrap();
        assert!(msgs.is_empty(), "再開で Peer が出た: {msgs:?}");
        assert!(!sfu.session(&listener).unwrap().is_muted());
    }

    #[tokio::test]
    async fn 知らない聞き手への操作はエラーになる() {
        let mut sfu = Sfu::new(0).unwrap();
        assert!(sfu.set_subscriptions(&"nobody".to_string(), &[]).is_err());
        assert!(sfu.mute_all(&"nobody".to_string()).is_err());
    }

    #[tokio::test]
    async fn 受信用_m_line_が無い_offer_は断る() {
        let mut sfu = Sfu::new(0).unwrap();
        let sdp = "m=audio 9 x 111\r\na=mid:0\r\na=sendonly\r\n";
        let err = sfu.accept_offer(&"a".to_string(), sdp).unwrap_err();
        assert!(err.to_string().contains("recvonly"), "{err}");
    }
}
