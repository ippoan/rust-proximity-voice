# デプロイ

リレー (`relay`) を Linux に常駐させる手順。担当 #1-5。

対象は **ゲームサーバーと同じ Linux ホスト**を想定している。別ホストでも動くが、
プラグイン → リレーの `/internal/*` が公開網に出るぶんだけ面倒が増える。

---

## 0. 前提

### DNS 名が必須

**IP 直打ちでは動かない。** ブラウザはマイクの許可を secure context でしか出さないので
HTTPS が要り、HTTPS には証明書が要り、証明書には DNS 名が要る。

ゲームサーバーの IP に **A レコードを 1 本**向ける。

```
vc.example.com.  A  203.0.113.10
```

これだけでよい。証明書は `rustls-acme` が Let's Encrypt から自動で取り、自動で更新する。

### 80 番は開けない

**TLS-ALPN-01** を使っている。ドメイン所有の検証が 443 の TLS ハンドシェイクの中
(ALPN `acme-tls/1` の特別な接続) で完結するので、HTTP-01 と違って 80 番が要らない。

### ファイアウォール

開けるのは **2 本だけ**。

| ポート | 用途 |
|---|---|
| `443/tcp` | PWA 配信 + シグナリング (WSS) + ACME の検証 |
| `<udp_port>/udp` | 音声 (既定 40000) |

```bash
sudo ufw allow 443/tcp
sudo ufw allow 40000/udp
```

**WebRTC で普通に必要な UDP レンジ (10000-20000 など) の開放は要らない。**
str0m は全ピア・全トラフィックを 1 つの UDP ソケットに多重化するため、
外から見えるのは 1 ポートだけ。

---

## 1. ユーザーとディレクトリを作る

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin voicerelay
sudo install -d -o root -g root -m 0755 /opt/voicerelay
```

`/opt/voicerelay` は **root の所有のままでよい**。unit が `ProtectSystem=strict` なので
サービスからは read-only にしか見えず、書き込みは必要ない。

ACME のキャッシュ (`/var/lib/voicerelay`) は **systemd の `StateDirectory=` が
自動で作る**。手で `mkdir` / `chown` する必要はない。

---

## 2. ビルドして配置する

```bash
git clone https://github.com/ippoan/rust-proximity-voice
cd rust-proximity-voice
sudo ./deploy/build.sh install
```

`build.sh` は `cargo build --release` → `deploy/dist/` に staging → `/opt/voicerelay/` へ配置 →
unit を入れて `systemctl enable --now` まで行う。引数なしで実行すると staging だけで止まる。

> **glibc のバージョンに注意。** 動的リンクなので、ビルド機より古い glibc の本番機では
> 起動しない。ゲームサーバー本体か、同じディストリ・同じバージョンのコンテナでビルドする。

> **`setcap` は要らない。** 443 を bind する権限は unit の `AmbientCapabilities=` が渡す。
> バイナリに capability を付けても動くが、入れ替えのたびに消えるので使わない。

手でやるなら:

```bash
cargo build --release
sudo install -m 0755 target/release/relay /opt/voicerelay/voicerelay
sudo install -m 0644 deploy/voicerelay.service /etc/systemd/system/voicerelay.service
sudo systemctl daemon-reload
```

---

## 3. `config.toml` を書く

`deploy/config.example.toml` がひな形。**実物は repo に入れない** (`.gitignore` 済み)。

```bash
sudo install -o root -g voicerelay -m 0640 \
  deploy/config.example.toml /opt/voicerelay/config.toml
sudo -e /opt/voicerelay/config.toml
```

最低限これだけ埋める:

```toml
domain     = "vc.example.com"   # A レコードを向けた名前
hmac_secret = "…"               # プラグインと同じ値 (次節)
udp_port   = 40000              # ファイアウォールで開けた UDP

acme_contact   = "admin@example.com"          # 期限切れ警告の宛先 (任意)
acme_cache_dir = "/var/lib/voicerelay/acme-cache"
```

**`acme_cache_dir` は必ず `/var/lib/voicerelay/` の下にする。** 既定値
(`.acme-cache`) はローカル開発用で、本番では `/opt/voicerelay` が read-only なので書けない。

> `config.toml` は `WorkingDirectory` からの**相対パス**で読まれる。unit が
> `WorkingDirectory=/opt/voicerelay` を指定しているので `/opt/voicerelay/config.toml` になる。

### 起動時に落ちる

`domain` / `hmac_secret` / `udp_port` が欠けていたり空だったりすると、**プロセスは起動に失敗する**。
`/internal/*` が 401 を返し続けるのを後から追うより、`systemctl status` が赤いほうが早い。

---

## 4. HMAC シークレットをプラグインと合わせる

プラグイン → リレーの `/internal/*` は全部この共有鍵で署名される
(`X-PV-Signature`、詳細は [protocol.md](protocol.md) §1)。**片方だけ変えると全部 401 になる。**

```bash
openssl rand -hex 32
```

出た値を **2 か所**に同じだけ入れる:

1. リレー側 — `/opt/voicerelay/config.toml` の `hmac_secret`、または環境変数 `PV_HMAC_SECRET`
2. プラグイン側 — Oxide の設定 (`oxide/config/ProximityVoice.json` 想定、#1-4 の担当)

### 設定ファイルに秘密を置きたくない場合

**`PV_HMAC_SECRET` が設定されていれば `config.toml` の値より優先される。**

```bash
sudo install -o root -g voicerelay -m 0640 /dev/null /etc/voicerelay.env
echo "PV_HMAC_SECRET=$(openssl rand -hex 32)" | sudo tee -a /etc/voicerelay.env >/dev/null
```

unit に 1 行足す (drop-in で足すのが安全):

```bash
sudo systemctl edit voicerelay
```

```ini
[Service]
EnvironmentFile=/etc/voicerelay.env
```

この場合 `config.toml` の `hmac_secret` は空文字のままでよい (環境変数が満たす)。

> `Environment=PV_HMAC_SECRET=…` を unit に直接書かないこと。
> unit ファイルは 0644 で、`systemctl show` からも誰でも読める。

---

## 5. 起動と動作確認

```bash
sudo systemctl enable --now voicerelay
systemctl status voicerelay
```

### 見るべきもの (順に)

**1. プロセスが上がっているか**

```
● voicerelay.service - Rust proximity voice relay
     Active: active (running)
```

`Active: activating (auto-restart)` を繰り返しているなら設定エラー。5 秒ごとに再起動する。

```bash
journalctl -u voicerelay -n 50 --no-pager
```

`設定ファイル config.toml を読めない` / `hmac_secret が空` などがそのまま出る。

**2. 443 を掴めているか**

```bash
ss -tlnp | grep :443
```

出てこないなら bind に失敗している。`0.0.0.0:443 を bind できない` がログに出ていれば
`AmbientCapabilities=CAP_NET_BIND_SERVICE` が効いていない (unit を書き換えたあと
`daemon-reload` を忘れた、など)。

**3. 証明書が取れたか** — ここが本番

```bash
journalctl -u voicerelay -f | grep -i acme
```

| 出る行 | 意味 |
|---|---|
| `ACME (TLS-ALPN-01) を開始` | 起動した。ここまでは必ず出る |
| `TLS-ALPN-01 の検証要求を受けた` | **Let's Encrypt が実際に叩きに来た。** DNS と 443 が正しい |
| `acme event=AccountCacheStore` | ACME アカウントを作ってキャッシュした (初回のみ) |
| `acme event=DeployedNewCert` | **新しい証明書を取って適用した。成功。** |
| `acme event=DeployedCachedCert` | キャッシュ済みの証明書を使った (2 回目以降の起動) |
| `acme event=CertCacheStore` | 証明書をディスクに保存した |

`TLS-ALPN-01 の検証要求を受けた` が出ないまま `acme error=…Order(…)` になるなら、
**Let's Encrypt からこのホストの 443 に届いていない**。DNS の A レコードと
ファイアウォールを疑う (この順で)。

キャッシュの実体:

```bash
sudo ls -l /var/lib/voicerelay/acme-cache/
# cached_account_… と cached_cert_… が 0600 でいるはず
```

**4. 外から見えるか**

```bash
curl -sIv https://vc.example.com/ 2>&1 | grep -E "issuer|subject|HTTP/"
```

### 初回は staging で通す

**本番の Let's Encrypt は「同一ホスト・同一ドメイン組で 1 時間に 5 失敗」で止まる。**
DNS や 443 の疎通を間違えると数回で枯れ、しばらく再挑戦できなくなる。

初回や設定を大きく変えたときは、`config.toml` に

```toml
acme_staging = true
```

を入れて `systemctl restart voicerelay`。staging の証明書は**ブラウザに信頼されない**
(それが正しい) が、`DeployedNewCert` が出れば経路は通っている。確認できたら
`acme_staging = false` に戻し、**キャッシュを消してから**再起動する:

```bash
sudo rm -rf /var/lib/voicerelay/acme-cache
sudo systemctl restart voicerelay
```

(staging と本番でキャッシュのファイル名が違うので消さなくても動くが、
迷ったら消したほうが早い。)

---

## 6. 更新

```bash
cd rust-proximity-voice && git pull
sudo ./deploy/build.sh install
```

`build.sh install` は `config.toml` を**上書きしない**。ACME のキャッシュも触らないので、
更新のたびに証明書を取り直すことはない。

---

## hardening で足してはいけないもの

`deploy/voicerelay.service` は入れられる hardening をだいたい入れてあるが、
**下は入れると壊れる**。「よさそうだから」で足さないこと。

| 設定 | 何が壊れるか |
|---|---|
| `PrivateUsers=yes` | **443 が bind できなくなる。** user namespace の中では `CAP_NET_BIND_SERVICE` がホストのネットワーク名前空間に効かない |
| `ProtectSystem=strict` を `StateDirectory=` 無しで | ACME キャッシュが書けず、**毎回証明書を取りに行ってレート制限に当たる** |
| `RestrictAddressFamilies=` から `AF_NETLINK` を外す | ローカルアドレスの列挙 (`getifaddrs`) と名前解決が死ぬ。**ICE candidate が空になり、TLS は繋がるのに音声だけ繋がらない** という診断しにくい壊れ方をする |
| `PrivateNetwork=yes` / `IPAddressDeny=any` | 全部 |
| `DynamicUser=yes` | `/opt/voicerelay` と `StateDirectory` の所有が毎回変わる |
| `CapabilityBoundingSet=` を空に | 443 が bind できない |

追加したら必ず:

```bash
systemd-analyze verify /etc/systemd/system/voicerelay.service
sudo systemctl restart voicerelay && systemctl status voicerelay
```

の両方を通す。`verify` は構文しか見ないので、**実際に起動して 443 と証明書まで確認する**こと。

---

## 困ったとき

| 症状 | 見るところ |
|---|---|
| 5 秒ごとに再起動する | `journalctl -u voicerelay -n 50`。ほぼ設定の書き間違い |
| `ss -tlnp` に 443 が出ない | `AmbientCapabilities` / `daemon-reload` |
| 証明書が取れない | DNS の A レコード → 443 のファイアウォール → `acme_staging = true` で切り分け |
| プラグインからの push が全部 401 | `hmac_secret` の不一致。`PV_HMAC_SECRET` が設定ファイルを**上書きしている**ことを忘れがち。時刻ずれ (`HMAC_SKEW_S` = 30 秒) も疑う |
| ブラウザでマイクが出ない | HTTPS になっているか。証明書が staging のままだと信頼されない |
| 通話だけ繋がらない | UDP ポートの開放。`RestrictAddressFamilies` から `AF_NETLINK` を外していないか |
