//! 近接ボイスチャットのリレー。**このファイルは親が所有する。**
//!
//! 実装タスクは自分のモジュールの中身を埋める。ここの結線は変更しない
//! (必要なら親へ `[質問]`)。

mod auth;
mod config;
mod proto;
mod roster;
mod sfu;
mod signal;
mod web;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = config::Config::load("config.toml")?;
    let _roster = roster::Roster::new();
    let _hub = signal::Hub::new();
    let _sfu = sfu::Sfu::new(cfg.udp_port)?;

    let app = web::router();
    tracing::info!(domain = %cfg.domain, "starting relay");

    // TLS 終端と ACME は #1-5 が差し込む
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
