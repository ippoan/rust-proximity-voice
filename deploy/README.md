# デプロイ — 担当 #1-5

systemd unit、ACME (TLS-ALPN-01)、設定ファイル。

- 443/tcp と UDP 1 本だけ開ける (str0m は 1 ソケットに多重化する)
- `AmbientCapabilities=CAP_NET_BIND_SERVICE` で非 root のまま 443 を bind
- `rustls-acme` で証明書を自動取得・自動更新。80 番は使わない
