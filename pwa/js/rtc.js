// WebRTC。m-line を並べるのは offer 側 = こちら (docs/protocol.md §2)。
//
//   先頭  : マイク × 1        sendonly
//   以降  : 受信スロット × 16 recvonly
//
// answer は m-line を増やせないので、接続時にここで全部並べきる。
// **以後、再ネゴシエーションは一切起こさない。**
window.PV = window.PV || {};
(function (PV) {
  'use strict';

  var GATHER_TIMEOUT_MS = 2500;

  /**
   * @param engine PV.AudioEngine
   * @param signal PV.Signal
   * @param opts   { iceServers?: [], micTrack?: MediaStreamTrack }
   */
  function Rtc(engine, signal, opts) {
    this.engine = engine;
    this.signal = signal;
    this.opts = opts || {};
    this.pc = null;
    this.micSender = null;
    this.pendingIce = [];
    this.remoteSet = false;
    this.slotMids = [];
  }

  Rtc.prototype.negotiate = function (micTrack) {
    var self = this;
    this.close();
    var pc = new RTCPeerConnection({ iceServers: this.opts.iceServers || [] });
    this.pc = pc;
    this.pendingIce = [];
    this.remoteSet = false;

    pc.onicecandidate = function (ev) {
      // gathering 完了を待って offer を出すので、ここに来るのは取りこぼしぶん。
      // 送っておいて損は無い (サーバーが無視しても non-trickle で繋がる)。
      if (ev.candidate) self.signal.send({ t: 'ice', candidate: ev.candidate.toJSON ? ev.candidate.toJSON() : ev.candidate });
    };
    pc.onconnectionstatechange = function () { self._emit('state', pc.connectionState); };
    // ontrack は使わない。setRemoteDescription の直後に mid ごとに張るほうが、
    // スロットの割り当てが切り替わっても Web Audio のノードが既に揃っていて音が途切れない。

    // ★ マイクは必ず先頭。SLOTS を将来変えてもマイクの mid が動かない
    var micTx = pc.addTransceiver(micTrack || 'audio', { direction: 'sendonly' });
    this.micSender = micTx.sender;
    if (!micTrack) {
      // talk on を受けるまでは 1 バイトも送らない。replaceTrack は再ネゴシエーションを起こさない
      try { micTx.sender.replaceTrack(null); } catch (e) {}
    }
    for (var i = 0; i < PV.SLOTS; i++) pc.addTransceiver('audio', { direction: 'recvonly' });

    return pc.createOffer()
      .then(function (offer) { return pc.setLocalDescription(offer); })
      .then(function () { return gatheringDone(pc, GATHER_TIMEOUT_MS); })
      .then(function () {
        self.signal.send({ t: 'sdp_offer', sdp: pc.localDescription.sdp });
      });
  };

  /** { "t":"sdp_answer", sdp } */
  Rtc.prototype.onAnswer = function (sdp) {
    var self = this;
    return this.pc.setRemoteDescription({ type: 'answer', sdp: sdp }).then(function () {
      self.remoteSet = true;
      self.pendingIce.forEach(function (c) { self.pc.addIceCandidate(c).catch(function () {}); });
      self.pendingIce = [];
      self._attachSlots();
    });
  };

  /**
   * 受信スロットのトラックを mid ごとに Web Audio へ張る。**answer の直後に 1 回だけ。**
   * 割り当てが変わっても張り替えない — mid は固定で、リレーが中身だけを差し替える。
   */
  Rtc.prototype._attachSlots = function () {
    var txs = this.pc.getTransceivers();
    this.slotMids = [];
    for (var i = 0; i < txs.length; i++) {
      var t = txs[i];
      if (!t.mid) continue;
      if (t.sender && t.sender === this.micSender) continue;  // 先頭のマイクは受信スロットではない
      var track = t.receiver && t.receiver.track;
      if (!track || track.kind !== 'audio') continue;
      this.engine.attachTrack(t.mid, track);
      this.slotMids.push(t.mid);
    }
    this._emit('slots', this.slotMids);
  };

  /** { "t":"ice", candidate } */
  Rtc.prototype.onIce = function (candidate) {
    if (!candidate || !this.pc) return;
    if (!this.remoteSet) { this.pendingIce.push(candidate); return; }
    this.pc.addIceCandidate(candidate).catch(function (e) { console.warn('[rtc] addIceCandidate', e); });
  };

  /** マイクの送信を入れる / 止める。**再ネゴシエーションは起きない** */
  Rtc.prototype.setMicTrack = function (track) {
    if (!this.micSender) return Promise.resolve();
    return this.micSender.replaceTrack(track || null);
  };

  Rtc.prototype.close = function () {
    if (this.pc) { try { this.pc.close(); } catch (e) {} this.pc = null; }
    this.micSender = null;
    this.remoteSet = false;
  };

  Rtc.prototype.on = function (type, fn) { (this._h || (this._h = {}))[type] = fn; return this; };
  Rtc.prototype._emit = function (type, arg) { if (this._h && this._h[type]) this._h[type](arg); };

  function gatheringDone(pc, timeoutMs) {
    if (pc.iceGatheringState === 'complete') return Promise.resolve();
    return new Promise(function (res) {
      var done = false;
      var finish = function () {
        if (done) return;
        done = true;
        pc.removeEventListener('icegatheringstatechange', check);
        res();
      };
      var check = function () { if (pc.iceGatheringState === 'complete') finish(); };
      pc.addEventListener('icegatheringstatechange', check);
      setTimeout(finish, timeoutMs);
    });
  }

  PV.Rtc = Rtc;
})(window.PV);
