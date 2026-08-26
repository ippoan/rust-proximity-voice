//! WebRTC SFU。**担当: #1-1**
//!
//! str0m を使い、RTP を宛先表に従って転送する。**音声をデコードしない。**
//! スロットプール方式 (docs/protocol.md) により、可聴集合が変わっても
//! 再ネゴシエーションを起こさないこと。

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::proto::{ServerMsg, SteamId};
use crate::signal::Hub;
use crate::state::SfuCommand;

/// SFU のループ。**`Sfu` はここが単独で所有する** (Mutex で包まない)。
/// 指令は `rx` から受け、クライアントへ送るメッセージは `hub` 経由で出す
/// (ICE candidate など、こちら発のものを含む)。
///
/// **担当: #1-1。** シグネチャは親所有 (`state.rs` と対) なので変更しない。
pub async fn run(
    _udp_port: u16,
    _rx: mpsc::Receiver<SfuCommand>,
    _hub: Arc<Hub>,
) -> anyhow::Result<()> {
    todo!("#1-1: UDP 1 ソケットを bind し、指令を処理しつつ RTP を転送する")
}

/// 1 セッション = 1 ブラウザ。接続時に `SLOTS` 本の受信スロットを張る。
pub struct Session {
    pub steam_id: SteamId,
}

pub struct Sfu {}

impl Sfu {
    pub fn new(_udp_port: u16) -> anyhow::Result<Self> {
        todo!("#1-1: str0m の初期化と UDP ソケット 1 本の bind")
    }

    /// SDP offer を受けて answer を返す。ここで `SLOTS` 本の transceiver を張る。
    pub fn accept_offer(&mut self, _steam_id: &SteamId, _sdp: &str) -> anyhow::Result<String> {
        todo!("#1-1")
    }

    /// 転送先の更新。**ここが失効の実体** — 集合から外れた相手の RTP は流さない。
    /// 再ネゴシエーションは起こさず、スロットの割り当てを差し替えて
    /// `ServerMsg::Peer` を返す。
    pub fn set_subscriptions(
        &mut self,
        _listener: &SteamId,
        _speakers: &[SteamId],
    ) -> anyhow::Result<Vec<ServerMsg>> {
        todo!("#1-1")
    }

    /// 全転送を止める (roster TTL 切れ・死亡など)。**切断はしない。**
    pub fn mute_all(&mut self, _listener: &SteamId) -> anyhow::Result<()> {
        todo!("#1-1")
    }
}
