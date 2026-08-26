# PWA — 担当 #1-2

ブラウザ側。WebRTC で音声を受け、Web Audio で距離減衰と定位をかける。

仕様は `../docs/protocol.md`。**契約はあちらが正本**で、食い違いを見つけたら実装を合わせず親へ [質問] を上げる。

## ビルドステップが無い

**素の HTML + JS。** バンドラも npm 依存も無く、リレーが静的ファイルとして配るだけ。
そのため `<script type="module">` ではなく**素の `<script>`** で読み込み、名前空間は `window.PV` に置く
（`file://` で開いても CORS で落ちないようにするため。`dev.html` はローカルファイルとしても動く）。

## ファイル

| | |
|---|---|
| `index.html` / `js/app.js` | 本番 UI。接続・PTT 表示・発話中の名前だけ |
| `dev.html` / `js/dev.js` | **開発モード。リレーもゲームも無しで距離減衰と定位を確かめる** |
| `js/protocol.js` | `docs/protocol.md` §3 の式と定数の写し。ここで独自の調整をしない |
| `js/audio.js` | `source → GainNode → StereoPannerNode → master → destination` |
| `js/signal.js` | `/ws` の WebSocket |
| `js/rtc.js` | `RTCPeerConnection`。m-line を並べるのは offer 側 = こちら |
| `js/mic.js` / `js/mic-worklet.js` | マイクと 300ms のリングバッファ |
| `sw.js` / `manifest.json` / `icons/` | PWA として成立させるぶん |
| `verify.mjs` | 受け入れ確認 (node + headless Chrome、外部依存なし) |

## 動かす

```sh
# 開発モード。距離・方位・yaw をスライダーで動かす
python3 -m http.server 8137 --bind 127.0.0.1 --directory pwa
# → http://127.0.0.1:8137/dev.html
```

```sh
# 受け入れ確認 (google-chrome が要る)
node pwa/verify.mjs
```

`verify.mjs` が見ているもの:

1. §3 の式の境界値 (`gain(60)` が厳密に 0 になること、未知の距離が無音になること)
2. リングバッファが talk on の時点から **150ms 遡って**読み出すこと / 無音で遅延を詰めること
3. `dev.html` で距離を 0→70 と振ると `GainNode.gain` が式どおりに動き、60 超で**厳密に 0** になること
4. yaw を振ると `StereoPannerNode.pan` が左右に振れること
5. **`MediaStreamTrack` を繋いだスロットに実際に音が流れること**（下の罠）
6. マイク経路 (`getUserMedia → AudioWorklet → MediaStreamDestination`)。talk off で完全な無音であること
7. **WebRTC をブラウザ内ループバックで通す** — offer の m-line が 17 本 (マイク先頭)、
   `answer` 直後に 16 本を mid ごとに張ること、WebRTC 越しの音が Web Audio に届くこと、
   マイクの入り切りで `signalingState` が `stable` のまま (= 再ネゴシエーションが起きない) こと
8. console にエラーが出ていないこと

**リレーを立てての実地確認はまだ。** ループバックは相手役を同じページに置いているので、
リレー (#1-1) の `Peer` / `graph` / `yaw` / `talk` の実メッセージまでは通していない。

## 踏み抜いた / 避けた罠

### Chrome で WebRTC のトラックを Web Audio に流すと音が出ないことがある

`MediaStreamAudioSourceNode` に繋いだだけでは流れないので、**同じストリームを muted な
`<audio>` 要素にもアタッチする**（`audio.js` の `attachTrack`）。距離減衰が全部ここに乗るので、
これを最初に通した。`verify.mjs` の 5 が回帰テストになっている。

### `setTargetAtTime` は 0 に到達しない

指数で近づくだけなので、可聴範囲外が「とても小さい音」で止まる。**無音でなければならない**ので、
6τ 後に `setValueAtTime(0)` を予約して止める。戻ってきたときのために、当て直す前に
`cancelScheduledValues` で予約を消す。

### スロットのノードを作り直さない

`mid` は割り当てを跨いで不変（リレー側で実測済み）。`peer` でも `graph` でもノード・`<audio>`・
トラックは保持し、**変えるのは gain と pan だけ**。死亡→リスポーンは「gain が 0 に落ちて、また上がる」になる。
作り直すと音が途切れる。

### `ontrack` を待たない

`setRemoteDescription` の直後に全 transceiver の `receiver.track` を mid ごとに張る。
RTP が来る前に Web Audio の要素が揃うので、スロットの割り当てが切り替わっても隙間が出ない。

## セキュリティ要件

- **位置を画面に描かない。** 可聴範囲の相手の距離と方位は届くが、UI に出すと配信画面がそのまま ESP になる。
  出してよいのは発話中の名前程度。発話中の判定に使う `AnalyserNode` は **`GainNode` の後**に置いてあり、
  可聴範囲外の相手は構造的に「喋っていない」になる
- **秘密を画面に出さない。** 認証は Steam ログインで、トークンは HttpOnly Cookie。UI にも URL にも出さない
- PTT を押していない間は `sender.replaceTrack(null)` でトラック自体を外すので、**1 バイトも出て行かない**
