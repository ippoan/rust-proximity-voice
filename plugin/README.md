# Oxide プラグイン — 担当 #1-4

Rust ゲームサーバー側 (C#)。座標を聞き手ごとに絞り、リレーへ push する。

仕様は `../docs/protocol.md`。特に:

- **絶対座標を送らない。** 距離 (1m 刻み) と世界方位 (5° 刻み) のみ
- 聞き手ごとに `SUBSCRIBE_M` 以内の相手だけを入れる
- `OnPlayerVoice` で PTT を検出し、**既定のブロードキャストを抑止**する
- CUI で HUD (発話中・接続状態)。**位置は描かない**

---

## 導入

### 1. 置く

```
oxide/plugins/ProximityVoice.cs
```

に `ProximityVoice.cs` を置く。Oxide がロード時にコンパイルする
(**このリポジトリ側でビルドするものは何も無い**)。初回ロードで
`oxide/config/ProximityVoice.json` が既定値で作られる。

### 2. 設定する

`oxide/config/ProximityVoice.json` を編集して:

```jsonc
{
  "relay_url": "https://vc.example.com",
  "hmac_secret": "<リレーの PV_HMAC_SECRET と同じ値>",
  "server_id": "main"
}
```

`hmac_secret` が空のあいだ、プラグインは **1 通も push しない**
(コンソールに `hmac_secret が空` と出る)。設定して `oxide.reload ProximityVoice`。

### 3. 確かめる

コンソールに以下が出れば繋がっている。

```
ProximityVoice 起動: relay=https://vc.example.com server_id=main graph=2Hz yaw=20Hz
リレーへの roster push が通った (204)
```

失敗すると `roster push が失敗: code=401 ...` が **5 秒に 1 行**出る
(20 Hz の yaw でコンソールが埋まらないよう間引いてある)。

---

## 設定項目

| キー | 既定 | 意味 |
|---|---|---|
| `relay_url` | `http://127.0.0.1:8080` | リレーの根 URL。末尾のスラッシュは任意 |
| `hmac_secret` | `""` | 全 push の署名鍵。**リレーと同じ値**。空なら push しない |
| `server_id` | `main` | この Rust サーバーの識別子。リレーの名簿と seq はこの単位 |
| `revoke_on_death` | `true` | 死亡中に失効させるか。**未実測。下の TODO 参照** |
| `audible_m` | `60` | `AUDIBLE_M`。gain は PWA が計算するので**プラグインは使わない**。数字を突き合わせるための控え |
| `subscribe_m` | `75` | `SUBSCRIBE_M`。この距離以内の相手だけを graph に載せる |
| `graph_hz` | `2` | 可聴グラフのレート。**変化時のみ送る** |
| `yaw_hz` | `20` | 聞き手の向きのレート |
| `roster_interval_s` | `2` | 名簿の push 間隔。リレーの TTL は 10 秒 |
| `dist_quant_m` | `1` | 距離の量子化幅 |
| `bearing_quant_deg` | `5` | 方位の量子化幅 |
| `ptt_release_ms` | `200` | `OnPlayerVoice` がこの時間来なくなったら「離した」と見なす |
| `suppress_native_voice` | `true` | ネイティブの近接ブロードキャストを止めるか。false にすると声が二重に聞こえる |
| `hud_enabled` | `true` | CUI の HUD を出すか |
| `hud_hz` | `3` | HUD の更新レート |
| `resend_unchanged_s` | `0` | 変化が無くても再送する間隔。0 で無効。**下の注意を読む** |
| `http_timeout_s` | `2` | HTTP のタイムアウト |

### `resend_unchanged_s` を 1 にすべき場合

既定の `0` は仕様どおりで、**静止中のトラフィックがゼロ**になる
(`docs/protocol.md` の「静止中は graph が飛ばない」)。

ただしこれは「リレーが WS 接続時に保持済みの graph / yaw を撒き直す」ことを前提にしている。
リレー側にその再送が無いと、**途中から繋いだブラウザは誰かが動くまで何も受け取れない**。
`relay/src/roster.rs` には `hears_of` / `yaw_of` という取り出し口があるので、
本来はそちらで解決するのが筋 (#1-1 / #1-3 の領分)。それまでの逃げ道としてここを `1` にする。

---

## 送っているもの

| endpoint | レート | 中身 |
|---|---|---|
| `POST /internal/roster` | 2 秒ごと + 接続 / 切断で即時 | 接続中の SteamID64 の**フル名簿** |
| `POST /internal/graph` | 2 Hz、**変化時のみ** | 聞き手ごとの `{ id, d, b, sub }`。**距離の近い順** |
| `POST /internal/yaw` | 20 Hz | `[[SteamID, 度], ...]` を 1 リクエストに全員ぶん |
| `POST /internal/talk` | PTT の変化の都度 | `{ id, talking }` |

全部に次のヘッダが付く:

```
X-PV-Timestamp: <unix秒>
X-PV-Signature: hex(hmac_sha256(secret, timestamp + "." + body))
```

### seq は endpoint ごとに独立した counter

`roster` / `graph` / `yaw` / `talk` が**別々に**単調増加する。1 本にすると
0.5 Hz の roster と 20 Hz の yaw が互いを巻き戻し扱いして **401 が出続ける**。

**種は unix ミリ秒。** リレーの seq guard はプロセスが生きているあいだ最終 seq を
覚えているので、プラグインを reload するたびに 0 から始めると同じ 401 になる。
壁時計のミリ秒なら、いちばん速い yaw (秒 20 進む) でも 秒 1000 進む種に追い抜かれない。

### 同時に 1 リクエストまで

endpoint 1 本につき、前の応答が返るまで次を投げない。seq の単調増加はリレーが
**受信した順**に見るので、2 本が同時に飛ぶと到着が入れ替わったときに古いほうが 401 で落ちる。
状態を運ぶ graph / yaw / roster は tick を落として次の tick で収束させ、
イベントである talk だけは小さなキューに積む。

---

## 検証

**このリポジトリの開発環境に dotnet は無く、C# をコンパイルできない。**
Oxide プラグインは Rust サーバーがロード時にコンパイルするので運用上は問題ないが、
代わりにワイヤ形式を機械で検証できなくなる。`verify_wire.mjs` がその代わりで、
**プラグインが送るのと同一の JSON と HMAC ヘッダ**を作って実物のリレーに当てる。

```bash
cargo run -p relay --example dev_relay
```

```bash
node plugin/verify_wire.mjs
```

見ているもの:

- プラグインが自前で持つ SHA-256 / HMAC-SHA256 が正しいこと
  (C# の実装をそのまま JS に写して `node:crypto` と突き合わせる)
- 4 endpoint すべてが正しい署名で **204** を返すこと
- 署名を 1 バイト変えると **401**
- `ts` を 31 秒ずらすと **401**、29 秒なら通る
- `seq` を巻き戻す / 同じ値を再送すると **401**
- **`seq` を endpoint ごとに独立させないと 401 が出る**こと
- 送信 JSON に絶対座標のキーが 1 つも無いこと

CI 側にも機械検査がある (`.github/workflows/ci.yml` の `no-absolute-coords`):

```bash
grep -nE '"(pos|position|coord[s]?|world_pos)"' plugin/*.cs
```

が空であること。座標を計算に使うのは自由だが、送信 JSON に載せない。

---

## 実サーバーで潰すこと

### TODO: `revoke_on_death` の既定値

**ネイティブの Rust が死亡中にゲーム内 VC を聞かせるかは未実測。**
`docs/protocol.md` §0 の不変条件は「このシステムは、ゲーム内 VC が聞かせる以上を
聞かせない」なので、ネイティブが聞かせるなら `false` が正しい。実測できない環境なので
既定は安全側 (`true` = 切る) に倒してある。実サーバーで死亡中に近くの人の声が
聞こえるかを確かめて決めること。

### `// ASSUMPTION:` の一覧

コンパイルできない以上、Oxide / Facepunch の API 名とシグネチャは確認できていない。
コード中に `// ASSUMPTION:` で印を付けてあるので、実機で 1 つずつ潰す。

```bash
grep -n 'ASSUMPTION:' plugin/ProximityVoice.cs
```

| 箇所 | 仮定していること | 外れたときの症状 |
|---|---|---|
| `OnPlayerVoice` | `object OnPlayerVoice(BasePlayer, byte[])` で、**非 null を返すと既定の近接ブロードキャストが抑止される** | ゲーム内 VC とリレー越しの声が二重に聞こえる (一聴すれば分かる) |
| `BasePlayer.viewAngles.y` | 聞き手の yaw。+Z を 0° とする時計回り | 定位が回る / 固まる。代替は `player.eyes.rotation.eulerAngles.y` |
| `BaseCombatEntity.IsDead()` | 死亡判定 | `revoke_on_death` が効かない |
| `OnPlayerDeath` / `OnPlayerRespawned` | フック名とシグネチャ | 名簿の反映が最大 2 秒遅れるだけ (フル名簿が毎回上書きするので壊れない) |
| `CuiHelper.AddUi` | `CommunityEntity.ServerInstance.ClientRPC(..., "AddUI", json)` の薄い包み | HUD が出ない |
| `webrequest.Enqueue` | `(url, body, Action<int,string>, Plugin, RequestMethod, Dictionary, float 秒)` | push が 1 通も飛ばない |
| Oxide の timer 分解能 | `0.05s` (=20Hz) がサーバー FPS の範囲で出せる | yaw の追従が鈍い → `yaw_hz` を 10 に落とす |

### SHA-256 を自前で持っている理由

Oxide のプラグインは制限モードで `System.IO` / `System.Net` を含むいくつかの
名前空間を塞ぐ (`UnauthorizedAccessException: System access is restricted`)。
`System.Security.Cryptography.HMACSHA256` が使える保証が無く、ここは**全 push の
必須経路**なので賭けずに自前実装にした。使うのは `uint` の算術と配列だけ。

同じ理由で HTTP は `webrequest` ライブラリを使い、ログもファイルへ直接書かず
`Puts` / `PrintWarning` に流している。
