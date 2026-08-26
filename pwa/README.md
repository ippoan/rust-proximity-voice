# PWA — 担当 #1-2

ブラウザ側。WebRTC で音声を受け、Web Audio で距離減衰と定位をかける。

仕様は `../docs/protocol.md`。特に:

- スロットプール — `peer` メッセージの `mid` ↔ SteamID 対応で音源を識別する
- `graph` は 2 Hz・変化時のみ / `yaw` は 20 Hz。**pan は PWA が `b_world - yaw` から計算する**
- `talk on` を受けてからマイク送信を開始する。**300ms のリングバッファに遡って送る**
- **壁の向こうを描画しない。** 位置を UI に出さない
