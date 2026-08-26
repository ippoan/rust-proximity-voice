//! HTTP ルーティングと PWA の静的配信。**担当: #1-1**
//!
//! - `/`            PWA (pwa/ ディレクトリ)
//! - `/ws`          WebSocket (認証は auth::require_session)
//! - `/auth/steam`  Steam OpenID (実装は #1-3)
//! - `/internal/*`  プラグインからの push (実装は #1-3)

use axum::Router;

use crate::state::AppState;

/// **担当: #1-1。** `crate::auth::routes(st.clone())` と
/// `crate::roster::routes(st.clone())` を merge すること。
pub fn router(_st: AppState) -> Router {
    todo!("#1-1: 静的配信 (/) と /ws。/auth と /internal は auth/roster の routes を merge")
}
