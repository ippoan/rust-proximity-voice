# デプロイ — 担当 #1-5

**手順は [`docs/deploy.md`](../docs/deploy.md)。** ここは中身の索引。

| ファイル | 何か |
|---|---|
| `voicerelay.service` | systemd unit。`/etc/systemd/system/` へ |
| `config.example.toml` | 設定のひな形。**実物 (`config.toml`) は `.gitignore` 済み** |
| `build.sh` | `cargo build --release` → `/opt/voicerelay/` へ配置 |

要点:

- 開けるのは **443/tcp と UDP 1 本だけ** (str0m は 1 ソケットに多重化する)
- `AmbientCapabilities=CAP_NET_BIND_SERVICE` で**非 root のまま 443 を bind**
- `rustls-acme` の **TLS-ALPN-01** で証明書を自動取得・自動更新。**80 番は使わない**
- 証明書のキャッシュは `StateDirectory=` が用意する `/var/lib/voicerelay/acme-cache`
- `hmac_secret` は環境変数 `PV_HMAC_SECRET` で上書きできる (設定ファイルに秘密を置かない運用)

**hardening に手を足す前に、`docs/deploy.md` の「hardening で足してはいけないもの」を読むこと。**
`PrivateUsers=yes` と `AF_NETLINK` の除去は、どちらも「一見動くのに肝心なところだけ壊れる」。
