//! HTTP ルーティングと PWA の静的配信。**担当: #1-1**
//!
//! - `/`            PWA (pwa/ ディレクトリ)
//! - `/ws`          WebSocket (認証は auth::require_session)
//! - `/auth/steam`  Steam OpenID (実装は #1-3)
//! - `/internal/*`  プラグインからの push (実装は #1-3)
//!
//! WebSocket は `docs/protocol.md` §2 の ClientMsg / ServerMsg を素通しするだけ。
//! 音の判断は一切しない — 誰に何を流すかは `Sfu` が、誰が聞けるかは `Roster` が持つ。

use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use tokio::sync::oneshot;
use tower_http::services::ServeDir;

use crate::proto::{ByeReason, ClientMsg, ServerMsg, SteamId};
use crate::state::{AppState, SfuCommand};

/// PWA の置き場。`main` は repo ルートから起動する前提。
const PWA_DIR: &str = "pwa";

/// WebSocket の認証をどう通すか。
///
/// `require_session` (#1-3) は Cookie のトークンを検証する。本番はこれだけ。
/// 検証用サーバー (`examples/dev_relay.rs`) は Steam OpenID を通せないので、
/// **クエリの `?steam_id=` をそのまま信じる**モードを用意する。
/// このモードは `router` からは決して有効にならない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// 本番。Cookie のトークンを `auth::require_session` に渡す
    Session,
    /// **検証専用。** `?steam_id=` を信用する。認証をしていないのと同じ
    TrustQuery,
}

/// **担当: #1-1。** `crate::auth::routes(st.clone())` と
/// `crate::roster::routes(st.clone())` を merge すること。
pub fn router(st: AppState) -> Router {
    router_inner(st, AuthMode::Session, PWA_DIR.into())
}

/// 検証用サーバーの入口。認証を迂回し、静的配信の場所も差し替えられる。
///
/// `pwa/` はまだ空で、Steam OpenID (#1-3) も未実装なので、
/// ブラウザ 2 枚で音が通ることを確かめるにはこれを使う。
pub fn dev_router(st: AppState, static_dir: impl AsRef<Path>) -> Router {
    router_inner(st, AuthMode::TrustQuery, static_dir.as_ref().to_path_buf())
}

fn router_inner(st: AppState, auth: AuthMode, static_dir: PathBuf) -> Router {
    // #1-3 の route。いまは空の Router なので、これで独立にビルドできる
    let external = Router::new()
        .merge(crate::auth::routes(st.clone()))
        .merge(crate::roster::routes(st.clone()));

    Router::new()
        .route("/ws", get(ws_upgrade))
        .with_state(WsState { app: st, auth })
        .merge(external)
        // `/` と PWA の静的ファイル。上のどれにも当たらなければここ
        .fallback_service(ServeDir::new(static_dir).append_index_html_on_directories(true))
}

#[derive(Clone)]
struct WsState {
    app: AppState,
    auth: AuthMode,
}

// ---------------------------------------------------------------------------
// /ws
// ---------------------------------------------------------------------------

async fn ws_upgrade(
    State(st): State<WsState>,
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let steam_id = match authenticate(st.auth, &headers, &query) {
        Ok(id) => id,
        Err(e) => {
            tracing::debug!(error = %e, "ws の認証に失敗");
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    };

    ws.on_upgrade(move |socket| serve_ws(socket, st, steam_id))
}

fn authenticate(
    auth: AuthMode,
    headers: &HeaderMap,
    query: &std::collections::HashMap<String, String>,
) -> anyhow::Result<SteamId> {
    match auth {
        AuthMode::Session => {
            let cookie = headers
                .get(header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            crate::auth::require_session(cookie)
        }
        AuthMode::TrustQuery => query
            .get("steam_id")
            .filter(|s| !s.is_empty())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("?steam_id= が無い (検証モード)")),
    }
}

async fn serve_ws(socket: WebSocket, st: WsState, steam_id: SteamId) {
    let reg = match st.app.hub.register(steam_id.clone()) {
        Ok(reg) => reg,
        Err(e) => {
            tracing::warn!(steam_id = %steam_id, error = %e, "登録に失敗");
            return;
        }
    };
    let token = reg.token;
    let mut rx = reg.rx;

    let (mut sink, mut stream) = socket.split();

    // 送信は 1 本のタスクに寄せる。可聴グラフ側 (#1-3) も SFU も
    // Hub::send を呼ぶだけでよく、WebSocket の存在を知らずに済む
    let pump = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            // bye を送ったら本当に閉じる。ここは数少ない「切る」経路
            let closing = matches!(msg, ServerMsg::Bye { .. });
            let json = match serde_json::to_string(&msg) {
                Ok(json) => json,
                Err(e) => {
                    tracing::error!(error = %e, "ServerMsg を JSON にできない");
                    continue;
                }
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
            if closing {
                let _ = sink.close().await;
                break;
            }
        }
    });

    // **接続時の認可** (docs/protocol.md の「Bye を送る 3 場面」より、
    // `not_eligible` の起点は /ws ハンドラ)。名簿に載っていない・TTL 切れ・
    // whitelist 外はここで断る。
    //
    // 接続**後**に認可を失った場合は Bye ではなく転送停止 (§0)。それは
    // roster.rs が `MuteAll` で行うので、ここでは見ない。
    let server_id = match st.app.roster.server_of(&steam_id) {
        Some(sid) if st.app.roster.is_eligible(&sid, &steam_id) => sid,
        _ => {
            // Bye を送るのは受け手の責務なので、自分では流さず Disconnect を投げる
            tracing::info!(steam_id = %steam_id, "名簿に載っていないので断る");
            let _ = st
                .app
                .sfu
                .send(SfuCommand::Disconnect {
                    steam_id: steam_id.clone(),
                    reason: ByeReason::NotEligible,
                })
                .await;
            pump.await.ok();
            st.app.hub.unregister(&steam_id, token);
            return;
        }
    };

    // 仕様上の 1 通目 (docs/protocol.md §2)
    let _ = st.app.hub.send(
        &steam_id,
        ServerMsg::Ready {
            steam_id: steam_id.clone(),
            server_id,
        },
    );

    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            // Ping / Pong は axum が返す。Binary は使わない
            _ => continue,
        };
        match serde_json::from_str::<ClientMsg>(&text) {
            Ok(msg) => handle_client_msg(&st, &steam_id, msg).await,
            Err(e) => tracing::debug!(steam_id = %steam_id, error = %e, "読めない ClientMsg"),
        }
    }

    // 後始末。
    //
    // **ここで `Disconnect` は送らない。** `Disconnect` は Bye を伴う 3 場面
    // (二重接続・BAN・shutdown) 専用で、ブラウザが自分で閉じた WS はそのどれでもない。
    // §0 の「切断 → 即座に転送停止 → 猶予 60s 後にトランスポート回収」に従い、
    // 転送だけ止める。トランスポートは ICE が落ちた時点で `sfu::run` が回収し、
    // その人を掴んでいた聞き手の枠もそこで解放される。
    pump.abort();
    st.app.hub.unregister(&steam_id, token);
    let _ = st
        .app
        .sfu
        .send(SfuCommand::MuteAll {
            listener: steam_id.clone(),
        })
        .await;
    tracing::info!(steam_id = %steam_id, "ws closed");
}

async fn handle_client_msg(st: &WsState, steam_id: &SteamId, msg: ClientMsg) {
    match msg {
        ClientMsg::Hello { v } => {
            if v != 1 {
                tracing::warn!(steam_id = %steam_id, v, "知らない仕様バージョン");
            }
        }
        ClientMsg::SdpOffer { sdp } => {
            let (reply, rx) = oneshot::channel();
            if st
                .app
                .sfu
                .send(SfuCommand::AcceptOffer {
                    steam_id: steam_id.clone(),
                    sdp,
                    reply,
                })
                .await
                .is_err()
            {
                tracing::error!("sfu が居ない");
                return;
            }
            match rx.await {
                Ok(Ok(sdp)) => {
                    let _ = st.app.hub.send(steam_id, ServerMsg::SdpAnswer { sdp });
                }
                Ok(Err(e)) => {
                    tracing::warn!(steam_id = %steam_id, error = %e, "offer を受理できない")
                }
                Err(_) => tracing::error!("sfu が answer を返す前に落ちた"),
            }
        }
        ClientMsg::Ice { candidate } => {
            let _ = st
                .app
                .sfu
                .send(SfuCommand::Ice {
                    steam_id: steam_id.clone(),
                    candidate,
                })
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn 検証モードはクエリの_steam_id_を使う() {
        let q = query(&[("steam_id", "7656119800000001")]);
        let got = authenticate(AuthMode::TrustQuery, &HeaderMap::new(), &q).unwrap();
        assert_eq!(got, "7656119800000001");
    }

    #[test]
    fn 検証モードでも_steam_id_が無ければ弾く() {
        assert!(authenticate(AuthMode::TrustQuery, &HeaderMap::new(), &query(&[])).is_err());
        assert!(
            authenticate(
                AuthMode::TrustQuery,
                &HeaderMap::new(),
                &query(&[("steam_id", "")])
            )
            .is_err()
        );
    }

    /// 本番モードが Cookie を `auth::require_session` に渡すことだけを見る。
    /// 中身は #1-3 の担当なので、ここでは判定しない。
    #[test]
    fn 本番モードはクエリの_steam_id_を無視する() {
        // require_session が未実装 (`todo!()`) のうちは panic するのが正しい挙動。
        // 「クエリで認証を迂回できてしまう」ことだけは絶対に起きないと示す
        let q = query(&[("steam_id", "7656119800000001")]);
        // todo!() の backtrace で CI のログを埋めない
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let r = std::panic::catch_unwind(|| {
            authenticate(AuthMode::Session, &HeaderMap::new(), &q).ok()
        });
        std::panic::set_hook(prev);
        match r {
            // #1-3 実装前: todo!() で panic する
            Err(_) => {}
            // 実装後: Cookie が無いので必ず失敗する
            Ok(v) => assert!(v.is_none(), "クエリで本番の認証を迂回できてしまった"),
        }
    }
}
