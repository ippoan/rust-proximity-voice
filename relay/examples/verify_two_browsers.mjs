// ブラウザ 2 枚で実際に音が通るかを、headless Chrome 2 台で確かめる (#1-1 の受け入れ条件)。
// マイクは Chrome の --use-fake-device-for-media-stream が出す合成音。
//
//   # 1. リレーを立てる
//   cargo run -p relay --example dev_relay
//
//   # 2. Chrome を 2 台
//   for p in 9222 9223; do
//     google-chrome --headless=new --remote-debugging-port=$p --user-data-dir=/tmp/pv-$p \
//       --no-sandbox --use-fake-device-for-media-stream --use-fake-ui-for-media-stream \
//       --autoplay-policy=no-user-gesture-required about:blank &
//   done
//
//   # 3. 検証
//   node relay/examples/verify_two_browsers.mjs
//
// 見ているもの:
//   0. 名簿に載っていないと /ws が bye:not_eligible で断ること
//   1. offer 側が 1 (mic) + 16 (slot) の m-line を並べ、answer がそれを受ける
//   2. 購読前は 1 バイトも届かない
//   3. 購読すると RTP が流れる
//   4. mute_all で止まり、**切断はされず**スロットも動かない
//   5. 再開で戻る
//   6. 話者が**行儀よく**閉じたらスロットがその場で解放され、聞き手は切れない
//   7. 話者が**行儀悪く**消えても (タブごと kill = DTLS の close すら送らない)
//      同じことが起きる。★ ここを見ていなかったせいで issue #5 を見逃した。
//      §0 が名指ししている「Alt-F4 / クラッシュ」はこちらであって 6 ではない
//   8. WS だけ閉じて WebRTC を開いたままにしても枠が解放される (issue #7)。
//      制御チャネルが無いセッションは正しく動けないので畳む。ICE の検出
//      (20〜25s) を待たないぶん、7 より明確に速い
//   9. どの段階でも mid が動かない (= 再ネゴシエーションが起きていない)

import { createHmac } from 'node:crypto';

const HTTP = process.env.PV_HTTP || 'http://localhost:8080';
const SECRET = process.env.PV_HMAC_SECRET || 'dev';
const SERVER_ID = 'dev';

class Cdp {
  constructor(ws) { this.ws = ws; this.id = 0; this.pending = new Map(); }
  static async attach(port, url) {
    // ページターゲットを作る
    const res = await fetch(`http://127.0.0.1:${port}/json/new?${encodeURIComponent(url)}`, { method: 'PUT' });
    const target = await res.json();
    const ws = new WebSocket(target.webSocketDebuggerUrl);
    await new Promise((ok, ng) => { ws.onopen = ok; ws.onerror = ng; });
    const cdp = new Cdp(ws);
    cdp.targetId = target.id;
    cdp.port = port;
    ws.onmessage = (e) => {
      const m = JSON.parse(e.data);
      if (m.id && cdp.pending.has(m.id)) { cdp.pending.get(m.id)(m); cdp.pending.delete(m.id); }
    };
    await cdp.send('Runtime.enable');
    return cdp;
  }
  send(method, params = {}) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise(res => this.pending.set(id, res));
  }
  /** ブラウザのタブごと落とす。DTLS の close も ICE の後始末も送らない。 */
  async killTarget(port, targetId) {
    await fetch(`http://127.0.0.1:${port}/json/close/${targetId}`);
  }

  async eval(expr) {
    const r = await this.send('Runtime.evaluate', {
      expression: expr, awaitPromise: true, returnByValue: true,
    });
    if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails));
    return r.result?.result?.value;
  }
}

const sleep = ms => new Promise(r => setTimeout(r, ms));

async function waitFor(fn, what, timeoutMs = 25000) {
  const t0 = Date.now();
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() - t0 > timeoutMs) throw new Error(`timeout: ${what}`);
    await sleep(300);
  }
}

async function post(path, body) {
  const r = await fetch(HTTP + path, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
  });
  return r.text();
}

/** プラグイン → リレー の署名付き push (docs/protocol.md §1)。 */
let seq = 1;
async function internal(path, body) {
  const payload = JSON.stringify({ server_id: SERVER_ID, seq: seq++, ts: Math.floor(Date.now() / 1000), ...body });
  const ts = String(Math.floor(Date.now() / 1000));
  const sig = createHmac('sha256', SECRET).update(`${ts}.${payload}`).digest('hex');
  const r = await fetch(HTTP + path, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'X-PV-Timestamp': ts, 'X-PV-Signature': sig },
    body: payload,
  });
  return `${r.status} ${await r.text()}`;
}

/** その聞き手が今 受け取っている inbound RTP の合計バイト数。 */
const RX_BYTES = `(async () => {
  if (typeof pc === 'undefined' || !pc) return -1;
  let total = 0;
  const stats = await pc.getStats();
  stats.forEach(r => { if (r.type === 'inbound-rtp' && r.kind === 'audio') total += r.bytesReceived || 0; });
  return total;
})()`;

/** 割り当てられているスロットの一覧を表から読む。 */
const SLOT_TABLE = `Array.from(document.querySelectorAll('#slots tr')).map(tr => ({
  mid: tr.children[0].textContent, who: tr.children[1].textContent
})).filter(s => s.who !== '(空き)')`;

const [portA, portB] = [9222, 9223];

// --- 0. 名簿に載せる。載せる前は /ws が断ることも見る --------------------------
const denied = await Cdp.attach(portA, `${HTTP}/?steam_id=nobody`);
await denied.eval(`document.getElementById('connect').click(); 1`);
const byeLog = await waitFor(async () => {
  const t = await denied.eval(`document.getElementById('log').textContent`);
  return t.includes('bye:') ? t : null;
}, '名簿に無い相手が bye で断られる', 15000);
console.log('名簿外:', byeLog.trim().split('\n').filter(l => l.startsWith('bye:'))[0]);
if (!byeLog.includes('not_eligible')) throw new Error('bye の reason が not_eligible でない');

console.log('POST /internal/roster →', await internal('/internal/roster', { eligible: ['alice', 'bob'] }));

const alice = await Cdp.attach(portA, `${HTTP}/?steam_id=alice`);
const bob = await Cdp.attach(portB, `${HTTP}/?steam_id=bob`);
console.log('両ブラウザでページを開いた');

await sleep(1000);
await alice.eval(`document.getElementById('connect').click(); 1`);
await bob.eval(`document.getElementById('connect').click(); 1`);

for (const [name, cdp] of [['alice', alice], ['bob', bob]]) {
  await waitFor(() => cdp.eval(`typeof pc !== 'undefined' && !!pc && pc.connectionState === 'connected'`),
                `${name} の PeerConnection が connected になる`);
  console.log(`${name}: PeerConnection connected`);
}

// 張られた m-line の本数と向きを確認する (offer 側が並べた 1 + 16)
const shape = await alice.eval(`(() => {
  const ts = pc.getTransceivers();
  return {
    total: ts.length,
    send: ts.filter(t => t.currentDirection === 'sendonly').length,
    recv: ts.filter(t => t.currentDirection === 'recvonly').length,
    mids: ts.map(t => t.mid),
  };
})()`);
console.log('alice の transceiver:', JSON.stringify(shape));

const negotiations = async (cdp) => cdp.eval(`pc.localDescription.sdp.split('\\nm=').length - 1`);
console.log('alice の m-line 数:', await negotiations(alice));

// --- 1. 購読前は 1 バイトも届かない -----------------------------------------
await sleep(1500);
const before = await alice.eval(RX_BYTES);
console.log('購読前 alice の受信バイト:', before);

// --- 2. alice が bob を購読する ---------------------------------------------
console.log('POST /dev/subscribe →', await post('/dev/subscribe', { listener: 'alice', speakers: ['bob'] }));
const assigned = await waitFor(async () => {
  const rows = await alice.eval(SLOT_TABLE);
  return rows.length ? rows : null;
}, 'alice のスロットに bob が入る');
console.log('alice のスロット:', JSON.stringify(assigned));

// --- 3. 実際に RTP が流れる --------------------------------------------------
const t0 = await alice.eval(RX_BYTES);
await sleep(3000);
const t1 = await alice.eval(RX_BYTES);
console.log(`購読後の受信バイト: ${t0} → ${t1} (差 ${t1 - t0})`);
if (t1 - t0 < 1000) throw new Error('音が通っていない (RTP が届いていない)');

// 再ネゴシエーションが起きていないこと
const sigState = await alice.eval(`pc.signalingState`);
const mids2 = await alice.eval(`pc.getTransceivers().map(t => t.mid)`);
console.log('signalingState:', sigState, '/ mid は不変か:', JSON.stringify(mids2) === JSON.stringify(shape.mids));

// --- 4. mute_all で止まる (切断はしない) -------------------------------------
console.log('POST /dev/mute →', await post('/dev/mute', { listener: 'alice' }));
await sleep(1200);
const m0 = await alice.eval(RX_BYTES);
await sleep(2500);
const m1 = await alice.eval(RX_BYTES);
const stillConnected = await alice.eval(`pc.connectionState`);
console.log(`mute 後の受信バイト: ${m0} → ${m1} (差 ${m1 - m0}) / pc: ${stillConnected}`);
if (m1 - m0 > 500) throw new Error('mute_all のあとも RTP が流れている');
if (stillConnected !== 'connected') throw new Error('mute_all が切断してしまった');

// スロットの割り当ては保たれているか
const afterMute = await alice.eval(SLOT_TABLE);
console.log('mute 後のスロット:', JSON.stringify(afterMute));
if (JSON.stringify(afterMute) !== JSON.stringify(assigned)) throw new Error('mute でスロットが動いた');

// --- 5. 再開 -----------------------------------------------------------------
console.log('POST /dev/subscribe (再開) →', await post('/dev/subscribe', { listener: 'alice', speakers: ['bob'] }));
const r0 = await alice.eval(RX_BYTES);
await sleep(3000);
const r1 = await alice.eval(RX_BYTES);
console.log(`再開後の受信バイト: ${r0} → ${r1} (差 ${r1 - r0})`);
if (r1 - r0 < 1000) throw new Error('mute の解除で音が戻らない');

const finalMids = await alice.eval(`pc.getTransceivers().map(t => t.mid)`);
if (JSON.stringify(finalMids) !== JSON.stringify(shape.mids)) throw new Error('mid が動いた');

// --- 6. 話者が落ちたら、聞き手のスロットがその場で解放される -------------------
// docs/protocol.md §0「切断 → 即座に転送停止」。graph は 2 Hz でしか来ないので、
// 次の SetSubscriptions を待たずに解放されることを確かめる。
await bob.eval(`ws.close(); pc.close(); 1`);
const freed = await waitFor(async () => {
  const rows = await alice.eval(SLOT_TABLE);
  return rows.length === 0 ? true : null;
}, 'bob のスロットが解放される', 20000);
console.log('bob の切断でスロットが解放された:', freed);

const d0 = await alice.eval(RX_BYTES);
await sleep(2500);
const d1 = await alice.eval(RX_BYTES);
console.log(`切断後の受信バイト: ${d0} → ${d1} (差 ${d1 - d0})`);
if (d1 - d0 > 500) throw new Error('切断したのに RTP が流れている');

// alice 自身は切れていない
const aliceState = await alice.eval(`pc.connectionState`);
console.log('alice の pc:', aliceState);
if (aliceState !== 'connected') throw new Error('相手の切断で alice まで切れた');

// --- 7. ★ 行儀の悪い切断 (issue #5) -----------------------------------------
// タブごと落とすと DTLS の close も ICE の後始末も飛ばない。str0m は
// `Disconnected` を出すだけで自分では閉じないので、サーバーが自分で
// 「転送停止 → 猶予後に回収」をやらないとセッションが永久に残る。
console.log('\n--- 行儀の悪い切断 (タブごと kill) ---');
console.log('POST /internal/roster →', await internal('/internal/roster', { eligible: ['alice', 'carol'] }));

const carol = await Cdp.attach(portB, `${HTTP}/?steam_id=carol`);
await carol.eval(`document.getElementById('connect').click(); 1`);
await waitFor(() => carol.eval(`typeof pc !== 'undefined' && !!pc && pc.connectionState === 'connected'`),
              'carol が connected になる');
console.log('POST /dev/subscribe →', await post('/dev/subscribe', { listener: 'alice', speakers: ['carol'] }));

const c0 = await alice.eval(RX_BYTES);
await sleep(2500);
const c1 = await alice.eval(RX_BYTES);
console.log(`carol からの受信バイト: ${c0} → ${c1} (差 ${c1 - c0})`);
if (c1 - c0 < 1000) throw new Error('carol の音が通っていない');

// ★ ここが本番。close() を呼ばずにタブごと消す
console.log('carol のタブを kill する (pc.close() も ws.close() も呼ばない)');
await carol.killTarget(carol.port, carol.targetId);

// 「即座に転送停止」— 猶予 (60s) を待たずに枠が解放されること
const t = Date.now();
await waitFor(async () => {
  const rows = await alice.eval(SLOT_TABLE);
  return rows.length === 0 ? true : null;
}, '行儀の悪い切断でも枠が解放される', 40000);
console.log(`枠が解放されるまで ${((Date.now() - t) / 1000).toFixed(1)}s (猶予 60s を待っていない)`);

const k0 = await alice.eval(RX_BYTES);
await sleep(3000);
const k1 = await alice.eval(RX_BYTES);
console.log(`kill 後の受信バイト: ${k0} → ${k1} (差 ${k1 - k0})`);
if (k1 - k0 > 500) throw new Error('タブを kill したのに RTP が流れている');

const aliceAfterKill = await alice.eval(`pc.connectionState`);
const midsAfterKill = await alice.eval(`pc.getTransceivers().map(t => t.mid)`);
console.log('alice の pc:', aliceAfterKill);
if (aliceAfterKill !== 'connected') throw new Error('相手を kill したら alice まで切れた');
if (JSON.stringify(midsAfterKill) !== JSON.stringify(shape.mids)) throw new Error('mid が動いた');

// --- 8. ★ WS だけ閉じて WebRTC は開いたまま (issue #7) ------------------------
// 制御チャネルが死ぬと `Peer` が届かなくなり、クライアントの mid ↔ SteamID 対応が
// 凍る。それでもサーバーが割り当てを動かすと「A の声が B の名前で鳴る」。
// WebRTC が生きているので ICE は落ちない = 7 の経路では永久に回収されない。
console.log('\n--- WS だけ閉じる (WebRTC は開いたまま) ---');
console.log('POST /internal/roster →', await internal('/internal/roster', { eligible: ['alice', 'dave'] }));

const dave = await Cdp.attach(portB, `${HTTP}/?steam_id=dave`);
await dave.eval(`document.getElementById('connect').click(); 1`);
await waitFor(() => dave.eval(`typeof pc !== 'undefined' && !!pc && pc.connectionState === 'connected'`),
              'dave が connected になる');
console.log('POST /dev/subscribe →', await post('/dev/subscribe', { listener: 'alice', speakers: ['dave'] }));

const v0 = await alice.eval(RX_BYTES);
await sleep(2500);
const v1 = await alice.eval(RX_BYTES);
console.log(`dave からの受信バイト: ${v0} → ${v1} (差 ${v1 - v0})`);
if (v1 - v0 < 1000) throw new Error('dave の音が通っていない');

// ★ WS だけ閉じる。pc は触らない
console.log('dave の ws を閉じる (pc.close() は呼ばない)');
await dave.eval(`ws.close(); 1`);
const stillOpen = await dave.eval(`pc.connectionState`);
console.log('dave の pc は開いたまま:', stillOpen);
if (stillOpen !== 'connected') throw new Error('pc まで閉じてしまった。シナリオが成立していない');

const tWs = Date.now();
await waitFor(async () => {
  const rows = await alice.eval(SLOT_TABLE);
  return rows.length === 0 ? true : null;
}, 'WS の切断で枠が解放される', 30000);
const wsSecs = (Date.now() - tWs) / 1000;
console.log(`枠が解放されるまで ${wsSecs.toFixed(1)}s`);
// 受け入れ条件: ICE 待ち (20〜25s) より明確に短いこと
if (wsSecs > 10) throw new Error(`ICE 待ちと変わらない (${wsSecs.toFixed(1)}s)。WS を合図に使えていない`);

const w0 = await alice.eval(RX_BYTES);
await sleep(3000);
const w1 = await alice.eval(RX_BYTES);
console.log(`WS 切断後の受信バイト: ${w0} → ${w1} (差 ${w1 - w0})`);
if (w1 - w0 > 500) throw new Error('WS を閉じたのに RTP が流れている');

const aliceEnd = await alice.eval(`pc.connectionState`);
const midsEnd = await alice.eval(`pc.getTransceivers().map(t => t.mid)`);
console.log('alice の pc:', aliceEnd);
if (aliceEnd !== 'connected') throw new Error('相手の WS が落ちて alice まで切れた');
if (JSON.stringify(midsEnd) !== JSON.stringify(shape.mids)) throw new Error('mid が動いた');

console.log('\n✅ 名簿外は断られ、音が通り、mute で止まり、再開で戻り、');
console.log('   行儀よく閉じても・行儀悪く消えても・WS だけ落ちても枠が解放され、');
console.log('   mid は一度も動かなかった');
process.exit(0);
