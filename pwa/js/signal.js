// /ws のシグナリング。JSON 1 行 = 1 メッセージ (docs/protocol.md §2)。
//
// 認証は Cookie (HttpOnly) 。**トークンを URL にも画面にも出さない。**
window.PV = window.PV || {};
(function (PV) {
  'use strict';

  // bye を受けたら再接続してよいか。§0 の失効ルールに対応する。
  // 「聞こえてはいけない音声が届かない」ことはリレー側の転送停止で守られているので、
  // 一時的な事象は繋ぎ直してよいが、後勝ち・不適格は繋ぎ直さない。
  var RETRY_ON_BYE = { roster_expired: true, server_shutdown: true, duplicate_session: false, not_eligible: false };

  function Signal(url) {
    this.url = url || (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws';
    this.ws = null;
    this.handlers = Object.create(null);
    this.wantOpen = false;
    this.backoff = 500;
    this._timer = null;
    this.lastBye = null;
  }

  Signal.prototype.on = function (type, fn) {
    (this.handlers[type] || (this.handlers[type] = [])).push(fn);
    return this;
  };

  Signal.prototype._emit = function (type, arg) {
    var hs = this.handlers[type];
    if (!hs) return;
    for (var i = 0; i < hs.length; i++) {
      try { hs[i](arg); } catch (e) { console.error('[signal] handler ' + type, e); }
    }
  };

  Signal.prototype.open = function () {
    this.wantOpen = true;
    this._connect();
  };

  Signal.prototype._connect = function () {
    var self = this;
    if (this._timer) { clearTimeout(this._timer); this._timer = null; }
    if (this.ws && (this.ws.readyState === 0 || this.ws.readyState === 1)) return;

    var ws = new WebSocket(this.url);
    this.ws = ws;

    ws.onopen = function () {
      self.backoff = 500;
      self._emit('open');
      self.send({ t: 'hello', v: 1 });
    };
    ws.onmessage = function (ev) {
      var msg;
      try { msg = JSON.parse(ev.data); } catch (e) { console.warn('[signal] JSON ではない', e); return; }
      if (msg.t === 'bye') {
        self.lastBye = msg.reason || 'unknown';
        // bye は「本当に切る」場面だけ来る (二重接続 / BAN / shutdown)
        if (!RETRY_ON_BYE[self.lastBye]) self.wantOpen = false;
      }
      self._emit(msg.t, msg);
      self._emit('*', msg);
    };
    ws.onclose = function () {
      self._emit('close', self.lastBye);
      if (!self.wantOpen) return;
      self._timer = setTimeout(function () { self._connect(); }, self.backoff);
      self.backoff = Math.min(self.backoff * 2, 15000);
    };
    ws.onerror = function () { /* onclose が続けて呼ばれるのでここでは何もしない */ };
  };

  Signal.prototype.send = function (obj) {
    if (!this.ws || this.ws.readyState !== 1) return false;
    this.ws.send(JSON.stringify(obj));
    return true;
  };

  Signal.prototype.close = function () {
    this.wantOpen = false;
    if (this._timer) { clearTimeout(this._timer); this._timer = null; }
    if (this.ws) { try { this.ws.close(); } catch (e) {} }
  };

  PV.Signal = Signal;
})(window.PV);
