//! 共有状態と SFU への指令。**このファイルは親が所有する。変更しない。**
//!
//! `Sfu` は str0m の sans-I/O ループが単独で所有する (Mutex で包まない)。
//! 外からは `SfuCommand` をチャネルで送る。返り値が要るものは `oneshot` で受ける。
//!
//! - **送る側**: `web.rs` (#1-1) と `roster.rs` (#1-3)
//! - **受ける側**: `sfu::run` (#1-1)

use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use crate::proto::{ServerMsg, SteamId};

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<crate::signal::Hub>,
    pub roster: Arc<crate::roster::Roster>,
    pub cfg: Arc<crate::config::Config>,
    pub sfu: mpsc::Sender<SfuCommand>,
}

pub enum SfuCommand {
    /// SDP offer を受け、`proto::SLOTS` 本の transceiver を張って answer を返す
    AcceptOffer {
        steam_id: SteamId,
        sdp: String,
        reply: oneshot::Sender<anyhow::Result<String>>,
    },
    Ice {
        steam_id: SteamId,
        candidate: serde_json::Value,
    },
    /// 転送先の更新。**再ネゴシエーションを起こさず**スロット割り当てを差し替え、
    /// 変化を `ServerMsg::Peer` の列で返す
    SetSubscriptions {
        listener: SteamId,
        speakers: Vec<SteamId>,
        reply: oneshot::Sender<anyhow::Result<Vec<ServerMsg>>>,
    },
    /// その聞き手への **RTP 転送を止めるだけ**。**切断しない**
    /// (死亡・roster TTL 切れ。docs/protocol.md §0)
    MuteAll { listener: SteamId },
    /// セッションの終了 (二重接続・BAN・shutdown のときだけ)
    Disconnect { steam_id: SteamId },
}
