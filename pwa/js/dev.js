// 開発モードの結線。リレー抜きで PV.AudioEngine の本番経路を鳴らす。
//
// ここでやっていることは「graph と yaw を手で作って engine に流す」だけ。
// engine 側は本番と同じコードを通るので、このページで式が正しく鳴っていれば
// リレーが繋がったときも同じ音になる。
(function (PV) {
  'use strict';

  var DEV_MID = '1';                    // 本番の mid "1" (スロット 1 本目) に相当
  var DEV_PEER = '76561198000000042';   // protocol.md の例に出てくるダミー SteamID

  var $ = function (id) { return document.getElementById(id); };
  var engine = new PV.AudioEngine();
  var running = false;
  var srcNode = null;
  var micStream = null;
  var anim = null;

  function fmt(v, n) { return (Math.round(v * Math.pow(10, n)) / Math.pow(10, n)).toFixed(n); }

  // --- 音源 ----------------------------------------------------------------
  // 合成音。定位が分かるようにモノラルで、pan の左右が耳で追えるよう
  // 声に似た音節っぽい包絡 (2 音節/秒) を付ける。外部ファイルを持たない
  // (ビルドステップも配布物も増やさないため)。
  function makeToneNode(ctx) {
    var dur = 2.0;
    var buf = ctx.createBuffer(1, Math.floor(ctx.sampleRate * dur), ctx.sampleRate);
    var ch = buf.getChannelData(0);
    var f0 = 165;
    for (var i = 0; i < ch.length; i++) {
      var t = i / ctx.sampleRate;
      var v = 0;
      for (var h = 1; h <= 6; h++) v += Math.sin(2 * Math.PI * f0 * h * t) / h;
      // 音節の包絡: 0.5 秒周期で立ち上がって落ちる
      var ph = (t % 0.5) / 0.5;
      var env = ph < 0.08 ? ph / 0.08 : (ph < 0.55 ? 1 : Math.max(0, 1 - (ph - 0.55) / 0.25));
      ch[i] = v * 0.18 * env;
    }
    var node = ctx.createBufferSource();
    node.buffer = buf;
    node.loop = true;
    node.start();
    return node;
  }

  function stopSource() {
    if (srcNode) { try { srcNode.disconnect(); } catch (e) {} try { srcNode.stop(); } catch (e) {} srcNode = null; }
    if (micStream) { micStream.getTracks().forEach(function (t) { t.stop(); }); micStream = null; }
  }

  function buildSource() {
    var ctx = engine.ctx;
    stopSource();
    if ($('source').value === 'mic') {
      if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
        return Promise.reject(new Error('この文脈では getUserMedia が使えない (https か localhost で開くこと)'));
      }
      return navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true }
      }).then(function (ms) {
        micStream = ms;
        srcNode = ctx.createMediaStreamSource(ms);
        return srcNode;
      });
    }
    srcNode = makeToneNode(ctx);
    return Promise.resolve(srcNode);
  }

  // --- 状態を engine へ流す ------------------------------------------------
  function pushGraph() {
    var d = parseFloat($('d').value);
    var b = parseFloat($('b').value);
    // 本番と同じ形の graph を作る。sub はリレーが決める値なので d <= SUBSCRIBE_M を再現
    engine.applyGraph([{ id: DEV_PEER, d: d, b: b, sub: d <= PV.SUBSCRIBE_M }]);
  }
  function pushYaw() { engine.setYaw(parseFloat($('yaw').value)); }

  // --- 表示 ----------------------------------------------------------------
  function draw() {
    var d = parseFloat($('d').value), b = parseFloat($('b').value), yaw = parseFloat($('yaw').value);
    var m = PV.mix(d, b, yaw);
    $('dOut').textContent = fmt(d, 1) + ' m';
    $('bOut').textContent = b + '°';
    $('yawOut').textContent = yaw + '°';
    $('gExp').textContent = fmt(m.gain, 4);
    $('pExp').textContent = fmt(m.pan, 4);
    $('bearing').textContent = fmt(m.bearing, 0) + '°';
    $('lr').textContent = m.pan > 0.05 ? '右' : (m.pan < -0.05 ? '左' : (m.bearing > 90 && m.bearing < 270 ? '後ろ' : '正面'));
    $('audible').textContent = d <= PV.AUDIBLE_M ? '可聴' : '無音 (>60m)';
    $('subCell').textContent = d <= PV.SUBSCRIBE_M ? 'sub: true' : 'sub: false';

    var obs = engine.observe(DEV_MID);
    $('gAct').textContent = obs ? fmt(obs.gain, 4) : '—';
    $('pAct').textContent = obs ? fmt(obs.pan, 4) : '—';
    $('gBar').style.width = Math.round((obs ? obs.gain : m.gain) * 100) + '%';
    drawCurve(d, m.gain);
    requestAnimationFrame(draw);
  }

  function drawCurve(curD, curG) {
    var c = $('curve'), g = c.getContext('2d');
    var W = c.width, H = c.height, maxD = 80, pad = 6;
    var x = function (dd) { return (dd / maxD) * W; };
    var y = function (gg) { return H - pad - gg * (H - pad * 2); };
    g.clearRect(0, 0, W, H);
    g.strokeStyle = '#2c313a'; g.lineWidth = 1;
    g.beginPath(); g.moveTo(0, y(0)); g.lineTo(W, y(0)); g.stroke();
    g.setLineDash([4, 4]); g.strokeStyle = '#4a5160';
    [PV.FLAT_M, PV.AUDIBLE_M].forEach(function (dd) {
      g.beginPath(); g.moveTo(x(dd), 0); g.lineTo(x(dd), H); g.stroke();
    });
    g.setLineDash([]);
    g.strokeStyle = '#6ea8fe'; g.lineWidth = 2; g.beginPath();
    for (var i = 0; i <= W; i++) {
      var dd = (i / W) * maxD, gg = PV.gainForDistance(dd);
      if (i === 0) g.moveTo(x(dd), y(gg)); else g.lineTo(x(dd), y(gg));
    }
    g.stroke();
    g.fillStyle = '#e6e9ef';
    g.beginPath(); g.arc(x(curD), y(curG), 4.5, 0, Math.PI * 2); g.fill();
  }

  // --- 式の自己検査 --------------------------------------------------------
  function selfTest() {
    var cases = [
      ['gain(0)', PV.gainForDistance(0), 1],
      ['gain(5)', PV.gainForDistance(5), 1],
      ['gain(32.5)', PV.gainForDistance(32.5), Math.pow(0.5, 1.5)],
      ['gain(60)', PV.gainForDistance(60), 0],
      ['gain(60.5)', PV.gainForDistance(60.5), 0],
      ['gain(70)', PV.gainForDistance(70), 0],
      ['pan(b=90, yaw=0)', PV.panForBearing(PV.relativeBearing(90, 0)), 1],
      ['pan(b=270, yaw=0)', PV.panForBearing(PV.relativeBearing(270, 0)), -1],
      ['pan(b=90, yaw=90)', PV.panForBearing(PV.relativeBearing(90, 90)), 0],
      ['bearing(10, 20)', PV.relativeBearing(10, 20), 350]
    ];
    var rows = ['<tr><th>式</th><th class="num">値</th><th class="num">期待</th><th class="num">判定</th></tr>'];
    var allOk = true;
    cases.forEach(function (c) {
      var ok = Math.abs(c[1] - c[2]) < 1e-9;
      if (!ok) allOk = false;
      rows.push('<tr><td>' + c[0] + '</td><td class="num">' + fmt(c[1], 6) + '</td><td class="num">' + fmt(c[2], 6) +
        '</td><td class="num"><span class="pill ' + (ok ? 'ok' : 'bad') + '">' + (ok ? 'OK' : 'FAIL') + '</span></td></tr>');
    });
    $('selftest').innerHTML = rows.join('');
    return allOk;
  }

  // --- スイープ ------------------------------------------------------------
  function sweep(el, from, to, ms) {
    if (anim) cancelAnimationFrame(anim);
    var t0 = performance.now();
    (function step(now) {
      var k = Math.min(1, (now - t0) / ms);
      el.value = String(from + (to - from) * k);
      el.dispatchEvent(new Event('input'));
      if (k < 1) anim = requestAnimationFrame(step); else anim = null;
    })(t0);
  }

  // --- 結線 ----------------------------------------------------------------
  function setState(text, cls) { var e = $('state'); e.textContent = text; e.className = 'pill ' + cls; }

  window.addEventListener('DOMContentLoaded', function () {
    selfTest();
    ['d', 'b'].forEach(function (id) { $(id).addEventListener('input', function () { if (running) pushGraph(); }); });
    $('yaw').addEventListener('input', function () { if (running) pushYaw(); });
    $('sweepD').addEventListener('click', function () { sweep($('d'), 0, 70, 6000); });
    $('sweepYaw').addEventListener('click', function () { sweep($('yaw'), 0, 359, 6000); });
    $('reset').addEventListener('click', function () {
      if (anim) { cancelAnimationFrame(anim); anim = null; }
      $('d').value = 10; $('b').value = 90; $('yaw').value = 0;
      if (running) { pushGraph(); pushYaw(); }
    });
    $('source').addEventListener('change', function () {
      if (!running) return;
      buildSource().then(function (n) { engine.attachNode(DEV_MID, n); })
        .catch(function (e) { setState(e.message, 'bad'); });
    });

    $('start').addEventListener('click', function () {
      $('start').disabled = true;
      engine.start()
        .then(buildSource)
        .then(function (node) {
          engine.attachNode(DEV_MID, node);
          // 本番の { "t":"peer", mid, id } と同じ経路で対応表を作る
          engine.setPeer(DEV_MID, DEV_PEER);
          running = true;
          pushGraph(); pushYaw();
          setState('鳴っている', 'ok');
          $('hint').textContent = '距離スライダーを 60 より右へ動かすと無音になる。yaw を動かすと pan だけが振れる。';
        })
        .catch(function (e) {
          setState('失敗: ' + e.message, 'bad');
          $('start').disabled = false;
        });
    });

    draw();
  });
})(window.PV);
