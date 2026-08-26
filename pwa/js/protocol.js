// 通信仕様 v1 (docs/protocol.md) の定数と式。**契約の写しであり、ここで独自の調整をしない。**
// 食い違いを見つけたら実装を歪めず親へ [質問] を上げること。
//
// 素の <script> (module ではない) として読み込む。dev.html を file:// で開いても
// CORS で落ちないようにするため。
window.PV = window.PV || {};
(function (PV) {
  'use strict';

  // --- docs/protocol.md 「定数」 -------------------------------------------
  PV.AUDIBLE_M = 60;    // これを超えると gain 0
  PV.SUBSCRIBE_M = 75;  // 購読を張る距離。60〜75m は購読済みだが gain 0 (ヒステリシス)
  PV.SLOTS = 16;        // 音声受信スロット数
  PV.GRAPH_HZ = 2;
  PV.YAW_HZ = 20;

  // gain 式の内訳。AUDIBLE_M から導けるが、式を読むときに目で追えるよう名前を付ける。
  PV.FLAT_M = 5;                            // ここまでは減衰なし
  PV.FALLOFF_M = PV.AUDIBLE_M - PV.FLAT_M;  // 55
  PV.FALLOFF_EXP = 1.5;

  /**
   * gain(d) = 0                          (d > 60)
   *         = 1.0                        (d <= 5)
   *         = (1 - (d - 5) / 55) ^ 1.5   (5 < d <= 60)
   *
   * d が未知 (まだ graph が来ていない / graph から消えた) 場合は無音。
   * graph は「そのブラウザが聞ける相手だけ」が入る全状態なので、
   * **載っていない = 聞こえない** を fail closed で守る。
   */
  PV.gainForDistance = function (d) {
    if (!(typeof d === 'number') || !isFinite(d) || d < 0) return 0;
    if (d > PV.AUDIBLE_M) return 0;
    if (d <= PV.FLAT_M) return 1;
    return Math.pow(1 - (d - PV.FLAT_M) / PV.FALLOFF_M, PV.FALLOFF_EXP);
  };

  /** bearing = (b_world - yaw + 360) % 360 — b_world は graph、yaw は 20Hz で届く */
  PV.relativeBearing = function (bWorld, yaw) {
    var b = (Number(bWorld) || 0) - (Number(yaw) || 0);
    return ((b % 360) + 360) % 360;
  };

  /** pan = sin(bearing * PI / 180) — StereoPannerNode の -1.0 .. 1.0 */
  PV.panForBearing = function (bearing) {
    return Math.sin(bearing * Math.PI / 180);
  };

  /** graph の d/b と自分の yaw から、そのまま当てられる {gain, pan} を出す */
  PV.mix = function (d, bWorld, yaw) {
    var bearing = PV.relativeBearing(bWorld, yaw);
    return { gain: PV.gainForDistance(d), pan: PV.panForBearing(bearing), bearing: bearing };
  };
})(window.PV);
