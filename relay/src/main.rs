//! 実行バイナリ。**このファイルは親が所有する。**
//!
//! 中身は `lib.rs` の `build()` にある。ここは設定の読み込みと bind だけ。

use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = Arc::new(relay::config::Config::load("config.toml")?);
    let (app, _st) = relay::build(cfg.clone());

    tracing::info!(domain = %cfg.domain, "starting relay");

    // TLS 終端と ACME は #1-5 が差し込む (config::bind_tls)
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
