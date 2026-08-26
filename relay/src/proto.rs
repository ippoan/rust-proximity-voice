//! 通信仕様 v1 の型。`docs/protocol.md` と 1 対 1 に対応する。
//!
//! **このファイルは親が所有する。実装タスクは変更しないこと。**
//! 仕様と食い違いを見つけたら、実装を合わせるのではなく親へ `[質問]` を上げる。

use serde::{Deserialize, Serialize};

// ---- 定数 (docs/protocol.md §定数) ----
pub const AUDIBLE_M: u16 = 60;
pub const SUBSCRIBE_M: u16 = 75;
pub const GRAPH_HZ: u32 = 2;
pub const YAW_HZ: u32 = 20;
pub const SLOTS: usize = 16;
pub const ROSTER_INTERVAL_S: u64 = 2;
pub const ROSTER_TTL_S: u64 = 10;
pub const DIST_QUANT_M: u16 = 1;
pub const BEARING_QUANT_DEG: u16 = 5;
pub const HMAC_SKEW_S: i64 = 30;
pub const DISCONNECT_GRACE_S: u64 = 60;

pub type SteamId = String;
pub type ServerId = String;

// ---- プラグイン → リレー ----

#[derive(Debug, Clone, Deserialize)]
pub struct RosterPush {
    pub server_id: ServerId,
    pub seq: u64,
    pub ts: i64,
    pub eligible: Vec<SteamId>,
}

/// 可聴グラフ。**絶対座標を含めてはならない。**
#[derive(Debug, Clone, Deserialize)]
pub struct GraphPush {
    pub server_id: ServerId,
    pub seq: u64,
    pub ts: i64,
    pub listeners: Vec<Listener>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Listener {
    pub id: SteamId,
    pub hears: Vec<Heard>,
}

/// 聞き手の向きだけを高レートで運ぶ。**graph とは別便**
/// (graph は `GRAPH_HZ`=2 かつ変化時のみ、yaw は `YAW_HZ`=20)。
/// 1 リクエストに全聞き手ぶんを詰める (1 tick = 1 POST)。
#[derive(Debug, Clone, Deserialize)]
pub struct YawPush {
    pub server_id: ServerId,
    /// endpoint ごとに独立した counter (docs/protocol.md §1)
    pub seq: u64,
    pub ts: i64,
    /// (SteamID, 度 0-355)
    pub yaws: Vec<(SteamId, u16)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heard {
    pub id: SteamId,
    /// 距離 (m), `DIST_QUANT_M` 刻み
    pub d: u16,
    /// **世界座標系での**方位 (度, 0-355), `BEARING_QUANT_DEG` 刻み
    pub b: u16,
    /// 購読すべきか (`d <= SUBSCRIBE_M`)
    pub sub: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TalkPush {
    pub server_id: ServerId,
    /// endpoint ごとに独立した counter (docs/protocol.md §1)
    pub seq: u64,
    pub ts: i64,
    pub id: SteamId,
    pub talking: bool,
}

// ---- ブラウザ ↔ リレー (WebSocket) ----

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello { v: u32 },
    SdpOffer { sdp: String },
    Ice { candidate: serde_json::Value },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Ready {
        steam_id: SteamId,
        server_id: ServerId,
    },
    SdpAnswer {
        sdp: String,
    },
    Ice {
        candidate: serde_json::Value,
    },
    /// スロットの割り当て / 解放。`id: None` は解放
    Peer {
        mid: String,
        id: Option<SteamId>,
    },
    Graph {
        hears: Vec<Heard>,
    },
    /// 聞き手自身の向き。`YAW_HZ` で流れる
    Yaw {
        deg: u16,
    },
    /// 自分の PTT 状態
    Talk {
        on: bool,
    },
    Bye {
        reason: ByeReason,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeReason {
    DuplicateSession,
    NotEligible,
    ServerShutdown,
}
