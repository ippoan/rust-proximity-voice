//! 近接ボイスチャットのリレー。**このファイルは親が所有する。**
//!
//! bin (`main.rs`) と、`examples/` `tests/` の両方から使えるよう lib にしてある。
//! `mod` をここに集約し、bin 側では二重宣言しない (二重コンパイルになるため)。

// スタブ段階では未使用の関数が大量にある。**全タスクの実装が入ったら外すこと。**
// CI が clippy -D warnings なので、これが無いと骨格の時点で赤くなる。
#![allow(dead_code)]

pub mod auth;
pub mod config;
pub mod proto;
pub mod roster;
pub mod sfu;
pub mod signal;
pub mod state;
pub mod web;

use std::sync::Arc;

/// リレー一式を組み上げて `axum::Router` と共有状態を返す。
///
/// `main.rs` も `examples/` もここを通る。**検証用サーバーを書くときはこれを呼ぶ** —
/// 本番の `main` を経由しないので `Config` を差し替えられる。
pub fn build(cfg: Arc<config::Config>) -> (axum::Router, state::AppState) {
    let hub = Arc::new(signal::Hub::new());

    // Sfu は str0m のループが単独で所有する。外からは指令をチャネルで送る。
    let (sfu_tx, sfu_rx) = tokio::sync::mpsc::channel(256);

    // Roster は起動時に依存を全部受け取る。routes() の中で後から差す形にすると、
    // web.rs が routes() を呼び忘れた瞬間に whitelist と転送が無音で効かなくなる。
    let roster = Arc::new(roster::Roster::new(cfg.clone(), sfu_tx.clone(), hub.clone()));
    {
        let hub = hub.clone();
        let port = cfg.udp_port;
        tokio::spawn(async move {
            if let Err(e) = sfu::run(port, sfu_rx, hub).await {
                tracing::error!(error = %e, "sfu loop stopped");
            }
        });
    }

    let st = state::AppState {
        hub,
        roster,
        cfg,
        sfu: sfu_tx,
    };
    (web::router(st.clone()), st)
}
