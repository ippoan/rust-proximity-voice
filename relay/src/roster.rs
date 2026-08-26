//! 接続者名簿と可聴グラフの保持。**担当: #1-3**
//!
//! - roster は **差分ではなく毎回フル**で来る。置き換える
//! - 最終受信から `ROSTER_TTL_S` を過ぎたら **その server_id の全員へ転送停止**
//!   (fail closed)。**切断はしない**
//! - `revoke_on_death` は Config を見る

use axum::Router;

use crate::proto::{GraphPush, Heard, RosterPush, ServerId, SteamId, TalkPush};
use crate::state::AppState;

pub struct Roster {}

impl Roster {
    pub fn new() -> Self {
        todo!("#1-3")
    }

    pub fn apply_roster(&self, _push: RosterPush) -> anyhow::Result<()> {
        todo!("#1-3")
    }

    pub fn apply_graph(&self, _push: GraphPush) -> anyhow::Result<()> {
        todo!("#1-3")
    }

    pub fn apply_talk(&self, _push: TalkPush) -> anyhow::Result<()> {
        todo!("#1-3")
    }

    /// 認可されているか。roster に載っていること (+ 静的 whitelist)。
    pub fn is_eligible(&self, _server: &ServerId, _id: &SteamId) -> bool {
        todo!("#1-3")
    }

    /// その聞き手がいま聞ける相手。TTL 切れなら空を返す。
    pub fn hears_of(&self, _id: &SteamId) -> Vec<Heard> {
        todo!("#1-3")
    }
}

impl Default for Roster {
    fn default() -> Self {
        Self::new()
    }
}

/// このモジュールが提供する route。**#1-3 が中身を実装する。**
/// `web.rs` (#1-1) が `router()` の中で merge する。空でも #1-1 は独立にビルドできる。
pub fn routes(_st: AppState) -> Router {
    Router::new()
}
