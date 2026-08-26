//! HTTP ルーティングと PWA の静的配信。**担当: #1-1**
//!
//! - `/`            PWA (pwa/ ディレクトリ)
//! - `/ws`          WebSocket (認証は auth::require_session)
//! - `/auth/steam`  Steam OpenID (実装は #1-3)
//! - `/internal/*`  プラグインからの push (実装は #1-3)

use axum::Router;

pub fn router() -> Router {
    todo!("#1-1: 静的配信と /ws。/auth と /internal は #1-3 が nest する")
}
