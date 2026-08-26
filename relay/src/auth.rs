//! 認証と認可。**担当: #1-3**
//!
//! 3 層 (docs/protocol.md):
//!   認証   = Steam OpenID → SteamID64
//!   認可   = roster に載っているか (+ 任意の静的 whitelist)
//!   配信範囲 = graph が決める
//!
//! **画面に秘密を出さない。** ペアリングコードもトークン付きリンクも使わない。
//! トークンは HttpOnly Cookie で渡す。
//!
//! Rust は配信人口が多い。画面にペアリングコードを出す設計は、配信された瞬間に
//! 視聴者へ「その配信者に聞こえる範囲の情報」を渡してしまい、実質 ESP の配布になる。
//! 画面に出てよいのは**非秘密の URL だけ**。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::proto::{HMAC_SKEW_S, ServerId, SteamId};
use crate::state::AppState;

// ---- 定数 ----

/// セッショントークンを載せる Cookie 名。
pub const SESSION_COOKIE: &str = "pv_session";
/// セッションの寿命 (秒)。
pub const SESSION_TTL_S: i64 = 12 * 60 * 60;
/// Valve の OpenID 2.0 エンドポイント。
pub const STEAM_OPENID_ENDPOINT: &str = "https://steamcommunity.com/openid/login";
/// `openid.claimed_id` の接頭辞。
const STEAM_CLAIMED_ID_PREFIX: &str = "https://steamcommunity.com/openid/id/";

// ---- 時刻 (テストで進められるように注入可能にする) ----

/// 現在時刻の供給元。TTL / skew のテストで `sleep` を使わないための注入点。
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// unix 秒。
    fn now_unix(&self) -> i64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        now_unix()
    }
}

/// テスト用の手回し時計。
#[derive(Debug)]
pub struct TestClock(AtomicI64);

impl TestClock {
    pub fn new(at: i64) -> Self {
        Self(AtomicI64::new(at))
    }
    /// 秒だけ進める。
    pub fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
    pub fn set(&self, at: i64) {
        self.0.store(at, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---- HMAC 検証 (プラグイン → リレー, docs/protocol.md §1) ----

type HmacSha256 = Hmac<Sha256>;

/// `hex(hmac_sha256(secret, timestamp + "." + body))` を作る。
///
/// 署名対象は `timestamp` と body の連結。timestamp を署名に含めることで、
/// 「署名は正しいが時刻ヘッダだけ差し替える」リプレイを塞ぐ。
pub fn sign(secret: &str, timestamp: &str, body: &[u8]) -> String {
    hex::encode(sign_raw(secret, timestamp, body))
}

fn sign_raw(secret: &str, timestamp: &str, body: &[u8]) -> Vec<u8> {
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 は任意長の鍵を受け付ける");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    mac.finalize().into_bytes().to_vec()
}

/// プラグインからの push を検証する。**3 点すべてを満たさなければ 401。**
///   1. 署名一致 (定数時間比較)
///   2. |now - timestamp| <= HMAC_SKEW_S
///   3. seq が同一 server_id について単調増加
///
/// ここで見るのは 1 と 2 (body だけで完結する部分)。3 は `seq` が本文中にあり
/// 状態を跨ぐので [`SeqGuard::accept`] が持つ。route ハンドラが両方を呼ぶ。
///
/// **3 点全部が要る理由**: 署名だけならリプレイが通る。時刻だけなら偽造が通る。
/// seq だけなら古いリクエストの再送が通る。
pub fn verify_hmac(
    secret: &str,
    timestamp: &str,
    signature: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    verify_hmac_at(secret, timestamp, signature, body, now_unix())
}

/// [`verify_hmac`] の時刻注入版。テストは `sleep` せずにここを使う。
pub fn verify_hmac_at(
    secret: &str,
    timestamp: &str,
    signature: &str,
    body: &[u8],
    now: i64,
) -> anyhow::Result<()> {
    // 2. 時刻ずれ
    let ts: i64 = timestamp
        .trim()
        .parse()
        .with_context(|| format!("X-PV-Timestamp が数値でない: {timestamp:?}"))?;
    let skew = now.saturating_sub(ts).abs();
    if skew > HMAC_SKEW_S {
        bail!("timestamp のずれが {skew}s で HMAC_SKEW_S={HMAC_SKEW_S} を超える");
    }

    // 1. 署名一致 (定数時間)
    let expected = sign_raw(secret, timestamp, body);
    let got = hex::decode(signature.trim()).map_err(|_| anyhow!("署名が hex でない"))?;
    if got.len() != expected.len() {
        bail!("署名長が違う");
    }
    if !bool::from(got.ct_eq(&expected)) {
        bail!("署名が一致しない");
    }
    Ok(())
}

/// `seq` の単調増加を `server_id` ごとに見張る。巻き戻り (と同値) は破棄。
#[derive(Debug, Default)]
pub struct SeqGuard {
    last: Mutex<HashMap<ServerId, u64>>,
}

impl SeqGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 受理してよければ true を返し、内部の最終 seq を更新する。
    /// 既に見た seq 以下 (= 巻き戻り / 再送) なら false で、状態は変えない。
    ///
    /// `stream` はエンドポイント名。`docs/protocol.md` §1 の例では roster が
    /// `seq: 8412`、graph が `seq: 90211` と**別々に採番されている**ので、
    /// 1 本の counter で見張ると片方が常に巻き戻り扱いになる。
    pub fn accept_stream(&self, stream: &str, server_id: &str, seq: u64) -> bool {
        self.accept(&format!("{stream}:{server_id}"), seq)
    }

    /// 受理してよければ true を返し、内部の最終 seq を更新する。
    /// 既に見た seq 以下 (= 巻き戻り / 再送) なら false で、状態は変えない。
    pub fn accept(&self, server_id: &str, seq: u64) -> bool {
        let mut last = self.last.lock().expect("SeqGuard poisoned");
        match last.get(server_id) {
            Some(&prev) if seq <= prev => false,
            _ => {
                last.insert(server_id.to_string(), seq);
                true
            }
        }
    }

    /// いま覚えている最終 seq。
    pub fn last_seq(&self, server_id: &str) -> Option<u64> {
        self.last
            .lock()
            .expect("SeqGuard poisoned")
            .get(server_id)
            .copied()
    }
}

// ---- Steam OpenID 2.0 ----
//
// 検証部分は Valve への実通信を含むので、**純関数に切り出して**テストする。
// 通信するのは `verify_steam_openid` だけ。

/// `GET /auth/steam` のリダイレクト先。realm と return_to 以外に秘密は無い。
pub fn steam_login_url(realm: &str, return_to: &str) -> String {
    let q = form_urlencoded::Serializer::new(String::new())
        .append_pair("openid.ns", "http://specs.openid.net/auth/2.0")
        .append_pair("openid.mode", "checkid_setup")
        .append_pair("openid.return_to", return_to)
        .append_pair("openid.realm", realm)
        .append_pair(
            "openid.identity",
            "http://specs.openid.net/auth/2.0/identifier_select",
        )
        .append_pair(
            "openid.claimed_id",
            "http://specs.openid.net/auth/2.0/identifier_select",
        )
        .finish();
    format!("{STEAM_OPENID_ENDPOINT}?{q}")
}

/// 戻ってきた query を `openid.mode=check_authentication` に差し替えて、
/// **Valve へ投げ直す**ための body を組む。
///
/// 自前で署名を検証しない (= 共有鍵を持たない) ので、判定は必ず Valve に委ねる。
pub fn build_check_authentication_body(query: &str) -> anyhow::Result<String> {
    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    let mode = pairs
        .iter()
        .find(|(k, _)| k == "openid.mode")
        .map(|(_, v)| v.as_str())
        .ok_or_else(|| anyhow!("openid.mode が無い"))?;
    if mode != "id_res" {
        bail!("openid.mode が id_res でない: {mode:?}");
    }

    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in &pairs {
        // openid.* 以外は Valve へ送り返さない (こちらが足した戻り先パラメータ等)
        if !k.starts_with("openid.") {
            continue;
        }
        if k == "openid.mode" {
            ser.append_pair(k, "check_authentication");
        } else {
            ser.append_pair(k, v);
        }
    }
    Ok(ser.finish())
}

/// Valve の Key-Value Form レスポンスを読む。`is_valid:true` だけを真とする。
pub fn parse_check_authentication_response(body: &str) -> bool {
    body.lines()
        .filter_map(|l| l.split_once(':'))
        .any(|(k, v)| k.trim() == "is_valid" && v.trim() == "true")
}

/// `openid.claimed_id` から SteamID64 を取り出す。
///
/// 接頭辞が Valve のものであること、残りが 17 桁の数字であることまで見る。
pub fn extract_steam_id(claimed_id: &str) -> anyhow::Result<SteamId> {
    let id = claimed_id
        .strip_prefix(STEAM_CLAIMED_ID_PREFIX)
        .ok_or_else(|| anyhow!("claimed_id が Steam のものでない: {claimed_id:?}"))?;
    if id.len() != 17 || !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!("SteamID64 の形をしていない: {id:?}");
    }
    Ok(id.to_string())
}

/// **これが検証の純関数**。`query` は `/auth/steam/return` に来た生のクエリ、
/// `valve_response` は check_authentication に対する Valve の生レスポンス。
/// 固定文字列を食わせてテストできる (Valve への実通信は `verify_steam_openid` だけ)。
///
/// `expected_return_to` を渡すと、他サイト向けに発行された正しい assertion を
/// こちらへ流し込むリプレイを塞ぐ。
pub fn verify_openid_response(
    query: &str,
    valve_response: &str,
    expected_return_to: Option<&str>,
) -> anyhow::Result<SteamId> {
    // mode が id_res であることはここで弾かれる
    let _ = build_check_authentication_body(query)?;

    let pairs: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    // op_endpoint を見ておく。別の OP に検証させて成りすます経路を塞ぐ
    match pairs.get("openid.op_endpoint") {
        Some(ep) if ep == STEAM_OPENID_ENDPOINT => {}
        Some(ep) => bail!("op_endpoint が Steam でない: {ep:?}"),
        None => bail!("op_endpoint が無い"),
    }

    // Valve が署名したのは `openid.signed` に挙がったフィールドだけ。
    // claimed_id が署名対象に無ければ、署名を通したまま別人を名乗れてしまう
    let signed = pairs
        .get("openid.signed")
        .ok_or_else(|| anyhow!("openid.signed が無い"))?;
    let signed: HashSet<&str> = signed.split(',').map(|f| f.trim()).collect();
    for required in ["claimed_id", "identity", "op_endpoint", "return_to"] {
        if !signed.contains(required) {
            bail!("openid.signed に {required} が入っていない");
        }
    }

    // 他サイト向けの assertion をこちらへ流し込むリプレイを塞ぐ
    if let Some(expected) = expected_return_to {
        match pairs.get("openid.return_to") {
            Some(rt) if rt == expected => {}
            Some(rt) => bail!("return_to がこのリレー宛でない: {rt:?}"),
            None => bail!("return_to が無い"),
        }
    }

    if !parse_check_authentication_response(valve_response) {
        bail!("Valve が is_valid:true を返さなかった");
    }

    // identity と claimed_id が食い違っていたら信用しない
    let claimed = pairs
        .get("openid.claimed_id")
        .ok_or_else(|| anyhow!("claimed_id が無い"))?;
    let identity = pairs
        .get("openid.identity")
        .ok_or_else(|| anyhow!("identity が無い"))?;
    if claimed != identity {
        bail!("claimed_id と identity が食い違う");
    }
    extract_steam_id(claimed)
}

/// Steam OpenID の戻りを検証して SteamID64 を得る。
///
/// **`openid.mode=check_authentication` で Valve に投げ直して**判定させる。
pub async fn verify_steam_openid(query: &str) -> anyhow::Result<SteamId> {
    // return_to は realm を知る呼び出し側 (`steam_return`) が渡す。
    // ここは realm を持たないので、リプレイ検査を省いた最小形。
    verify_steam_openid_for(query, None).await
}

/// [`verify_steam_openid`] の return_to 明示版。
pub async fn verify_steam_openid_for(
    query: &str,
    expected_return_to: Option<&str>,
) -> anyhow::Result<SteamId> {
    let body = build_check_authentication_body(query)?;
    let resp = reqwest::Client::new()
        .post(STEAM_OPENID_ENDPOINT)
        .header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .body(body)
        .send()
        .await
        .context("Valve への check_authentication が失敗した")?
        .text()
        .await
        .context("Valve のレスポンスが読めない")?;
    verify_openid_response(query, &resp, expected_return_to)
}

// ---- セッション ----

#[derive(Debug, Clone)]
struct SessionEntry {
    steam_id: SteamId,
    expires_at: i64,
}

/// トークン → SteamID64。
#[derive(Debug)]
pub struct Sessions {
    clock: Arc<dyn Clock>,
    inner: Mutex<HashMap<String, SessionEntry>>,
}

impl Sessions {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 新しいトークンを発行する。**画面には出さない** — Cookie でしか渡らない。
    pub fn issue(&self, steam_id: SteamId) -> String {
        let token = random_token();
        let expires_at = self.clock.now_unix() + SESSION_TTL_S;
        self.inner.lock().expect("Sessions poisoned").insert(
            token.clone(),
            SessionEntry {
                steam_id,
                expires_at,
            },
        );
        token
    }

    /// トークンを引く。期限切れは None。
    pub fn lookup(&self, token: &str) -> Option<SteamId> {
        let now = self.clock.now_unix();
        let mut inner = self.inner.lock().expect("Sessions poisoned");
        match inner.get(token) {
            Some(e) if e.expires_at > now => Some(e.steam_id.clone()),
            Some(_) => {
                inner.remove(token);
                None
            }
            None => None,
        }
    }

    pub fn revoke(&self, token: &str) {
        self.inner.lock().expect("Sessions poisoned").remove(token);
    }
}

fn random_token() -> String {
    use rand::Rng;
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// `Cookie:` ヘッダから 1 つ取り出す。
pub fn cookie_value<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    cookie_header.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// `Set-Cookie` の中身を組む。**HttpOnly + Secure + SameSite=Lax。**
pub fn session_cookie(token: &str, secure: bool) -> String {
    let mut c = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL_S}"
    );
    if secure {
        c.push_str("; Secure");
    }
    c
}

// ---- プロセス共有のセッション表 ----
//
// `require_session` / `verify_hmac` は `AppState` を取らない自由関数として
// 親が宣言している (`/ws` は #1-1 が Cookie だけを持って呼ぶ)。
// セッション表と seq の記憶はプロセス全体で 1 つなので、ここに置く。

static SESSIONS: LazyLock<Sessions> = LazyLock::new(|| Sessions::new(Arc::new(SystemClock)));
static SEQ: LazyLock<SeqGuard> = LazyLock::new(SeqGuard::new);

/// プロセス共有のセッション表。
pub fn sessions() -> &'static Sessions {
    &SESSIONS
}

/// プロセス共有の seq 見張り。`server_id` × stream ごとに単調増加を要求する。
pub fn seq_guard() -> &'static SeqGuard {
    &SEQ
}

/// Cookie のトークンからセッションを引く。
pub fn require_session(cookie: &str) -> anyhow::Result<SteamId> {
    let token = cookie_value(cookie, SESSION_COOKIE)
        .ok_or_else(|| anyhow!("{SESSION_COOKIE} Cookie が無い"))?;
    SESSIONS
        .lookup(token)
        .ok_or_else(|| anyhow!("セッションが無効か期限切れ"))
}

/// HMAC の 1・2 点目 (署名一致 / 時刻ずれ) をヘッダから見る。
/// 3 点目 (seq) は本文を読んでから [`SeqGuard::accept_stream`]。
pub fn verify_headers(secret: &str, headers: &HeaderMap, body: &[u8]) -> anyhow::Result<()> {
    let ts = headers
        .get("X-PV-Timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("X-PV-Timestamp が無い"))?;
    let sig = headers
        .get("X-PV-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("X-PV-Signature が無い"))?;
    verify_hmac(secret, ts, sig, body)
}

// ---- route ----

/// `https://<domain>`。OpenID の realm。
fn realm(st: &AppState) -> String {
    let domain = st.cfg.domain.trim_end_matches('/');
    if domain.starts_with("http://") || domain.starts_with("https://") {
        domain.to_string()
    } else {
        format!("https://{domain}")
    }
}

fn return_to(st: &AppState) -> String {
    format!("{}/auth/steam/return", realm(st))
}

/// Cookie に `Secure` を付けるか。TLS の無いローカル検証でだけ落とす。
fn secure_cookie(st: &AppState) -> bool {
    !realm(st).starts_with("http://")
}

async fn steam_login(State(st): State<AppState>) -> Response {
    // 画面に出るのはこの URL だけ。**秘密は含まれない** (配信対策)
    redirect(&steam_login_url(&realm(&st), &return_to(&st)), None)
}

async fn steam_return(State(st): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let Some(query) = query else {
        return (StatusCode::BAD_REQUEST, "openid のクエリが無い").into_response();
    };
    match verify_steam_openid_for(&query, Some(&return_to(&st))).await {
        Ok(steam_id) => {
            let token = SESSIONS.issue(steam_id.clone());
            tracing::info!(%steam_id, "steam openid ok");
            redirect("/", Some(session_cookie(&token, secure_cookie(&st))))
        }
        Err(e) => {
            tracing::warn!(error = %e, "steam openid rejected");
            (StatusCode::UNAUTHORIZED, "Steam の検証に失敗しました").into_response()
        }
    }
}

fn redirect(location: &str, set_cookie: Option<String>) -> Response {
    let mut res = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, location);
    if let Some(c) = set_cookie {
        res = res.header(header::SET_COOKIE, c);
    }
    res.body(axum::body::Body::empty())
        .expect("固定ヘッダなので組み立ては失敗しない")
        .into_response()
}

/// このモジュールが提供する route。**#1-3 が中身を実装する。**
/// `web.rs` (#1-1) が `router()` の中で merge する。空でも #1-1 は独立にビルドできる。
///
/// パスは絶対 (`/auth/...`) なので `nest` ではなく `merge` でよい。
pub fn routes(st: AppState) -> Router {
    Router::new()
        .route("/auth/steam", get(steam_login))
        .route("/auth/steam/return", get(steam_return))
        .with_state(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cr3t-shared-with-plugin";
    const BODY: &[u8] = br#"{"server_id":"main","seq":1,"ts":1756180000,"eligible":[]}"#;

    // ---- HMAC: 正しい署名で通る ----
    #[test]
    fn hmac_accepts_valid_signature() {
        let now = 1_756_180_000i64;
        let ts = now.to_string();
        let sig = sign(SECRET, &ts, BODY);
        verify_hmac_at(SECRET, &ts, &sig, BODY, now).expect("正しい署名は通る");
    }

    // ---- HMAC: 署名 1 バイト違いで 401 ----
    #[test]
    fn hmac_rejects_one_byte_off() {
        let now = 1_756_180_000i64;
        let ts = now.to_string();
        let sig = sign(SECRET, &ts, BODY);

        // 先頭 hex 1 文字 (= 4 bit) を別の値に差し替える
        let mut bad = sig.clone();
        let first = &sig[0..1];
        bad.replace_range(0..1, if first == "a" { "b" } else { "a" });
        assert_ne!(bad, sig);
        assert!(verify_hmac_at(SECRET, &ts, &bad, BODY, now).is_err());

        // 生バイト 1 個を反転しても落ちる
        let mut raw = sign_raw(SECRET, &ts, BODY);
        raw[31] ^= 0x01;
        assert!(verify_hmac_at(SECRET, &ts, &hex::encode(&raw), BODY, now).is_err());

        // body が 1 バイト違っても落ちる
        let mut body2 = BODY.to_vec();
        body2[10] ^= 0x01;
        assert!(verify_hmac_at(SECRET, &ts, &sig, &body2, now).is_err());
    }

    // ---- HMAC: timestamp が 31 秒ずれて 401 ----
    #[test]
    fn hmac_rejects_stale_timestamp() {
        let ts_val = 1_756_180_000i64;
        let ts = ts_val.to_string();
        let sig = sign(SECRET, &ts, BODY);

        // ちょうど HMAC_SKEW_S (=30) はまだ通る
        assert_eq!(HMAC_SKEW_S, 30);
        verify_hmac_at(SECRET, &ts, &sig, BODY, ts_val + 30).expect("30s は許容内");

        // 31 秒古い → 401
        assert!(verify_hmac_at(SECRET, &ts, &sig, BODY, ts_val + 31).is_err());
        // 31 秒未来 → 401 (未来方向も同じく弾く)
        assert!(verify_hmac_at(SECRET, &ts, &sig, BODY, ts_val - 31).is_err());
    }

    #[test]
    fn hmac_rejects_malformed_headers() {
        let now = 1_756_180_000i64;
        let ts = now.to_string();
        assert!(verify_hmac_at(SECRET, "not-a-number", "00", BODY, now).is_err());
        assert!(verify_hmac_at(SECRET, &ts, "zz-not-hex", BODY, now).is_err());
        // 長さ違いも弾く (短い前方一致で通してしまわないこと)
        let sig = sign(SECRET, &ts, BODY);
        assert!(verify_hmac_at(SECRET, &ts, &sig[..40], BODY, now).is_err());
    }

    // ---- HMAC: seq が巻き戻って破棄される ----
    #[test]
    fn seq_must_increase_per_server() {
        let g = SeqGuard::new();
        assert!(g.accept("main", 10));
        assert!(g.accept("main", 11));
        // 巻き戻り → 破棄
        assert!(!g.accept("main", 10));
        // 同値の再送 → 破棄
        assert!(!g.accept("main", 11));
        // 破棄しても最終 seq は動かない
        assert_eq!(g.last_seq("main"), Some(11));
        // 進めば通る
        assert!(g.accept("main", 12));
        // server_id ごとに独立
        assert!(g.accept("eu", 1));
        assert_eq!(g.last_seq("eu"), Some(1));
    }

    // ---- Steam OpenID: 検証部分は純関数として固定文字列で試す ----

    fn ok_query() -> String {
        form_urlencoded::Serializer::new(String::new())
            .append_pair("openid.ns", "http://specs.openid.net/auth/2.0")
            .append_pair("openid.mode", "id_res")
            .append_pair("openid.op_endpoint", STEAM_OPENID_ENDPOINT)
            .append_pair(
                "openid.claimed_id",
                "https://steamcommunity.com/openid/id/76561198000000001",
            )
            .append_pair(
                "openid.identity",
                "https://steamcommunity.com/openid/id/76561198000000001",
            )
            .append_pair(
                "openid.return_to",
                "https://vc.example.com/auth/steam/return",
            )
            .append_pair("openid.sig", "Zm9vYmFy")
            .append_pair(
                "openid.signed",
                "signed,op_endpoint,claimed_id,identity,return_to",
            )
            .finish()
    }

    #[test]
    fn openid_valid_response_yields_steam_id() {
        let id = verify_openid_response(
            &ok_query(),
            "ns:http://specs.openid.net/auth/2.0\nis_valid:true\n",
            None,
        )
        .expect("is_valid:true なら通る");
        assert_eq!(id, "76561198000000001");
    }

    #[test]
    fn openid_rejects_is_valid_false() {
        let e = verify_openid_response(
            &ok_query(),
            "ns:http://specs.openid.net/auth/2.0\nis_valid:false\n",
            None,
        );
        assert!(e.is_err());
    }

    #[test]
    fn openid_rejects_foreign_op_endpoint() {
        let q = ok_query().replace(
            "openid.op_endpoint=https%3A%2F%2Fsteamcommunity.com%2Fopenid%2Flogin",
            "openid.op_endpoint=https%3A%2F%2Fevil.example.com%2Fopenid%2Flogin",
        );
        assert!(verify_openid_response(&q, "is_valid:true\n", None).is_err());
    }

    #[test]
    fn openid_rejects_non_id_res_mode() {
        let q = ok_query().replace("openid.mode=id_res", "openid.mode=cancel");
        assert!(verify_openid_response(&q, "is_valid:true\n", None).is_err());
    }

    #[test]
    fn openid_rejects_unsigned_claimed_id() {
        // claimed_id が署名対象に無ければ、Valve の is_valid:true でも信用しない
        let q = ok_query().replace(
            "openid.signed=signed%2Cop_endpoint%2Cclaimed_id%2Cidentity%2Creturn_to",
            "openid.signed=signed%2Cop_endpoint%2Cidentity%2Creturn_to",
        );
        assert!(verify_openid_response(&q, "is_valid:true\n", None).is_err());
    }

    #[test]
    fn openid_rejects_return_to_for_another_site() {
        let q = ok_query();
        // 自分宛なら通る
        assert!(
            verify_openid_response(
                &q,
                "is_valid:true\n",
                Some("https://vc.example.com/auth/steam/return")
            )
            .is_ok()
        );
        // 他サイト向けの assertion を流し込むリプレイは弾く
        assert!(
            verify_openid_response(
                &q,
                "is_valid:true\n",
                Some("https://other.example.com/auth/steam/return")
            )
            .is_err()
        );
    }

    #[test]
    fn openid_rejects_identity_claimed_id_mismatch() {
        let q = ok_query().replace(
            "openid.identity=https%3A%2F%2Fsteamcommunity.com%2Fopenid%2Fid%2F76561198000000001",
            "openid.identity=https%3A%2F%2Fsteamcommunity.com%2Fopenid%2Fid%2F76561198000000042",
        );
        assert!(verify_openid_response(&q, "is_valid:true\n", None).is_err());
    }

    #[test]
    fn seq_streams_are_independent() {
        // 4 endpoint は別採番 (docs/protocol.md §1)。roster は 0.5Hz、yaw は 20Hz なので
        // 1 本の counter で見張ると互いを巻き戻し扱いして 401 が出続ける
        let g = SeqGuard::new();
        for (stream, seq) in [
            ("roster", 8_412u64),
            ("graph", 90_211),
            ("yaw", 55_120),
            ("talk", 311),
        ] {
            assert!(g.accept_stream(stream, "main", seq), "{stream} の初回");
            // 巻き戻り / 同値の再送は破棄。**talk のリプレイはホットマイク**
            assert!(!g.accept_stream(stream, "main", seq), "{stream} の再送");
            assert!(
                !g.accept_stream(stream, "main", seq - 1),
                "{stream} の巻き戻り"
            );
            assert!(g.accept_stream(stream, "main", seq + 1), "{stream} の前進");
        }
        // server_id が違えば独立
        assert!(g.accept_stream("talk", "eu", 1));
    }

    #[test]
    fn check_authentication_body_swaps_mode_only() {
        let body = build_check_authentication_body(&ok_query()).unwrap();
        let m: HashMap<String, String> = form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(m["openid.mode"], "check_authentication");
        assert_eq!(m["openid.sig"], "Zm9vYmFy");
        assert_eq!(
            m["openid.claimed_id"],
            "https://steamcommunity.com/openid/id/76561198000000001"
        );
    }

    #[test]
    fn claimed_id_must_be_steam_and_17_digits() {
        assert!(extract_steam_id("https://steamcommunity.com/openid/id/76561198000000001").is_ok());
        assert!(extract_steam_id("https://evil.example.com/openid/id/76561198000000001").is_err());
        assert!(extract_steam_id("https://steamcommunity.com/openid/id/123").is_err());
        assert!(
            extract_steam_id("https://steamcommunity.com/openid/id/7656119800000000x").is_err()
        );
    }

    #[test]
    fn kvf_parser_is_strict() {
        assert!(parse_check_authentication_response("is_valid:true"));
        assert!(!parse_check_authentication_response("is_valid:false"));
        assert!(!parse_check_authentication_response("is_valid:truthy"));
        assert!(!parse_check_authentication_response(""));
    }

    // ---- セッション / Cookie ----

    #[test]
    fn session_cookie_is_httponly_secure_samesite_lax() {
        let c = session_cookie("deadbeef", true);
        assert!(c.contains("HttpOnly"), "{c}");
        assert!(c.contains("Secure"), "{c}");
        assert!(c.contains("SameSite=Lax"), "{c}");
        assert!(c.starts_with("pv_session=deadbeef;"), "{c}");
        // ローカル開発 (http) でだけ Secure を落とす
        assert!(!session_cookie("deadbeef", false).contains("Secure"));
    }

    #[test]
    fn session_roundtrip_and_expiry() {
        let clock = Arc::new(TestClock::new(1_000));
        let sessions = Sessions::new(clock.clone());

        let token = sessions.issue("76561198000000001".into());
        let header = format!("other=1; {SESSION_COOKIE}={token}");
        let from_cookie =
            |h: &str| cookie_value(h, SESSION_COOKIE).and_then(|t| sessions.lookup(t));
        assert_eq!(from_cookie(&header).as_deref(), Some("76561198000000001"));

        // Cookie が無ければ引けない
        assert_eq!(from_cookie("other=1"), None);
        // 別トークンは通らない
        assert_eq!(from_cookie(&format!("{SESSION_COOKIE}=deadbeef")), None);

        // 期限切れ (sleep せず時計を進める)
        clock.advance(SESSION_TTL_S + 1);
        assert_eq!(from_cookie(&header), None);
    }

    #[test]
    fn require_session_uses_the_process_wide_table() {
        let token = sessions().issue("76561198000000001".into());
        assert_eq!(
            require_session(&format!("{SESSION_COOKIE}={token}")).unwrap(),
            "76561198000000001"
        );
        assert!(require_session("nothing=1").is_err());
    }

    #[test]
    fn login_url_carries_no_secret() {
        let url = steam_login_url(
            "https://vc.example.com",
            "https://vc.example.com/auth/steam/return",
        );
        assert!(url.starts_with(STEAM_OPENID_ENDPOINT));
        assert!(url.contains("openid.mode=checkid_setup"));
        // 画面に出る URL に秘密が混ざっていないこと
        assert!(!url.contains("token"));
        assert!(!url.contains("secret"));
    }
}
