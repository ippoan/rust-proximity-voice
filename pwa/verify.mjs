// pwa/ の受け入れ確認。**外部依存なし** (node 標準ライブラリと google-chrome だけ)。
//
//   node pwa/verify.mjs
//
// 見ているもの:
//   A. node だけで済むもの
//      1. docs/protocol.md §3 の式 (境界値)
//      2. マイクのリングバッファが「talk on の時点から遡って」読み出すこと
//   B. headless Chrome (CDP) で見るもの
//      3. dev.html が console エラー無しで開くこと
//      4. 距離スライダーを 0→70 と動かすと GainNode.gain が式どおりに動き、60 超で無音になること
//      5. yaw スライダーで StereoPannerNode.pan が左右に振れること
//      6. ★ MediaStreamTrack を繋いだスロットに **実際に音が流れる**こと
//         (Chrome の「muted な <audio> にもアタッチする」罠を踏んでいないこと)
//      7. マイク経路 (getUserMedia → AudioWorklet → MediaStreamDestination) が通ること
import { createServer } from 'node:http';
import { readFile, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { spawn } from 'node:child_process';
import { createHmac } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { dirname, join, normalize } from 'node:path';

const ROOT = dirname(fileURLToPath(import.meta.url));
const results = [];
const ok = (name, extra = '') => { results.push([true, name, extra]); console.log(`  ok   ${name}${extra ? '  ' + extra : ''}`); };
const bad = (name, extra = '') => { results.push([false, name, extra]); console.log(`  FAIL ${name}${extra ? '  ' + extra : ''}`); };
// 環境が揃わなくて**確認できなかった**もの。FAIL とは区別する (埋めたふりをしない)
const skipped = [];
const skip = (name, why) => { skipped.push([name, why]); console.log(`  skip ${name}  — ${why}`); };
const near = (a, b, eps = 1e-9) => Math.abs(a - b) <= eps;

// ---------------------------------------------------------------- A-1 式
console.log('\nA-1. docs/protocol.md §3 の式');
const win = { PV: {} };
globalThis.window = win;
await import('./js/protocol.js');
const PV = win.PV;

for (const [d, want] of [[0, 1], [5, 1], [32.5, Math.pow(0.5, 1.5)], [60, 0], [60.5, 0], [70, 0]]) {
  const got = PV.gainForDistance(d);
  near(got, want, 1e-12) ? ok(`gain(${d}) = ${want.toFixed(6)}`) : bad(`gain(${d})`, `got ${got}`);
}
for (const [d, want] of [[null, 0], [NaN, 0], [Infinity, 0], [-1, 0]]) {
  near(PV.gainForDistance(d), want) ? ok(`gain(${String(d)}) = 0 (未知は無音)`) : bad(`gain(${String(d)})`);
}
near(PV.relativeBearing(10, 20), 350) ? ok('bearing(b=10, yaw=20) = 350 (負に回り込む)') : bad('bearing の回り込み');
near(PV.panForBearing(PV.relativeBearing(90, 0)), 1) ? ok('pan(真右) = +1') : bad('pan(真右)');
near(PV.panForBearing(PV.relativeBearing(270, 0)), -1) ? ok('pan(真左) = -1') : bad('pan(真左)');
near(PV.panForBearing(PV.relativeBearing(90, 90)), 0, 1e-12) ? ok('pan(そちらを向いた) = 0') : bad('pan(正面)');

// ------------------------------------------------- A-2 リングバッファの遡り
console.log('\nA-2. マイクのリングバッファ (talk on から遡って読む)');
{
  const SR = 48000, N = 128;
  globalThis.sampleRate = SR;
  let Processor = null;
  globalThis.registerProcessor = (_n, cls) => { Processor = cls; };
  globalThis.AudioWorkletProcessor = class { constructor() { this.port = makePort(); } };
  function makePort() {
    const port = { onmessage: null, _out: [] };
    port.postMessage = (m) => port._out.push(m);
    port.deliver = (m) => port.onmessage && port.onmessage({ data: m });
    return port;
  }
  await import('./js/mic-worklet.js');
  const p = new Processor({ processorOptions: { bufferMs: 300, lookbackMs: 150 } });

  // マイクには通し番号を入れておく。どこから読み出したかが後で分かる
  let t = 0;
  const inBuf = new Float32Array(N), outBuf = new Float32Array(N);
  const tick = () => {
    for (let i = 0; i < N; i++) inBuf[i] = (t + i) / 1e6;   // 単調増加 = 絶対時刻の代わり
    p.process([[inBuf]], [[outBuf]]);
    t += N;
    return Array.from(outBuf);
  };

  for (let i = 0; i < Math.ceil(SR / N); i++) tick();        // 1 秒ぶん溜める
  const silent = tick().every(v => v === 0);
  silent ? ok('talk off の間は完全な無音を出す (1 バイトも出さない)') : bad('talk off なのに音が出ている');

  const before = t;
  p.port.deliver({ t: 'start' });
  const started = p.port._out.find(m => m.t === 'started');
  const firstOut = tick();
  const firstSample = Math.round(firstOut[0] * 1e6);
  const lag = before - firstSample;
  const lagMs = (lag / SR) * 1000;
  near(lagMs, 150, 6)
    ? ok(`talk on で ${lagMs.toFixed(1)}ms 遡って読み出した`, `(worklet の申告 ${started.lookbackMs.toFixed(1)}ms)`)
    : bad('遡り量が 150ms から外れている', `${lagMs.toFixed(1)}ms`);

  // 無音が続くと遅延が詰まっていく (追いつき)。音のある区間は捨てない
  for (let i = 0; i < N; i++) inBuf[i] = 0;
  for (let i = 0; i < 200; i++) { p.process([[inBuf]], [[outBuf]]); t += N; }
  const backlog = p.written - p.read;
  backlog < lag ? ok(`無音が続くと遅延を詰める (${(lag / SR * 1000).toFixed(0)}ms → ${(backlog / SR * 1000).toFixed(0)}ms)`)
                : bad('無音でも遅延が詰まらない');

  // stop は溜まりを吐き切ってから止まる (語尾を落とさない)
  p.port._out.length = 0;
  p.port.deliver({ t: 'stop' });
  for (let i = 0; i < 200 && !p.port._out.some(m => m.t === 'drained'); i++) p.process([[inBuf]], [[outBuf]]);
  p.port._out.some(m => m.t === 'drained') ? ok('stop は溜まりを吐き切ってから止まる') : bad('drained が来ない');
}

// ------------------------------------------------------------ B. ブラウザ
console.log('\nB. headless Chrome (dev.html)');
const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/manifest+json', '.png': 'image/png', '.mjs': 'text/javascript' };
const server = createServer(async (req, res) => {
  const path = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname)).replace(/^(\.\.[/\\])+/, '');
  const file = join(ROOT, path === '/' ? '/index.html' : path);
  try {
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': MIME[file.slice(file.lastIndexOf('.'))] || 'application/octet-stream' });
    res.end(body);
  } catch { res.writeHead(404); res.end('not found'); }
});
await new Promise(r => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;

// ★ CDP のポートは固定しない。**このマシンでは他のセッションも headless Chrome を
//    動かしている**ので、9222 などを決め打ちすると他人のブラウザに繋いでしまう
//    (実際に踏んだ)。Chrome に選ばせて DevToolsActivePort から読む。
const PROFILE = await mkdtemp(join(tmpdir(), 'pv-verify-'));
const chrome = spawn('google-chrome', [
  '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${PROFILE}`,
  '--no-sandbox', '--disable-gpu', '--no-first-run',
  '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
  '--autoplay-policy=no-user-gesture-required',
  'about:blank'
], { stdio: 'ignore' });

let PORT = 0;
async function waitForCdp() {
  for (let i = 0; i < 80; i++) {
    try {
      PORT = parseInt((await readFile(join(PROFILE, 'DevToolsActivePort'), 'utf8')).split('\n')[0], 10);
      if (PORT > 0 && (await fetch(`http://127.0.0.1:${PORT}/json/version`)).ok) return true;
    } catch {}
    await new Promise(r => setTimeout(r, 250));
  }
  return false;
}

class Cdp {
  constructor(ws) { this.ws = ws; this.id = 0; this.pending = new Map(); this.logs = []; }
  static async open(url) {
    const t = await (await fetch(`http://127.0.0.1:${PORT}/json/new?${encodeURIComponent(url)}`, { method: 'PUT' })).json();
    const ws = new WebSocket(t.webSocketDebuggerUrl);
    await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
    const c = new Cdp(ws);
    ws.onmessage = (e) => {
      const m = JSON.parse(e.data);
      if (m.id && c.pending.has(m.id)) { c.pending.get(m.id)(m); c.pending.delete(m.id); }
      if (m.method === 'Runtime.exceptionThrown') c.logs.push('exception: ' + (m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text));
      if (m.method === 'Runtime.consoleAPICalled' && (m.params.type === 'error' || m.params.type === 'warning')) {
        c.logs.push(m.params.type + ': ' + m.params.args.map(a => a.description ?? a.value).join(' '));
      }
      if (m.method === 'Log.entryAdded' && m.params.entry.level === 'error') c.logs.push('log: ' + m.params.entry.text);
    };
    await c.send('Runtime.enable');
    await c.send('Log.enable');
    return c;
  }
  send(method, params = {}) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise(r => this.pending.set(id, r));
  }
  async eval(expr) {
    const r = await this.send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.result?.exceptionDetails) throw new Error(r.result.exceptionDetails.exception?.description || 'eval failed');
    return r.result?.result?.value;
  }
}

async function settle(cdp, expr, want, eps, ms = 3000) {
  const t0 = Date.now();
  let last = null;
  while (Date.now() - t0 < ms) {
    last = await cdp.eval(expr);
    if (typeof last === 'number' && Math.abs(last - want) <= eps) return last;
    await new Promise(r => setTimeout(r, 60));
  }
  return last;
}

let cdp = null;
try {
  if (!await waitForCdp()) throw new Error('google-chrome の CDP が上がらない');
  cdp = await Cdp.open(`${base}/dev.html`);
  // DOMContentLoaded より前にクリックしても握られないので、結線が済むまで待つ
  for (let i = 0; i < 100; i++) {
    if (await cdp.eval(`document.readyState === 'complete' && !!window.__engine`)) break;
    await new Promise(r => setTimeout(r, 100));
  }

  // 3. 開始できるか
  await cdp.eval(`document.getElementById('start').click()`);
  const state = await settleText(cdp);
  state === '鳴っている' ? ok('dev.html が開始できる (AudioContext が running)') : bad('dev.html が開始できない', state);

  // 4. 距離 0 → 70
  console.log('     距離を 0 → 70 に振る:');
  for (const d of [0, 5, 20, 32.5, 50, 59, 60, 61, 70]) {
    await cdp.eval(`(() => { const e = document.getElementById('d'); e.value = ${d}; e.dispatchEvent(new Event('input')); })()`);
    const want = PV.gainForDistance(d);
    // 可聴範囲外は「小さい音」では駄目なので、そこだけ厳密に 0 を求める
    const eps = want === 0 ? 1e-9 : 0.01;
    const got = await settle(cdp, `window.__engine.observe('1').gain`, want, eps);
    near(got, want, eps) ? ok(`  d=${d}m  gain → ${got.toFixed(6)} (式 ${want.toFixed(6)})`)
                         : bad(`  d=${d}m  gain`, `got ${got}, want ${want}`);
  }

  // 5. yaw
  console.log('     yaw を振る (b_world = 90° 固定):');
  await cdp.eval(`(() => { const e = document.getElementById('d'); e.value = 10; e.dispatchEvent(new Event('input'));
                           const b = document.getElementById('b'); b.value = 90; b.dispatchEvent(new Event('input')); })()`);
  for (const yaw of [0, 90, 180, 270]) {
    await cdp.eval(`(() => { const e = document.getElementById('yaw'); e.value = ${yaw}; e.dispatchEvent(new Event('input')); })()`);
    const want = PV.panForBearing(PV.relativeBearing(90, yaw));
    const got = await settle(cdp, `window.__engine.observe('1').pan`, want, 0.02);
    near(got, want, 0.02) ? ok(`  yaw=${yaw}°  pan → ${got.toFixed(3)} (式 ${want.toFixed(3)})`)
                          : bad(`  yaw=${yaw}°  pan`, `got ${got}, want ${want}`);
  }

  // 6. ★ MediaStreamTrack を繋いだスロットに実際に音が流れるか
  const flow = await cdp.eval(`(async () => {
    const eng = window.__engine;
    const ctx = eng.ctx;
    // WebRTC のリモートトラックの代わり: 実物の MediaStreamTrack を作る
    const dst = ctx.createMediaStreamDestination();
    const osc = ctx.createOscillator(); osc.frequency.value = 440;
    const g = ctx.createGain(); g.gain.value = 0.5;
    osc.connect(g); g.connect(dst); osc.start();
    const track = dst.stream.getAudioTracks()[0];
    const slot = eng.attachTrack('9', track);
    eng.setPeer('9', 'test-peer');
    eng.applyGraph([{ id: 'test-peer', d: 1, b: 0, sub: true }]);
    await new Promise(r => setTimeout(r, 900));
    const buf = new Float32Array(slot.analyser.fftSize);
    slot.analyser.getFloatTimeDomainData(buf);
    let s = 0; for (const v of buf) s += v * v;
    return { rms: Math.sqrt(s / buf.length),
             hasAudioEl: !!slot.audioEl,
             muted: !!(slot.audioEl && slot.audioEl.muted),
             elHasStream: !!(slot.audioEl && slot.audioEl.srcObject) };
  })()`);
  flow.hasAudioEl && flow.muted && flow.elHasStream
    ? ok('MediaStreamTrack を muted な <audio> にもアタッチしている (Chrome の罠回避)')
    : bad('罠回避の <audio> が無い / muted でない', JSON.stringify(flow));
  flow.rms > 0.01
    ? ok(`スロットに実際に音が流れている (RMS ${flow.rms.toFixed(3)})`)
    : bad('MediaStreamTrack を繋いだのに音が流れていない', `RMS ${flow.rms}`);

  // 6b. 60m 超では鳴らないこと (実測)
  const far = await cdp.eval(`(async () => {
    window.__engine.applyGraph([{ id: 'test-peer', d: 70, b: 0, sub: true }]);
    await new Promise(r => setTimeout(r, 900));
    const slot = window.__engine.slots['9'];
    const buf = new Float32Array(slot.analyser.fftSize);
    slot.analyser.getFloatTimeDomainData(buf);
    let s = 0; for (const v of buf) s += v * v;
    return { rms: Math.sqrt(s / buf.length), speaking: window.__engine.speaking().length };
  })()`);
  far.rms < 0.005 ? ok(`d=70m では音が出ない (RMS ${far.rms.toFixed(5)})`) : bad('60m を超えても鳴っている', `RMS ${far.rms}`);
  far.speaking === 0 ? ok('聞こえない相手は「発話中」に出ない (ESP 防止)') : bad('聞こえない相手が発話中に出ている');


  // 6c. ★ **graph が peer より先に来ても拾えること。**
  //     リレー側で Graph は Hub へ直接、Peer は SFU のタスク経由で出るので、
  //     接続時の撒き直し (issue #11) では graph が先に届く。graph は「変化時のみ」なので、
  //     ここで取りこぼすと静止した場面では次が来ず、永久に無音になる。
  const order = await cdp.eval(`(async () => {
    const eng = window.__engine;
    const ctx = eng.ctx;
    const dst = ctx.createMediaStreamDestination();
    const osc = ctx.createOscillator(); osc.frequency.value = 300;
    const g = ctx.createGain(); g.gain.value = 0.5; osc.connect(g); g.connect(dst); osc.start();
    eng.attachTrack('12', dst.stream.getAudioTracks()[0]);
    eng.setYaw(0);   // 直前の yaw 試験で 270° のままなので、明示的に戻す
    // ★ 先に graph、後から peer
    eng.applyGraph([{ id: 'late-peer', d: 2, b: 90, sub: true }]);
    eng.setPeer('12', 'late-peer');
    await new Promise(r => setTimeout(r, 900));
    const slot = eng.slots['12'];
    const b = new Float32Array(slot.analyser.fftSize);
    slot.analyser.getFloatTimeDomainData(b);
    let s = 0; for (const v of b) s += v * v;
    const rms = Math.sqrt(s / b.length);
    const gain = slot.gain.gain.value, pan = slot.pan.pan.value;   // 消す**前**に読む
    // その後 graph から消えたら無音へ戻ること (置き換えであって差分ではない)
    eng.applyGraph([]);
    await new Promise(r => setTimeout(r, 900));
    return { gain, pan, rms, goneGain: slot.gain.gain.value };
  })()`);
  order.gain > 0.9 && order.rms > 0.05
    ? ok(`graph → peer の順で届いても鳴る (gain ${order.gain.toFixed(3)}, RMS ${order.rms.toFixed(3)})`)
    : bad('graph が peer より先だと拾えていない', `gain ${order.gain} rms ${order.rms}`);
  Math.abs(order.pan - 1) < 0.05 ? ok('  その場合も定位が当たっている') : bad('  pan', String(order.pan));
  order.goneGain === 0 ? ok('  graph から消えたら無音へ戻る (差分ではなく置き換え)') : bad('  消えても鳴っている', String(order.goneGain));

  // 7. マイク経路。**本番ページ (index.html) 側**で見る。dev.html は mic.js を読まない
  const page = await Cdp.open(`${base}/index.html`);
  for (let i = 0; i < 100; i++) {
    if (await page.eval(`document.readyState === 'complete' && !!(window.PV && PV.Mic)`)) break;
    await new Promise(r => setTimeout(r, 100));
  }
  const micRes = await page.eval(`(async () => {
    try {
      const ctx = new AudioContext();
      const mic = new PV.Mic(ctx, { bufferMs: 300, lookbackMs: 150 });
      const track = await mic.open();
      const probe = ctx.createAnalyser();
      ctx.createMediaStreamSource(mic.dest.stream).connect(probe);
      await new Promise(r => setTimeout(r, 400));
      const read = () => { const b = new Float32Array(probe.fftSize); probe.getFloatTimeDomainData(b);
                           let s = 0; for (const v of b) s += v * v; return Math.sqrt(s / b.length); };
      // Chrome の疑似マイクは 1 秒周期のビープなので、瞬間値ではなく窓の最大を見る
      const peakOver = async (ms) => { let p = 0, t0 = Date.now();
        while (Date.now() - t0 < ms) { p = Math.max(p, read()); await new Promise(r => setTimeout(r, 40)); }
        return p; };
      const idle = await peakOver(1200);
      mic.startSending();
      const live = await peakOver(1600);
      await mic.stopSending();
      await new Promise(r => setTimeout(r, 400));
      const after = await peakOver(1200);
      mic.close();
      return { kind: track.kind, idle, live, after };
    } catch (e) { return { error: String(e && e.message || e) }; }
  })()`);
  if (micRes.error) bad('マイク経路', micRes.error);
  else {
    micRes.kind === 'audio' ? ok('getUserMedia → AudioWorklet → MediaStreamDestination が通る') : bad('マイクのトラックが取れない');
    micRes.idle === 0 ? ok('talk off では完全な無音 (RMS 0)') : bad('talk off なのに音が出ている', String(micRes.idle));
    micRes.live > 1e-3 ? ok(`talk on で音が流れ出す (RMS ピーク ${micRes.live.toFixed(5)})`) : bad('talk on でも音が出ない', String(micRes.live));
    micRes.after === 0 ? ok('talk off に戻ると完全な無音に戻る (RMS 0)') : bad('talk off に戻っても止まらない', String(micRes.after));
  }

  // 8. 本番ページが素で開けるか (manifest / service worker / 位置を出していないこと)
  const shell = await page.eval(`(() => ({
    manifest: !!document.querySelector('link[rel=manifest]'),
    icons: !!document.querySelector('link[rel=icon]')
  }))()`);
  shell.manifest && shell.icons ? ok('index.html が manifest / アイコンを参照している') : bad('manifest かアイコンが無い');
  const swReg = await page.eval(`navigator.serviceWorker.getRegistration().then(r => !!r)`);
  swReg ? ok('service worker が登録される') : bad('service worker が登録されない');

  // 位置を UI に出していないこと。graph を流し込んでから「発話中」欄を覗く
  const leak = await page.eval(`(async () => {
    // app.js の engine を直接は触れないので、同じ経路を組んで表示関数の出力を見る
    const eng = new PV.AudioEngine();
    await eng.start();
    eng.setPeer('1', '76561198000000042');
    eng.applyGraph([{ id: '76561198000000042', d: 23, b: 145, sub: true }]);
    return JSON.stringify(eng.speaking());   // 位置が混ざっていないか
  })()`);
  !/23|145|"d"|"b"/.test(leak)
    ? ok('可聴範囲の相手の距離 / 方位が UI 側へ出て行かない (ESP 防止)')
    : bad('UI へ渡る値に位置が混ざっている', leak);

// --- C. WebRTC (ブラウザ内ループバック) -----------------------------------
// リレーを立てずに rtc.js を通す。相手役の RTCPeerConnection を同じページに置き、
// 「offer を受けて 16 本の recvonly に音を載せて answer を返す」だけをやらせる。
// 見ているのは rtc.js 側の挙動 — m-line の並び / mid / answer 後の張り方 /
// マイクの入り切りで再ネゴシエーションが起きないこと。
const RTC_TEST = `(async () => {
  const out = { steps: [] };
  const engine = new PV.AudioEngine();
  await engine.start();
  const ctx = engine.ctx;

  const tone = (hz) => {
    const dst = ctx.createMediaStreamDestination();
    const osc = ctx.createOscillator(); osc.frequency.value = hz;
    const g = ctx.createGain(); g.gain.value = 0.6;
    osc.connect(g); g.connect(dst); osc.start();
    return dst.stream.getAudioTracks()[0];
  };
  const rms = (an) => { const b = new Float32Array(an.fftSize); an.getFloatTimeDomainData(b);
                        let s = 0; for (const v of b) s += v * v; return Math.sqrt(s / b.length); };
  const gathered = (pc) => pc.iceGatheringState === 'complete' ? Promise.resolve()
    : new Promise(r => { const f = () => { if (pc.iceGatheringState === 'complete') { pc.removeEventListener('icegatheringstatechange', f); r(); } };
                         pc.addEventListener('icegatheringstatechange', f); setTimeout(r, 4000); });

  // PV.Signal の代役 — 送られたものを溜めるだけ
  const signal = { sent: [], send(m) { this.sent.push(m); return true; } };
  const rtc = new PV.Rtc(engine, signal);

  const micTrack = tone(880);
  await rtc.negotiate(micTrack);
  const offer = signal.sent.find(m => m.t === 'sdp_offer');
  out.offerSent = !!offer;
  out.mLines = (offer.sdp.match(/^m=audio/gm) || []).length;
  out.directions = (offer.sdp.match(/^a=(sendonly|recvonly|sendrecv|inactive)/gm) || []).map(s => s.slice(2));

  // --- 相手役 (リレーの代役) ---
  const server = new RTCPeerConnection();
  await server.setRemoteDescription({ type: 'offer', sdp: offer.sdp });
  const stx = server.getTransceivers();
  stx[0].direction = 'recvonly';                       // PWA のマイクを受ける側
  for (let i = 1; i < stx.length; i++) stx[i].direction = 'sendonly';
  const slotTx = stx[3];                               // 4 本目の m-line に声を載せる
  const slotMid = slotTx.mid;
  await slotTx.sender.replaceTrack(tone(330));
  const micIn = ctx.createAnalyser();
  ctx.createMediaStreamSource(new MediaStream([stx[0].receiver.track])).connect(micIn);

  await server.setLocalDescription(await server.createAnswer());
  await gathered(server);
  await rtc.onAnswer(server.localDescription.sdp);

  out.slotMids = rtc.slotMids.slice();
  out.slotMidCount = rtc.slotMids.length;
  out.micMid = stx[0].mid;
  out.signalingState = rtc.pc.signalingState;

  for (let i = 0; i < 100 && rtc.pc.connectionState !== 'connected'; i++) await new Promise(r => setTimeout(r, 100));
  out.connectionState = rtc.pc.connectionState;

  // 割り当てて鳴らす
  engine.setPeer(slotMid, 'peer-a');
  engine.applyGraph([{ id: 'peer-a', d: 2, b: 90, sub: true }]);
  const slot = engine.slots[slotMid];
  out.slotHasAudioEl = !!(slot && slot.audioEl && slot.audioEl.muted);
  let peak = 0;
  for (let i = 0; i < 40; i++) { await new Promise(r => setTimeout(r, 50)); if (slot) peak = Math.max(peak, rms(slot.analyser)); }
  out.heardRms = peak;
  out.pan = slot ? slot.pan.pan.value : null;

  // 60m の外へ出す → 無音になる (購読は張ったまま)
  engine.applyGraph([{ id: 'peer-a', d: 70, b: 90, sub: true }]);
  await new Promise(r => setTimeout(r, 900));
  let farPeak = 0;
  for (let i = 0; i < 10; i++) { await new Promise(r => setTimeout(r, 40)); farPeak = Math.max(farPeak, rms(slot.analyser)); }
  out.farRms = farPeak;
  out.farGain = slot.gain.gain.value;

  // マイクの入り切りで再ネゴシエーションが起きないこと
  await rtc.setMicEnabled(true);
  let micPeak = 0;
  for (let i = 0; i < 30; i++) { await new Promise(r => setTimeout(r, 50)); micPeak = Math.max(micPeak, rms(micIn)); }
  out.micHeardRms = micPeak;
  out.stateAfterMicOn = rtc.pc.signalingState;
  await rtc.setMicEnabled(false);
  await new Promise(r => setTimeout(r, 600));
  let micOff = 0;
  for (let i = 0; i < 15; i++) { await new Promise(r => setTimeout(r, 40)); micOff = Math.max(micOff, rms(micIn)); }
  out.micOffRms = micOff;
  out.stateAfterMicOff = rtc.pc.signalingState;
  out.midsUnchanged = JSON.stringify(rtc.slotMids) === JSON.stringify(out.slotMids);

  rtc.close(); server.close();
  return out;
})()`;

console.log('\nC. WebRTC (ブラウザ内ループバック)');
try {
  const r = await page.eval(RTC_TEST);
  r.mLines === 17 ? ok(`offer に m-line が 17 本 (マイク 1 + スロット ${PV.SLOTS})`) : bad('m-line の本数', String(r.mLines));
  r.directions[0] === 'sendonly' && r.directions.slice(1).every(d => d === 'recvonly')
    ? ok('先頭がマイク (sendonly)、以降がスロット (recvonly)') : bad('m-line の並び', r.directions.join(','));
  r.micMid === '0' ? ok('マイクの mid は "0"') : bad('マイクの mid', String(r.micMid));
  r.slotMidCount === PV.SLOTS ? ok(`answer 直後に ${PV.SLOTS} 本すべてを mid ごとに張った (ontrack を待たない)`)
                              : bad('張られたスロット数', String(r.slotMidCount));
  r.connectionState === 'connected' ? ok('PeerConnection が繋がる') : bad('繋がらない', r.connectionState);
  r.slotHasAudioEl ? ok('リモートトラックが muted な <audio> にもアタッチされている') : bad('罠回避の <audio> が無い');
  r.heardRms > 0.05 ? ok(`WebRTC 越しの音がスロットに流れている (RMS ${r.heardRms.toFixed(3)})`)
                    : bad('WebRTC 越しの音が流れていない', String(r.heardRms));
  Math.abs(r.pan - 1) < 0.05 ? ok(`定位が当たっている (b=90°, yaw=0° → pan ${r.pan.toFixed(3)})`) : bad('pan', String(r.pan));
  r.farGain === 0 && r.farRms < 0.005
    ? ok('60m の外へ出ると、購読を張ったまま gain が厳密に 0 になる') : bad('60m 外で無音にならない', `gain ${r.farGain} rms ${r.farRms}`);
  r.micHeardRms > 0.01 ? ok(`talk on でマイクが相手側に届く (RMS ${r.micHeardRms.toFixed(3)})`) : bad('マイクが届かない', String(r.micHeardRms));
  r.micOffRms < 0.01 ? ok(`talk off で止まる (RMS ${r.micOffRms.toFixed(4)})`) : bad('talk off でも送っている', String(r.micOffRms));
  r.stateAfterMicOn === 'stable' && r.stateAfterMicOff === 'stable' && r.midsUnchanged
    ? ok('マイクの入り切りで再ネゴシエーションが起きない (signalingState は stable のまま / mid も不変)')
    : bad('再ネゴシエーションが起きている', `${r.stateAfterMicOn}/${r.stateAfterMicOff}`);
} catch (e) {
  bad('WebRTC ループバック', String(e && e.message || e));
}

// --- D. 本物のリレー相手 (relay/examples/dev_relay) ------------------------
// C はリレーの代役を同じページに書いたものなので、「代役が実物と違っていた」箇所は
// 出てこない。ここは #1-1 の dev_relay を実際に立てて、signal.js が本物の
// hello / sdp_answer / peer / bye を受け取れるかを見る。
//
// HTTP は 127.0.0.1 に bind する (dev_relay の既定)。立たなければ FAIL ではなく skip。
console.log('\nD. リレー (relay/examples/dev_relay) 相手の実地確認');
{
  const freePort = () => new Promise(res => {
    const s = createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)); });
  });
  const HTTP_PORT = await freePort();
  const UDP_PORT = await freePort();
  const SECRET = 'dev', SERVER_ID = 'dev';
  const HTTP = `http://127.0.0.1:${HTTP_PORT}`;
  let seq = 1;
  const internal = async (path, body) => {
    const payload = JSON.stringify({ server_id: SERVER_ID, seq: seq++, ts: Math.floor(Date.now() / 1000), ...body });
    const ts = String(Math.floor(Date.now() / 1000));
    const sig = createHmac('sha256', SECRET).update(`${ts}.${payload}`).digest('hex');
    const r = await fetch(HTTP + path, { method: 'POST',
      headers: { 'content-type': 'application/json', 'X-PV-Timestamp': ts, 'X-PV-Signature': sig }, body: payload });
    return `${r.status} ${await r.text()}`;
  };
  const dev = (path, body) => fetch(HTTP + path, { method: 'POST',
    headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }).then(r => r.text());

  let relay = null, keepalive = null, relayPage = null;
  try {
    relay = spawn('cargo', ['run', '-q', '-p', 'relay', '--example', 'dev_relay'], {
      cwd: join(ROOT, '..'),
      env: { ...process.env, PV_HTTP_PORT: String(HTTP_PORT), PV_UDP_PORT: String(UDP_PORT), PV_HMAC_SECRET: SECRET },
      stdio: 'ignore'
    });
    let up = false;
    for (let i = 0; i < 120 && !up; i++) {
      try { up = (await fetch(HTTP + '/')).ok; } catch {}
      if (!up) await new Promise(r => setTimeout(r, 500));
    }
    if (!up) throw new Error(`dev_relay が ${HTTP} で応答しない`);
    ok(`dev_relay が 127.0.0.1:${HTTP_PORT} で起動 (UDP ${UDP_PORT})`);

    const WS = `ws://127.0.0.1:${HTTP_PORT}/ws`;
    relayPage = await Cdp.open(`${base}/index.html`);
    for (let i = 0; i < 100; i++) {
      if (await relayPage.eval(`document.readyState === 'complete' && !!(window.PV && PV.Rtc)`)) break;
      await new Promise(r => setTimeout(r, 100));
    }

    // D-1. 名簿に載っていない相手は bye: not_eligible で断られる。
    //      #1-1 はリレー側から見た同じ経路を確認済みだが、**ブラウザ側がどう受け取るか**は未確認だった
    const denied = await relayPage.eval(`(async () => {
      const sig = new PV.Signal(${JSON.stringify(WS + '?steam_id=nobody')});
      const got = new Promise(res => { sig.on('bye', m => res(m.reason)); setTimeout(() => res('(来なかった)'), 12000); });
      sig.open();
      const reason = await got;
      return { reason, wantOpen: sig.wantOpen };
    })()`);
    denied.reason === 'not_eligible' ? ok('名簿に無い相手は bye: not_eligible で断られる') : bad('bye の reason', denied.reason);
    denied.wantOpen === false ? ok('not_eligible では signal.js が再接続を諦める') : bad('not_eligible なのに再接続しようとする');

    // D-2. 名簿に載せて 2 人ぶん繋ぐ
    const ELIGIBLE = ['alice', 'bob', 'carol', 'dave'];
    const pushed = await internal('/internal/roster', { eligible: ELIGIBLE });
    // 成功は 204 No Content (本文を返さない)。2xx なら通っている
    /^2\d\d/.test(pushed) ? ok(`POST /internal/roster (HMAC 付き) が通る (${pushed.trim()})`) : bad('roster push', pushed);
    // ROSTER_TTL_S = 10 で切られるので、試験中は push し続ける
    keepalive = setInterval(() => { internal('/internal/roster', { eligible: ELIGIBLE }).catch(() => {}); }, 2500);

    const setup = await relayPage.eval(`(async () => {
      window.__c = {};
      const WSBASE = ${JSON.stringify(WS)};
      const toneCtx = new AudioContext();
      const tone = (hz) => { const d = toneCtx.createMediaStreamDestination();
        const o = toneCtx.createOscillator(); o.frequency.value = hz;
        const g = toneCtx.createGain(); g.gain.value = 0.6; o.connect(g); g.connect(d); o.start();
        return d.stream.getAudioTracks()[0]; };
      const client = async (id, hz) => {
        const engine = new PV.AudioEngine(); await engine.start();
        const sig = new PV.Signal(WSBASE + '?steam_id=' + id);
        const rtc = new PV.Rtc(engine, sig);
        const peers = [], graphs = [], yaws = [], order = [];
        sig.on('ready', () => rtc.negotiate(tone(hz)).catch(e => peers.push({ err: String(e) })));
        sig.on('sdp_answer', m => rtc.onAnswer(m.sdp).then(() => rtc.setMicEnabled(true)));
        sig.on('ice', m => rtc.onIce(m.candidate));
        sig.on('peer', m => { order.push('peer'); peers.push({ mid: m.mid, id: m.id }); engine.setPeer(m.mid, m.id); });
        // app.js と同じ結線。**ここで applyGraph するのが唯一の音量の源**なので、
        // graph が来なければ何も鳴らない (fail closed)
        sig.on('graph', m => { order.push('graph'); graphs.push(m); engine.applyGraph(m.hears); });
        sig.on('yaw', m => { yaws.push(m.deg); engine.setYaw(m.deg); });
        sig.open();
        return { engine, sig, rtc, peers, graphs, yaws, order };
      };
      window.__client = client;
      window.__tone = tone;
      window.__c.alice = await client('alice', 440);
      window.__c.bob = await client('bob', 330);
      const wait = async (f, ms) => { const t0 = Date.now();
        while (Date.now() - t0 < ms) { if (f()) return true; await new Promise(r => setTimeout(r, 100)); } return false; };
      const conn = await wait(() => ['alice','bob'].every(k => window.__c[k].rtc.pc && window.__c[k].rtc.pc.connectionState === 'connected'), 25000);
      return { connected: conn,
               states: ['alice','bob'].map(k => window.__c[k].rtc.pc && window.__c[k].rtc.pc.connectionState),
               slotMids: window.__c.alice.rtc.slotMids.length,
               signaling: window.__c.alice.rtc.pc.signalingState };
    })()`);
    setup.connected ? ok('本物のリレー相手に 2 人とも PeerConnection が繋がる') : bad('繋がらない', JSON.stringify(setup.states));
    setup.slotMids === PV.SLOTS ? ok(`answer 直後に ${PV.SLOTS} 本のスロットを張れている (実物の answer で)`)
                                : bad('スロット数', String(setup.slotMids));

    // D-3. 購読前は 1 バイトも来ない
    const before = await relayPage.eval(`(async () => {
      let total = 0;
      const stats = await window.__c.alice.rtc.pc.getStats();
      stats.forEach(r => { if (r.type === 'inbound-rtp' && r.kind === 'audio') total += r.bytesReceived || 0; });
      return { bytes: total, peers: window.__c.alice.peers.length };
    })()`);
    before.bytes === 0 ? ok('購読前は 1 バイトも届かない') : bad('購読していないのに届いている', String(before.bytes));
    before.peers === 0 ? ok('購読前は peer が来ない') : bad('購読前に peer が来た', JSON.stringify(before.peers));

    // D-4. 購読させると peer が来て、実際に音が鳴る
    await dev('/dev/subscribe', { listener: 'alice', speakers: ['bob'] });
    const heard = await relayPage.eval(`(async () => {
      const a = window.__c.alice;
      const t0 = Date.now();
      while (Date.now() - t0 < 10000 && !a.peers.some(p => p.id === 'bob')) await new Promise(r => setTimeout(r, 100));
      const p = a.peers.find(x => x.id === 'bob');
      if (!p) return { assigned: false, peers: a.peers };
      a.engine.applyGraph([{ id: 'bob', d: 2, b: 90, sub: true }]);
      const slot = a.engine.slots[p.mid];
      let peak = 0;
      for (let i = 0; i < 60; i++) { await new Promise(r => setTimeout(r, 50));
        const b = new Float32Array(slot.analyser.fftSize); slot.analyser.getFloatTimeDomainData(b);
        let s = 0; for (const v of b) s += v * v; peak = Math.max(peak, Math.sqrt(s / b.length)); }
      return { assigned: true, mid: p.mid, rms: peak, pan: slot.pan.pan.value,
               muted: !!(slot.audioEl && slot.audioEl.muted),
               signaling: a.rtc.pc.signalingState, slotMids: a.rtc.slotMids.slice() };
    })()`);
    heard.assigned ? ok(`購読すると peer が来てスロットが割り当たる (mid "${heard.mid}")`)
                   : bad('peer が来ない', JSON.stringify(heard.peers));
    heard.rms > 0.02 ? ok(`実物のリレー越しに音が Web Audio へ届く (RMS ${heard.rms.toFixed(3)})`)
                     : bad('リレー越しの音が届かない', String(heard.rms));
    heard.muted ? ok('リモートトラックが muted な <audio> にもアタッチされている') : bad('罠回避の <audio> が無い');
    Math.abs(heard.pan - 1) < 0.05 ? ok(`定位が当たっている (pan ${heard.pan.toFixed(3)})`) : bad('pan', String(heard.pan));

    // D-5. mute_all: 転送だけ止まる。スロットも mid も PeerConnection も動かない
    const midsBefore = heard.slotMids;
    await dev('/dev/mute', { listener: 'alice' });
    const muted = await relayPage.eval(`(async () => {
      const a = window.__c.alice;
      const n = a.peers.length;
      await new Promise(r => setTimeout(r, 2500));
      const slot = a.engine.slots[${JSON.stringify(heard.mid)}];
      let peak = 0;
      for (let i = 0; i < 30; i++) { await new Promise(r => setTimeout(r, 50));
        const b = new Float32Array(slot.analyser.fftSize); slot.analyser.getFloatTimeDomainData(b);
        let s = 0; for (const v of b) s += v * v; peak = Math.max(peak, Math.sqrt(s / b.length)); }
      return { rms: peak, newPeers: a.peers.slice(n), conn: a.rtc.pc.connectionState,
               signaling: a.rtc.pc.signalingState, slotMids: a.rtc.slotMids.slice(),
               steamId: slot.steamId };
    })()`);
    muted.rms < 0.02 ? ok(`mute_all で音が止まる (RMS ${muted.rms.toFixed(4)})`) : bad('mute_all でも鳴っている', String(muted.rms));
    muted.newPeers.length === 0 ? ok('mute_all では Peer{id:null} が撒かれない (スロットは保持)')
                                : bad('mute_all でスロットが動いた', JSON.stringify(muted.newPeers));
    muted.steamId === 'bob' ? ok('mid ↔ SteamID の対応が保たれている') : bad('対応が外れた', String(muted.steamId));
    muted.conn === 'connected' && muted.signaling === 'stable' && JSON.stringify(muted.slotMids) === JSON.stringify(midsBefore)
      ? ok('mute_all で PeerConnection も mid も動かない (再ネゴシエーションなし)')
      : bad('mute_all で接続が動いた', `${muted.conn}/${muted.signaling}`);

    // D-6. 購読を戻すと鳴り出す (死亡 → リスポーンの形)
    await dev('/dev/subscribe', { listener: 'alice', speakers: ['bob'] });
    const back = await relayPage.eval(`(async () => {
      const a = window.__c.alice;
      const slot = a.engine.slots[${JSON.stringify(heard.mid)}];
      let peak = 0;
      for (let i = 0; i < 80; i++) { await new Promise(r => setTimeout(r, 50));
        const b = new Float32Array(slot.analyser.fftSize); slot.analyser.getFloatTimeDomainData(b);
        let s = 0; for (const v of b) s += v * v; peak = Math.max(peak, Math.sqrt(s / b.length)); }
      return { rms: peak, slotMids: a.rtc.slotMids.slice(), signaling: a.rtc.pc.signalingState };
    })()`);
    back.rms > 0.02 ? ok(`購読を戻すと同じスロットで鳴り出す (RMS ${back.rms.toFixed(3)})`)
                    : bad('戻しても鳴らない', String(back.rms));
    JSON.stringify(back.slotMids) === JSON.stringify(midsBefore) && back.signaling === 'stable'
      ? ok('一連の出入りで mid が一度も動いていない') : bad('mid が動いた');

    // D-7. **fail closed の回帰テスト。** peer が来てスロットが割り当たり、RTP まで
    //      流れていても、graph が 1 通も来ていない相手は鳴らしてはいけない
    //      (docs/protocol.md §0 — 「聞こえてはいけない音声が届かない」)。
    //      d 未知 = 無音 は意図した挙動であり、うっかり緩めないための歯止め。
    const carol = await relayPage.eval(`(async () => {
      window.__c.carol = await window.__client('carol', 520);
      const c = window.__c.carol;
      const t0 = Date.now();
      while (Date.now() - t0 < 25000 && !(c.rtc.pc && c.rtc.pc.connectionState === 'connected')) await new Promise(r => setTimeout(r, 100));
      return { conn: c.rtc.pc && c.rtc.pc.connectionState };
    })()`);
    carol.conn === 'connected' ? ok('carol が繋がる (graph は 1 通も push しない)') : bad('carol が繋がらない', String(carol.conn));

    await dev('/dev/subscribe', { listener: 'carol', speakers: ['bob'] });
    const closed = await relayPage.eval(`(async () => {
      const c = window.__c.carol;
      const t0 = Date.now();
      while (Date.now() - t0 < 10000 && !c.peers.some(p => p.id === 'bob')) await new Promise(r => setTimeout(r, 100));
      const p = c.peers.find(x => x.id === 'bob');
      if (!p) return { assigned: false };
      const slot = c.engine.slots[p.mid];
      let peak = 0;
      for (let i = 0; i < 60; i++) { await new Promise(r => setTimeout(r, 50));
        const b = new Float32Array(slot.analyser.fftSize); slot.analyser.getFloatTimeDomainData(b);
        let s = 0; for (const v of b) s += v * v; peak = Math.max(peak, Math.sqrt(s / b.length)); }
      let bytes = 0;
      (await c.rtc.pc.getStats()).forEach(r => { if (r.type === 'inbound-rtp' && r.kind === 'audio') bytes += r.bytesReceived || 0; });
      const heardIds = c.graphs.flatMap(g => (g.hears || []).map(h => h.id));
      return { assigned: true, rms: peak, gain: slot.gain.gain.value, d: slot.d,
               bytes, graphs: c.graphs.length, heardIds, speaking: c.engine.speaking().length };
    })()`);
    closed.assigned ? ok('carol にも peer が来てスロットが割り当たる') : bad('carol に peer が来ない');
    closed.bytes > 0 ? ok(`carol には RTP が届いている (${closed.bytes} bytes)`) : bad('RTP が届いていない', String(closed.bytes));
    // 接続時の resync で**空の** graph は届く。それは正しい (「今は誰も聞こえない」の通知)。
    // 要件は「bob が載った graph が来ていないのに鳴っていない」こと
    closed.heardIds.length === 0
      ? ok(`carol の graph に載った相手はゼロ (graph ${closed.graphs} 通、すべて空)`)
      : bad('聞こえるはずのない相手が graph に載った', closed.heardIds.join(','));
    closed.gain === 0 && closed.rms < 0.005
      ? ok('★ graph の無い相手は鳴らない (d 未知 = 無音。fail closed)')
      : bad('graph が無いのに鳴っている', `gain ${closed.gain} rms ${closed.rms}`);
    closed.speaking === 0 ? ok('graph の無い相手は「発話中」にも出ない') : bad('発話中に出ている');

    // D-8. **issue #11。** 名簿と graph を push して**静止させ**、その後で接続する。
    //      graph は「変化時のみ」なので、接続後には 1 通も飛んでこない。
    //      リレーが接続時に今の状態を撒かないと、この PWA は永久に無音のまま。
    await internal('/internal/yaw', { yaws: [['dave', 0]] });
    await internal('/internal/graph', {
      listeners: [{ id: 'dave', hears: [{ id: 'bob', d: 2, b: 90, sub: true }] }]
    });
    await new Promise(r => setTimeout(r, 500));   // ここから先、graph は一切 push しない (= 静止)
    const late = await relayPage.eval(`(async () => {
      window.__c.dave = await window.__client('dave', 610);
      const d = window.__c.dave;
      const t0 = Date.now();
      while (Date.now() - t0 < 25000 && !(d.rtc.pc && d.rtc.pc.connectionState === 'connected')) await new Promise(r => setTimeout(r, 100));
      // 接続後、graph / peer が来るのを待つ
      const t1 = Date.now();
      while (Date.now() - t1 < 8000 && !(d.graphs.length && d.peers.some(p => p.id === 'bob'))) await new Promise(r => setTimeout(r, 100));
      const p = d.peers.find(x => x.id === 'bob');
      let peak = 0, gain = null, pan = null;
      if (p) { const slot = d.engine.slots[p.mid];
        for (let i = 0; i < 60; i++) { await new Promise(r => setTimeout(r, 50));
          const b = new Float32Array(slot.analyser.fftSize); slot.analyser.getFloatTimeDomainData(b);
          let s = 0; for (const v of b) s += v * v; peak = Math.max(peak, Math.sqrt(s / b.length)); }
        gain = slot.gain.gain.value; pan = slot.pan.pan.value; }
      let bytes = 0;
      (await d.rtc.pc.getStats()).forEach(r => { if (r.type === 'inbound-rtp' && r.kind === 'audio') bytes += r.bytesReceived || 0; });
      return { conn: d.rtc.pc && d.rtc.pc.connectionState, graphs: d.graphs.length, yaws: d.yaws.length,
               heardIds: d.graphs.flatMap(g => (g.hears || []).map(h => h.id)),
               peer: !!p, peers: d.peers.slice(), bytes, rms: peak, gain, pan, order: d.order.slice() };
    })()`);
    late.conn === 'connected' ? ok('dave が繋がる') : bad('dave が繋がらない', String(late.conn));
    late.graphs > 0 ? ok(`★ 静止したまま接続しても graph が届く (${late.graphs} 通)`)
                    : bad('★ 途中参加に graph が来ない (issue #11)', '0 通 — 誰かが動くまで永久に無音');
    late.heardIds.includes('bob')
      ? ok(`graph の中身も正しい (hears に bob が載っている: ${late.heardIds.join(',')})`)
      : bad('graph は来たが bob が載っていない', late.heardIds.join(',') || '(空)');
    late.peer ? ok('接続時に peer が来てスロットが割り当たる')
              : bad('接続時に peer が来ない', `peers=${JSON.stringify(late.peers)} 受信 ${late.bytes} bytes — graph は届いているので購読 (SetSubscriptions) だけが効いていない`);
    late.yaws > 0 ? ok(`接続時に yaw も届く (${late.yaws} 通)`) : bad('接続時に yaw が来ない (issue #11)');
    late.rms > 0.02 ? ok(`★ 途中参加でも音が鳴る (RMS ${late.rms.toFixed(3)})`, `到着順: ${late.order.join(' → ')}`)
                    : bad('★ 途中参加が無音のまま (issue #11)', `gain ${late.gain} rms ${late.rms} 到着順: ${late.order.join(' → ')}`);
  } catch (e) {
    skip('リレー相手の実地確認', String(e && e.message || e));
  } finally {
    if (keepalive) clearInterval(keepalive);
    try { if (relay) relay.kill('SIGKILL'); } catch {}
  }
}

  // 3'. console
  const noise = [...cdp.logs, ...page.logs].filter(l => !/favicon/i.test(l));
  noise.length === 0 ? ok('console にエラー / 警告が無い (dev.html / index.html)')
                     : bad('console にエラーがある', '\n       ' + noise.join('\n       '));
} catch (e) {
  bad('ブラウザでの確認', String(e && e.message || e));
} finally {
  try { chrome.kill(); } catch {}
  server.close();
  await new Promise(r => setTimeout(r, 300));
  await rm(PROFILE, { recursive: true, force: true, maxRetries: 5 }).catch(() => {});
}

async function settleText(cdp) {
  for (let i = 0; i < 40; i++) {
    const t = await cdp.eval(`document.getElementById('state').textContent`);
    if (t !== '停止中') return t;
    await new Promise(r => setTimeout(r, 100));
  }
  return await cdp.eval(`document.getElementById('state').textContent`);
}

const failed = results.filter(r => !r[0]);
console.log(`\n${results.length - failed.length}/${results.length} ok` + (skipped.length ? ` / ${skipped.length} skip` : ''));
for (const [n, w] of skipped) console.log(`  skip: ${n} — ${w}`);
process.exit(failed.length ? 1 : 0);
