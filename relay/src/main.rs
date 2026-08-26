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

    // 443 を bind し、ACME (TLS-ALPN-01) で証明書を自動取得・自動更新する。
    let listener = relay::config::bind_tls(&cfg).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
