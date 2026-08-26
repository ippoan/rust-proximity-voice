# ファイル所有権

並列タスクが衝突しないための割り当て。**自分の担当外のファイルを編集しない。**
必要になったら親へ `[質問]` を上げる。

| 担当 | 所有するファイル |
|---|---|
| **親 (#p1)** | `README.md` `OWNERSHIP.md` `docs/protocol.md` `Cargo.toml` `relay/Cargo.toml` `relay/src/main.rs` `relay/src/proto.rs` `relay/src/config.rs` |
| **#1-1 リレー SFU** | `relay/src/sfu.rs` `relay/src/signal.rs` `relay/src/web.rs` |
| **#1-2 PWA** | `pwa/**` |
| **#1-3 認証・名簿** | `relay/src/auth.rs` `relay/src/roster.rs` |
| **#1-4 Oxide プラグイン** | `plugin/**` |
| **#1-5 デプロイ** | `deploy/**` `docs/deploy.md` |

## 依存の追加

`relay/Cargo.toml` は親の所有だが、**依存の追加だけは各タスクが `cargo add` してよい**。
既存の依存のバージョンを変更しないこと。追加した依存は `[完了]` に書く。

## 結線

`relay/src/main.rs` の結線は親が置いた。**呼び出し方が合わないときは実装を歪めず親へ `[質問]`。**
`web::router()` に `/auth` と `/internal` を生やすのは #1-3 だが、
`web.rs` 自体は #1-1 の所有なので、**#1-3 は `auth.rs` / `roster.rs` に `pub fn routes() -> Router`
を用意するだけ**にして、#1-1 がそれを `nest` する。
