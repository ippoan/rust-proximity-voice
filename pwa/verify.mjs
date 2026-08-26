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
import { fileURLToPath } from 'node:url';
import { dirname, join, normalize } from 'node:path';

const ROOT = dirname(fileURLToPath(import.meta.url));
const results = [];
const ok = (name, extra = '') => { results.push([true, name, extra]); console.log(`  ok   ${name}${extra ? '  ' + extra : ''}`); };
const bad = (name, extra = '') => { results.push([false, name, extra]); console.log(`  FAIL ${name}${extra ? '  ' + extra : ''}`); };
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
console.log(`\n${results.length - failed.length}/${results.length} ok`);
process.exit(failed.length ? 1 : 0);
