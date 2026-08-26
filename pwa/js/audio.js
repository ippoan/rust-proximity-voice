// Web Audio の距離減衰と定位。
//
// 1 スロット = 1 本の音声 transceiver (mid) = 高々 1 人の声。
//
//   MediaStreamAudioSourceNode → GainNode → StereoPannerNode → master → destination
//
// mid ↔ SteamID の対応は server の { "t":"peer", mid, id } だけが知っている。
// graph は SteamID で来るので、この対応表が無いとどのトラックが誰の声か分からない。
window.PV = window.PV || {};
(function (PV) {
  'use strict';

  // gain / pan をステップで変えるとプチノイズが出るので setTargetAtTime で当てる。
  // 時定数は「graph 2Hz / yaw 20Hz の更新間隔より十分短く、1 フレームでは飛ばない」値。
  var GAIN_TAU = 0.06; // 距離は歩行速度でしか変わらない
  var PAN_TAU = 0.03;  // 振り向きは速いので追従を優先
  var SPEAKING_THRESHOLD = 0.02; // RMS。発話中の名前を出す判定 (下の注意書きを読むこと)

  /**
   * @param {object} opts
   *  - ctx: 既存の AudioContext (省略時は生成)
   */
  function AudioEngine(opts) {
    opts = opts || {};
    this.ctx = opts.ctx || null;
    this.master = null;
    this.slots = Object.create(null);    // mid -> slot
    // 直近の graph = 「今 このブラウザが聞ける相手」の**全状態** (docs/protocol.md §2)。
    // スロットの音量はここから導く。graph は「変化時のみ」なので、
    // **一度取りこぼすと次が来ない**。だから保持して、peer の割り当て時に引き直す
    this.hears = Object.create(null);    // steamId -> { d, b, sub }
    this.yaw = 0;
    // <audio> 要素の置き場。**Chrome の罠 (下記 _attach) のためだけに要る**
    this.sinkHost = null;
  }

  AudioEngine.prototype.start = function () {
    if (!this.ctx) {
      var AC = window.AudioContext || window.webkitAudioContext;
      if (!AC) throw new Error('この browser には Web Audio がありません');
      this.ctx = new AC({ latencyHint: 'interactive' });
    }
    if (!this.ctx.createStereoPanner) {
      throw new Error('この browser には StereoPannerNode がありません (定位ができない)');
    }
    if (!this.master) {
      this.master = this.ctx.createGain();
      this.master.gain.value = 1;
      this.master.connect(this.ctx.destination);
    }
    // AudioContext は user gesture の後でないと running にならない
    return this.ctx.state === 'running' ? Promise.resolve() : this.ctx.resume();
  };

  AudioEngine.prototype.setMasterGain = function (v) {
    if (!this.master) return;
    this.master.gain.setTargetAtTime(v, this.ctx.currentTime, GAIN_TAU);
  };

  AudioEngine.prototype._slot = function (mid) {
    mid = String(mid);
    var s = this.slots[mid];
    if (s) return s;
    var ctx = this.ctx;
    s = {
      mid: mid,
      steamId: null,
      d: Infinity,     // 未知は無音 (fail closed)
      bWorld: 0,
      sub: false,
      src: null,
      audioEl: null,
      gain: ctx.createGain(),
      pan: ctx.createStereoPanner(),
      analyser: null
    };
    s.gain.gain.value = 0; // 割り当てられるまで無音。フェードインは graph が来てから

    // 発話中の名前を出すための tap。**gain の後**に置くのが要点:
    // gain 0 (可聴範囲外だが購読中) の相手は自動的に「喋っていない」扱いになり、
    // 聞こえない相手の発話が UI に出る = ESP になることを構造的に防ぐ。
    s.analyser = ctx.createAnalyser();
    s.analyser.fftSize = 512;
    s.analyser.smoothingTimeConstant = 0.5;
    s._buf = new Float32Array(s.analyser.fftSize);

    s.gain.connect(s.pan);
    s.gain.connect(s.analyser);
    s.pan.connect(this.master);
    this.slots[mid] = s;
    return s;
  };

  /**
   * WebRTC のリモートトラックをスロットに繋ぐ。
   *
   * ★ 必ず踏む罠: Chrome では MediaStreamTrack を MediaStreamAudioSourceNode に
   *   繋いだだけでは音が流れないことがある (トラックが "consume" されないため)。
   *   同じストリームを **muted な <audio> 要素にもアタッチする**のが定番の回避策。
   *   muted なので二重に鳴ることはなく、Web Audio 側だけが聞こえる。
   */
  AudioEngine.prototype.attachTrack = function (mid, track, stream) {
    var s = this._slot(mid);
    var ms = stream || new MediaStream([track]);
    if (s.src) { try { s.src.disconnect(); } catch (e) {} }

    var el = s.audioEl;
    if (!el) {
      if (!this.sinkHost) {
        this.sinkHost = document.createElement('div');
        this.sinkHost.setAttribute('aria-hidden', 'true');
        this.sinkHost.style.display = 'none';
        document.body.appendChild(this.sinkHost);
      }
      el = document.createElement('audio');
      el.autoplay = true;
      el.muted = true;        // ← これが無いと二重に鳴る。これを消さないこと
      el.playsInline = true;
      this.sinkHost.appendChild(el);
      s.audioEl = el;
    }
    el.srcObject = ms;
    // autoplay が弾かれても Web Audio 側は動くので、失敗は握り潰さずログだけ残す
    var p = el.play();
    if (p && p.catch) p.catch(function () {});

    s.src = this.ctx.createMediaStreamSource(ms);
    s.src.connect(s.gain);
    return s;
  };

  /** dev.html 用。WebRTC 抜きで同じ経路 (gain → pan → master) を鳴らす */
  AudioEngine.prototype.attachNode = function (mid, node) {
    var s = this._slot(mid);
    if (s.src) { try { s.src.disconnect(); } catch (e) {} }
    s.src = node;
    node.connect(s.gain);
    return s;
  };

  /**
   * { "t":"peer", mid, id } — mid ↔ SteamID の対応。
   * id: null はそのスロットの解放。トラックも <audio> も残したまま無音にする
   * (再ネゴシエーションを起こさないため。実際の遮断はリレーの転送停止側でされる)。
   */
  AudioEngine.prototype.setPeer = function (mid, steamId) {
    var s = this._slot(mid);
    s.steamId = steamId || null;
    // ★ **graph が peer より先に来ることがある。**
    //   リレー側で Graph は Hub へ直接、Peer は SFU のタスクを経由して出るので、
    //   接続時の撒き直し (issue #11) では Graph のほうが先に届く。
    //   ここで直近の graph を引き直さないと、静止した場面では次の graph が
    //   来ないまま無音のままになる。
    this._syncSlot(s);
    return s;
  };

  /** スロットの d / b / sub を、保持してある graph の全状態から引き直す */
  AudioEngine.prototype._syncSlot = function (s) {
    var h = s.steamId ? this.hears[s.steamId] : null;
    if (h) {
      s.d = Number(h.d);
      s.bWorld = Number(h.b) || 0;
      s.sub = !!h.sub;
    } else {
      // graph に載っていない = 聞こえない。未知は無音 (fail closed)
      s.d = Infinity;
      s.sub = false;
    }
    this._applySlot(s);
  };

  /**
   * 再接続したとき。**スロットのノードは残したまま**、割り当てだけを白紙に戻す。
   * 新しい PeerConnection の peer が来るまで、古い対応で鳴らさないため。
   */
  AudioEngine.prototype.releaseAll = function () {
    this.hears = Object.create(null);
    for (var mid in this.slots) this.setPeer(mid, null);
  };

  /**
   * { "t":"graph", hears:[{id,d,b,sub}] } — 2Hz・変化時のみ。
   * hears は「今そのブラウザが聞ける相手」の**全状態**。載っていない相手は無音にする。
   */
  AudioEngine.prototype.applyGraph = function (hears) {
    var list = hears || [];
    var next = Object.create(null);
    for (var i = 0; i < list.length; i++) next[list[i].id] = list[i];
    // 差分ではなく置き換え。載っていない相手は「もう聞こえない」
    this.hears = next;
    for (var mid in this.slots) this._syncSlot(this.slots[mid]);
  };

  /** { "t":"yaw", deg } — 20Hz。**pan だけ**を再計算する (gain は触らない) */
  AudioEngine.prototype.setYaw = function (deg) {
    this.yaw = Number(deg) || 0;
    if (!this.ctx) return;
    var t = this.ctx.currentTime;
    for (var mid in this.slots) {
      var s = this.slots[mid];
      if (s.d === Infinity) continue; // 無音の相手の pan を動かしても意味が無い
      s.pan.pan.setTargetAtTime(PV.panForBearing(PV.relativeBearing(s.bWorld, this.yaw)), t, PAN_TAU);
    }
  };

  AudioEngine.prototype._applySlot = function (s) {
    var t = this.ctx.currentTime;
    var m = PV.mix(s.d, s.bWorld, this.yaw);
    var g = s.gain.gain;
    // 前回ぶんの予約 (下の「本当に 0 にする」) を消してから当て直す。
    // 消さないと、可聴範囲へ戻ってきた直後に予約が発火して音が切れる
    g.cancelScheduledValues(t);
    g.setTargetAtTime(m.gain, t, GAIN_TAU);
    // setTargetAtTime は指数で近づくだけで 0 には**到達しない**。
    // 可聴範囲外は「小さい音」ではなく無音でなければならないので、
    // 十分減衰したところ (6τ ≒ -52dB) で 0 を予約して止める
    if (m.gain === 0) g.setValueAtTime(0, t + GAIN_TAU * 6);
    s.pan.pan.setTargetAtTime(m.pan, t, PAN_TAU);
  };

  /**
   * 今 **聞こえていて** 喋っている相手の SteamID。
   * 位置は返さない。距離・方位を UI に出すと配信画面がそのまま ESP になる。
   */
  AudioEngine.prototype.speaking = function () {
    var out = [];
    for (var mid in this.slots) {
      var s = this.slots[mid];
      if (!s.steamId) continue;
      // 可聴範囲外 (60〜75m の購読済み) は analyser が gain の後にあるので既に無音だが、
      // 判定でも明示的に落とす。二重の歯止め。
      if (!(s.d <= PV.AUDIBLE_M)) continue;
      s.analyser.getFloatTimeDomainData(s._buf);
      var sum = 0;
      for (var i = 0; i < s._buf.length; i++) sum += s._buf[i] * s._buf[i];
      if (Math.sqrt(sum / s._buf.length) >= SPEAKING_THRESHOLD) out.push(s.steamId);
    }
    return out;
  };

  /** dev / デバッグ用。実際に AudioParam に載っている値を読む */
  AudioEngine.prototype.observe = function (mid) {
    var s = this.slots[String(mid)];
    if (!s) return null;
    return {
      gain: s.gain.gain.value,
      pan: s.pan.pan.value,
      d: s.d, bWorld: s.bWorld, sub: s.sub, steamId: s.steamId
    };
  };

  AudioEngine.prototype.reset = function () {
    for (var mid in this.slots) {
      var s = this.slots[mid];
      try { if (s.src) s.src.disconnect(); } catch (e) {}
      try { s.gain.disconnect(); s.pan.disconnect(); } catch (e) {}
      if (s.audioEl) { s.audioEl.srcObject = null; s.audioEl.remove(); }
    }
    this.slots = Object.create(null);
    this.hears = Object.create(null);
  };

  PV.AudioEngine = AudioEngine;
})(window.PV);
