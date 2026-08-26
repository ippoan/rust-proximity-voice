// 本番 UI の結線。
//
// **画面に出してよいのは「今 聞こえていて 喋っている相手」までで、位置は出さない。**
// 認証は Cookie (HttpOnly) なので、トークンを触ることも表示することも無い。
(function (PV) {
  'use strict';

  var $ = function (id) { return document.getElementById(id); };

  var engine = new PV.AudioEngine();
  var signal = new PV.Signal();
  var mic = null;
  var rtc = null;
  var me = null;
  var voicesTimer = null;

  function setLink(text, cls) { var e = $('link'); e.textContent = text; e.className = 'pill ' + cls; }
  function setPtt(on) { var e = $('ptt'); e.textContent = on ? '送信中' : '待機'; e.className = 'pill ' + (on ? 'ok' : ''); }
  function note(t) { $('msg').textContent = t; }

  /** SteamID64 をそのまま並べると読みにくいので末尾だけ。名前は仕様上まだ届かない */
  function shortId(id) { return '…' + String(id).slice(-5); }

  function renderVoices() {
    var ids = engine.speaking();
    var ul = $('voices');
    if (!ids.length) { ul.innerHTML = '<li class="note" style="margin:0">—</li>'; return; }
    ul.innerHTML = ids.map(function (id) {
      return '<li><span class="pill ok">' + shortId(id) + '</span></li>';
    }).join('');
  }

  function connect() {
    $('connect').disabled = true;
    engine.start()
      .then(function () {
        mic = new PV.Mic(engine.ctx, { bufferMs: 300, lookbackMs: 150 });
        return mic.open();
      })
      .then(function () {
        rtc = new PV.Rtc(engine, signal);
        setLink('接続中', 'warn');
        note('リレーへ接続している…');
        signal.open();
        $('disconnect').disabled = false;
        if (!voicesTimer) voicesTimer = setInterval(renderVoices, 150);
      })
      .catch(function (e) {
        setLink('失敗', 'bad');
        note(String(e.message || e));
        $('connect').disabled = false;
      });
  }

  function disconnect() {
    signal.close();
    if (rtc) { rtc.close(); rtc = null; }
    if (mic) { mic.close(); mic = null; }
    engine.reset();
    if (voicesTimer) { clearInterval(voicesTimer); voicesTimer = null; }
    renderVoices();
    setLink('未接続', 'warn');
    setPtt(false);
    $('connect').disabled = false;
    $('disconnect').disabled = true;
  }

  // --- サーバーからのメッセージ (docs/protocol.md §2) ------------------------
  signal.on('ready', function (m) {
    me = m.steam_id;
    note('接続した (' + shortId(me) + ' @ ' + m.server_id + ')。ゲーム内で V を押すと送信される。');
    // 再接続なら古い割り当ては無効。新しい peer が来るまで鳴らさない
    engine.releaseAll();
    rtc.negotiate(mic.track).catch(function (e) {
      setLink('SDP 失敗', 'bad');
      note(String(e.message || e));
    });
  });

  signal.on('sdp_answer', function (m) {
    rtc.onAnswer(m.sdp).then(function () {
      // ネゴシエーションが済んだらマイクを外す。**V を押すまで 1 バイトも送らない**
      return rtc.setMicEnabled(false);
    }).then(function () {
      setLink('通話中', 'ok');
    }).catch(function (e) {
      setLink('SDP 失敗', 'bad');
      note(String(e.message || e));
    });
  });

  signal.on('ice', function (m) { rtc.onIce(m.candidate); });

  // スロットの割り当て / 解放。ここでノードを作り直さない (mid は不変)
  signal.on('peer', function (m) { engine.setPeer(m.mid, m.id); });

  // 2Hz・変化時のみ。距離と世界方位が入る
  signal.on('graph', function (m) { engine.applyGraph(m.hears); });

  // 20Hz。pan だけが再計算される
  signal.on('yaw', function (m) { engine.setYaw(m.deg); });

  // PTT。溜めてある 300ms を遡って送る
  signal.on('talk', function (m) {
    if (!mic || !rtc) return;
    if (m.on) {
      rtc.setMicEnabled(true);
      mic.startSending();
      setPtt(true);
    } else {
      mic.stopSending().then(function () { return rtc.setMicEnabled(false); });
      setPtt(false);
    }
  });

  signal.on('bye', function (m) {
    var why = {
      roster_expired: 'ゲームサーバーからの名簿が途切れた',
      not_eligible: 'このゲームサーバーに接続していない',
      duplicate_session: '同じ Steam アカウントで別のセッションが接続した',
      server_shutdown: 'リレーが停止した'
    }[m.reason] || m.reason;
    setLink('切断', 'bad');
    note('切断された: ' + why);
  });

  signal.on('close', function () {
    if (signal.wantOpen) { setLink('再接続中', 'warn'); }
    else { $('connect').disabled = false; $('disconnect').disabled = true; }
    setPtt(false);
  });

  // --- UI ------------------------------------------------------------------
  window.addEventListener('DOMContentLoaded', function () {
    renderVoices();
    $('connect').addEventListener('click', connect);
    $('disconnect').addEventListener('click', disconnect);
    $('vol').addEventListener('input', function () {
      var v = parseInt(this.value, 10);
      $('volOut').textContent = v + '%';
      engine.setMasterGain(v / 100);
    });
    if ('serviceWorker' in navigator) {
      navigator.serviceWorker.register('sw.js').catch(function (e) { console.warn('[sw]', e); });
    }
  });
})(window.PV);
