// マイク送信と PTT (docs/protocol.md §4)。
//
//   getUserMedia → MediaStreamAudioSourceNode → AudioWorklet(mic-ring)
//                → MediaStreamAudioDestinationNode → RTCRtpSender
//
// worklet が常時 300ms 溜めているので、talk on を受けた時点から**遡って**送れる。
window.PV = window.PV || {};
(function (PV) {
  'use strict';

  // 自分の script の隣から worklet を読む。ページの置き場所に依存しないため
  var HERE = (document.currentScript && document.currentScript.src) || (location.href + 'js/mic.js');
  var WORKLET_URL = new URL('mic-worklet.js', HERE).href;

  var CONSTRAINTS = {
    audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
    video: false
  };

  /**
   * @param ctx  AudioContext (AudioEngine と同じものを使う)
   * @param opts { bufferMs, lookbackMs }
   */
  function Mic(ctx, opts) {
    opts = opts || {};
    this.ctx = ctx;
    this.bufferMs = opts.bufferMs || 300;
    this.lookbackMs = opts.lookbackMs || 150;
    this.stream = null;
    this.src = null;
    this.node = null;
    this.dest = null;
    this.track = null;
    this.sending = false;
    this._onDrained = null;
    this.onstate = null; // function(sending)
  }

  /** マイクを開いてリングバッファを回し始める。**この時点では送信していない** */
  Mic.prototype.open = function () {
    var self = this;
    if (this.track) return Promise.resolve(this.track);
    if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
      return Promise.reject(new Error('getUserMedia が使えない (https か localhost で開くこと)'));
    }
    if (!this.ctx.audioWorklet) {
      return Promise.reject(new Error('この browser には AudioWorklet がない (リングバッファを回せない)'));
    }
    return navigator.mediaDevices.getUserMedia(CONSTRAINTS)
      .then(function (ms) {
        self.stream = ms;
        return self.ctx.audioWorklet.addModule(WORKLET_URL);
      })
      .then(function () {
        self.src = self.ctx.createMediaStreamSource(self.stream);
        self.node = new AudioWorkletNode(self.ctx, 'mic-ring', {
          numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1],
          processorOptions: { bufferMs: self.bufferMs, lookbackMs: self.lookbackMs }
        });
        self.node.port.onmessage = function (e) {
          var m = e.data || {};
          if (m.t === 'drained') {
            self.sending = false;
            if (self._onDrained) { var f = self._onDrained; self._onDrained = null; f(); }
            if (self.onstate) self.onstate(false);
          }
        };
        self.dest = self.ctx.createMediaStreamDestination();
        self.src.connect(self.node);
        self.node.connect(self.dest);
        // **destination には繋がない。** 自分の声を自分で鳴らすとハウリングする
        self.track = self.dest.stream.getAudioTracks()[0];
        return self.track;
      });
  };

  /** { "t":"talk", "on":true } — 溜めてある 300ms を遡って送り始める */
  Mic.prototype.startSending = function () {
    if (!this.node || this.sending) return;
    this.sending = true;
    this.node.port.postMessage({ t: 'start' });
    if (this.onstate) this.onstate(true);
  };

  /**
   * { "t":"talk", "on":false }
   * 遡ったぶん出力が遅れているので、溜まりを吐き切ってから止める
   * (即座に切ると語尾が落ちる)。吐き切ったら resolve する。
   */
  Mic.prototype.stopSending = function () {
    var self = this;
    if (!this.node || !this.sending) return Promise.resolve();
    return new Promise(function (res) {
      self._onDrained = res;
      self.node.port.postMessage({ t: 'stop' });
      // worklet が止まっている (AudioContext が suspended 等) 場合の保険
      setTimeout(function () {
        if (self._onDrained === res) { self._onDrained = null; self.sending = false; res(); }
      }, 1000);
    });
  };

  Mic.prototype.close = function () {
    if (this.src) { try { this.src.disconnect(); } catch (e) {} }
    if (this.node) { try { this.node.disconnect(); } catch (e) {} }
    if (this.stream) this.stream.getTracks().forEach(function (t) { t.stop(); });
    this.stream = null; this.src = null; this.node = null; this.dest = null; this.track = null;
    this.sending = false;
  };

  PV.Mic = Mic;
  PV.MIC_WORKLET_URL = WORKLET_URL;
})(window.PV);
