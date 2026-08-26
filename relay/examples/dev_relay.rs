//! **検証用サーバー。本番の入口ではない。**
//!
//! ブラウザ 2 枚で実際に音が通ることを確かめるためのもの (#1-1 の受け入れ条件)。
//! `pwa/` (#1-2) と Steam OpenID (#1-3) が揃うまでのあいだ、SFU とシグナリングだけを
//! 単体で動かす。
//!
//! ```sh
//! cargo run -p relay --example dev_relay
//! # ブラウザ 2 枚で
//! #   http://localhost:8080/?steam_id=alice
//! #   http://localhost:8080/?steam_id=bob
//! ```
//!
//! 本番と違うのは 2 点だけで、どちらも `relay` 本体には入っていない:
//!
//! - **Steam OpenID を迂回する** (`web::dev_router` = `?steam_id=` をそのまま信じる)。
//!   名簿による認可 (`/ws` の `is_eligible`) は本番と同じく効くので、
//!   先に `/internal/roster` を push しておく必要がある
//! - 可聴グラフの代わりに `/dev/subscribe` `/dev/mute` で **SFU を直接叩ける**。
//!   #1-1 の受け入れ条件はスロットの挙動なので、graph の解釈 (#1-3) を挟まずに見る

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

use relay::config::Config;
use relay::proto::SteamId;
use relay::roster::Roster;
use relay::signal::Hub;
use relay::state::{AppState, SfuCommand};
use relay::{sfu, web};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let udp_port: u16 = std::env::var("PV_UDP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40000);
    let http_port: u16 = std::env::var("PV_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);

    let cfg = Arc::new(Config {
        domain: "dev".into(),
        hmac_secret: std::env::var("PV_HMAC_SECRET").unwrap_or_else(|_| "dev".into()),
        steam_api_key: None,
        udp_port,
        revoke_on_death: true,
        whitelist: vec![],
        acme_contact: None,
        acme_staging: true,
        acme_cache_dir: std::env::temp_dir().join("pv-dev-acme"),
    });

    let hub = Arc::new(Hub::new());
    let (sfu_tx, sfu_rx) = mpsc::channel(256);
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = sfu::run(udp_port, sfu_rx, hub).await {
                tracing::error!(error = %e, "sfu loop stopped");
            }
        });
    }

    let st = AppState {
        roster: Arc::new(Roster::new(cfg.clone(), sfu_tx.clone(), hub.clone())),
        hub,
        cfg,
        sfu: sfu_tx,
    };

    // 静的配信はこの examples ディレクトリ (index.html が検証用クライアント)
    let static_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    let app = web::dev_router(st.clone(), static_dir).merge(dev_api(st));

    let addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, udp_port, "dev relay listening");
    tracing::info!("ブラウザ 2 枚で http://localhost:{http_port}/?steam_id=alice と ?steam_id=bob");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 可聴グラフの代役
// ---------------------------------------------------------------------------

/// #1-3 の `roster.rs` が入るまで、購読の差し替えを手で起こすための口。
/// **`relay` 本体には無い。** 検証用サーバーだけが生やす。
fn dev_api(st: AppState) -> Router {
    Router::new()
        .route("/dev/subscribe", post(dev_subscribe))
        .route("/dev/mute", post(dev_mute))
        .with_state(st)
}

#[derive(Deserialize)]
struct Subscribe {
    listener: SteamId,
    /// **距離の近い順**に並べること (契約: docs/protocol.md)
    speakers: Vec<SteamId>,
}

async fn dev_subscribe(State(st): State<AppState>, Json(body): Json<Subscribe>) -> String {
    let (reply, rx) = oneshot::channel();
    if st
        .sfu
        .send(SfuCommand::SetSubscriptions {
            listener: body.listener.clone(),
            speakers: body.speakers,
            reply,
        })
        .await
        .is_err()
    {
        return "sfu が居ない".into();
    }
    // `Peer` は SFU が hub 経由で自分で送る。ここは結果を見るだけ
    match rx.await {
        Ok(Ok(())) => "ok".into(),
        Ok(Err(e)) => format!("失敗: {e}"),
        Err(_) => "sfu が落ちた".into(),
    }
}

#[derive(Deserialize)]
struct Mute {
    listener: SteamId,
}

async fn dev_mute(State(st): State<AppState>, Json(body): Json<Mute>) -> String {
    let _ = st
        .sfu
        .send(SfuCommand::MuteAll {
            listener: body.listener,
        })
        .await;
    "muted".into()
}
