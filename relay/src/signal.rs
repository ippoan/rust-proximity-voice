//! WebSocket シグナリング。**担当: #1-1**
//!
//! `docs/protocol.md` §2 の ClientMsg / ServerMsg を仲介する。
//!
//! Hub は「SteamID → そのブラウザへの送信キュー」の表でしかない。
//! WebSocket そのものは `web.rs` が持ち、Hub から受け取った receiver を
//! 読んで流すだけ。こうしておくと、可聴グラフ側 (#1-3) は WebSocket の
//! 存在を知らずに `send` だけで喋れる。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::proto::{ByeReason, ServerMsg, SteamId};

/// 登録の控え。**drop しても登録は消えない** — WebSocket 側は最後に
/// `Hub::unregister` を自分の `token` 付きで呼ぶこと。
pub struct Registration {
    pub steam_id: SteamId,
    /// この登録の世代。後勝ちで切られた古い接続が、後から来た接続の
    /// 登録を消してしまわないための識別子
    pub token: u64,
    /// このブラウザへ流す ServerMsg
    pub rx: UnboundedReceiver<ServerMsg>,
}

struct Entry {
    token: u64,
    tx: UnboundedSender<ServerMsg>,
}

#[derive(Default)]
pub struct Hub {
    sessions: Mutex<HashMap<SteamId, Entry>>,
    next_token: AtomicU64,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SteamId, Entry>> {
        // 中で panic する処理は無いので、毒された錠は中身をそのまま使う
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 認証済みセッションを登録する。既存の同一 SteamID は
    /// `ByeReason::DuplicateSession` で切る (後勝ち)。
    ///
    /// 返る `Registration` の `rx` を読んで WebSocket へ流すのは呼び出し側。
    pub fn register(&self, steam_id: SteamId) -> anyhow::Result<Registration> {
        let (tx, rx) = unbounded_channel();
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);

        let mut sessions = self.lock();
        if let Some(old) = sessions.insert(steam_id.clone(), Entry { token, tx }) {
            // ★ ここは数少ない「本当に切る」ケース (docs/protocol.md §0)
            let _ = old.tx.send(ServerMsg::Bye {
                reason: ByeReason::DuplicateSession,
            });
            tracing::info!(steam_id = %steam_id, "二重接続。前のセッションを切る");
        }

        Ok(Registration {
            steam_id,
            token,
            rx,
        })
    }

    /// 登録を外す。**自分の世代でなければ何もしない。**
    /// 後勝ちで切られた古い接続の後始末が、新しい接続を巻き込まないため。
    pub fn unregister(&self, steam_id: &SteamId, token: u64) {
        let mut sessions = self.lock();
        if sessions.get(steam_id).map(|e| e.token) == Some(token) {
            sessions.remove(steam_id);
        }
    }

    pub fn send(&self, steam_id: &SteamId, msg: ServerMsg) -> anyhow::Result<()> {
        let sessions = self.lock();
        let Some(entry) = sessions.get(steam_id) else {
            anyhow::bail!("セッションが無い: {steam_id}");
        };
        entry
            .tx
            .send(msg)
            .map_err(|_| anyhow::anyhow!("送信キューが閉じている: {steam_id}"))
    }

    /// 複数の ServerMsg をまとめて送る (`set_subscriptions` の戻りなど)。
    pub fn send_all(
        &self,
        steam_id: &SteamId,
        msgs: impl IntoIterator<Item = ServerMsg>,
    ) -> anyhow::Result<()> {
        for msg in msgs {
            self.send(steam_id, msg)?;
        }
        Ok(())
    }

    /// `bye` を送って登録を外す。**ここだけが本当に切る経路** — 二重接続・
    /// BAN / 名簿から外れた・shutdown (docs/protocol.md §0)。
    /// roster TTL 切れや死亡は `Sfu::mute_all` であって、ここではない。
    pub fn kick(&self, steam_id: &SteamId, reason: ByeReason) {
        let mut sessions = self.lock();
        if let Some(entry) = sessions.remove(steam_id) {
            let _ = entry.tx.send(ServerMsg::Bye { reason });
            tracing::info!(steam_id = %steam_id, ?reason, "セッションを切る");
        }
    }

    /// 全員へ `bye` を送って畳む (shutdown)。
    pub fn kick_all(&self, reason: ByeReason) {
        let mut sessions = self.lock();
        for (steam_id, entry) in sessions.drain() {
            let _ = entry.tx.send(ServerMsg::Bye { reason });
            tracing::info!(steam_id = %steam_id, ?reason, "セッションを切る");
        }
    }

    pub fn is_connected(&self, steam_id: &SteamId) -> bool {
        self.lock().contains_key(steam_id)
    }

    pub fn connected(&self) -> Vec<SteamId> {
        self.lock().keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recv(rx: &mut UnboundedReceiver<ServerMsg>) -> Option<ServerMsg> {
        rx.try_recv().ok()
    }

    #[test]
    fn 登録した相手に届く() {
        let hub = Hub::new();
        let id = "7656119800000001".to_string();
        let mut reg = hub.register(id.clone()).unwrap();

        hub.send(&id, ServerMsg::Talk { on: true }).unwrap();
        assert!(matches!(
            recv(&mut reg.rx),
            Some(ServerMsg::Talk { on: true })
        ));
    }

    #[test]
    fn 知らない相手へは送れない() {
        let hub = Hub::new();
        assert!(
            hub.send(&"nobody".to_string(), ServerMsg::Talk { on: true })
                .is_err()
        );
    }

    #[test]
    fn 二重接続は後勝ちで前に_duplicate_session_を送る() {
        let hub = Hub::new();
        let id = "7656119800000001".to_string();
        let mut first = hub.register(id.clone()).unwrap();
        let mut second = hub.register(id.clone()).unwrap();

        assert!(
            matches!(
                recv(&mut first.rx),
                Some(ServerMsg::Bye {
                    reason: ByeReason::DuplicateSession
                })
            ),
            "前のセッションに bye が届いていない"
        );

        // 以後の送信は後のセッションだけに届く
        hub.send(&id, ServerMsg::Talk { on: true }).unwrap();
        assert!(recv(&mut first.rx).is_none());
        assert!(matches!(
            recv(&mut second.rx),
            Some(ServerMsg::Talk { on: true })
        ));
        assert_eq!(hub.len(), 1);
    }

    #[test]
    fn 古い接続の後始末は新しい登録を消さない() {
        let hub = Hub::new();
        let id = "7656119800000001".to_string();
        let first = hub.register(id.clone()).unwrap();
        let second = hub.register(id.clone()).unwrap();

        // 切られた 1 本目の WebSocket タスクが後から片付けに来る
        hub.unregister(&id, first.token);
        assert!(hub.is_connected(&id), "後から来たセッションが消された");

        hub.unregister(&id, second.token);
        assert!(!hub.is_connected(&id));
    }

    #[test]
    fn kick_は_bye_を送って登録を外す() {
        let hub = Hub::new();
        let id = "7656119800000001".to_string();
        let mut reg = hub.register(id.clone()).unwrap();

        hub.kick(&id, ByeReason::NotEligible);
        assert!(matches!(
            recv(&mut reg.rx),
            Some(ServerMsg::Bye {
                reason: ByeReason::NotEligible
            })
        ));
        assert!(!hub.is_connected(&id));
    }

    #[test]
    fn kick_all_は全員を畳む() {
        let hub = Hub::new();
        let mut regs: Vec<_> = (0..3)
            .map(|i| hub.register(format!("id{i}")).unwrap())
            .collect();

        hub.kick_all(ByeReason::ServerShutdown);
        assert!(hub.is_empty());
        for reg in &mut regs {
            assert!(matches!(
                recv(&mut reg.rx),
                Some(ServerMsg::Bye {
                    reason: ByeReason::ServerShutdown
                })
            ));
        }
    }

    #[test]
    fn send_all_はまとめて流す() {
        let hub = Hub::new();
        let id = "a".to_string();
        let mut reg = hub.register(id.clone()).unwrap();

        hub.send_all(
            &id,
            vec![
                ServerMsg::Peer {
                    mid: "1".into(),
                    id: Some("b".into()),
                },
                ServerMsg::Peer {
                    mid: "2".into(),
                    id: None,
                },
            ],
        )
        .unwrap();

        assert!(matches!(recv(&mut reg.rx), Some(ServerMsg::Peer { .. })));
        assert!(matches!(recv(&mut reg.rx), Some(ServerMsg::Peer { .. })));
        assert!(recv(&mut reg.rx).is_none());
    }
}
