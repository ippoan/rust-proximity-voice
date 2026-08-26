//! 認証と認可。**担当: #1-3**
//!
//! 3 層 (docs/protocol.md):
//!   認証   = Steam OpenID → SteamID64
//!   認可   = roster に載っているか (+ 任意の静的 whitelist)
//!   配信範囲 = graph が決める
//!
//! **画面に秘密を出さない。** ペアリングコードもトークン付きリンクも使わない。
//! トークンは HttpOnly Cookie で渡す。

use axum::Router;
use crate::proto::SteamId;

/// Steam OpenID の戻りを検証して SteamID64 を得る。
pub async fn verify_steam_openid(_query: &str) -> anyhow::Result<SteamId> {
    todo!("#1-3")
}

/// プラグインからの push を検証する。**3 点すべてを満たさなければ 401。**
///   1. 署名一致 (定数時間比較)
///   2. |now - timestamp| <= HMAC_SKEW_S
///   3. seq が同一 server_id について単調増加
pub fn verify_hmac(
    _secret: &str,
    _timestamp: &str,
    _signature: &str,
    _body: &[u8],
) -> anyhow::Result<()> {
    todo!("#1-3")
}

/// Cookie のトークンからセッションを引く。
pub fn require_session(_cookie: &str) -> anyhow::Result<SteamId> {
    todo!("#1-3")
}

/// このモジュールが提供する route。**#1-3 が中身を実装する。**
/// `web.rs` (#1-1) が `router()` の中で merge する。空でも #1-1 は独立にビルドできる。
pub fn routes() -> Router {
    Router::new()
}
