//! WebSocket シグナリング。**担当: #1-1**
//!
//! `docs/protocol.md` §2 の ClientMsg / ServerMsg を仲介する。

use crate::proto::SteamId;

pub struct Hub {}

impl Hub {
    pub fn new() -> Self {
        todo!("#1-1")
    }

    /// 認証済みセッションを登録する。既存の同一 SteamID は
    /// `ByeReason::DuplicateSession` で切る (後勝ち)。
    pub fn register(&self, _steam_id: SteamId) -> anyhow::Result<()> {
        todo!("#1-1")
    }

    pub fn send(&self, _steam_id: &SteamId, _msg: crate::proto::ServerMsg) -> anyhow::Result<()> {
        todo!("#1-1")
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}
