// plugin/ProximityVoice.cs が送るのと **同一の JSON と HMAC ヘッダ**を作って、
// 実物のリレーに当てる (#1-4 の受け入れ条件)。
//
// このマシンには dotnet が無く C# をコンパイルできない。Oxide プラグインは Rust
// サーバーがロード時にコンパイルするので運用上はそれで構わないが、代わりに
// **ワイヤ形式を機械で検証できなくなる**。ここがその代わり。
//
//   # 1. リレーを立てる
//   cargo run -p relay --example dev_relay
//
//   # 2. 検証
//   node plugin/verify_wire.mjs
//
// 見ているもの:
//   A. プラグインが自前で持っている SHA-256 / HMAC-SHA256 が正しいこと。
//      C# の実装をそのまま JS に写して node:crypto と突き合わせる
//      (Oxide の制限モードで System.Security が使えるか分からないので自前実装にした。
//       コンパイルできない以上、算術が合っていることは別の手段で示すしかない)
//   B. 4 endpoint すべてに正しい署名で投げて **204** が返ること
//   C. 署名を 1 バイト変えると **401**
//   D. ts を 31 秒ずらすと **401** (HMAC_SKEW_S = 30)
//   E. seq を巻き戻すと弾かれること
//   F. ★ seq を endpoint ごとに独立させないと 401 が出ること。
//      仕様の意図がここにある (docs/protocol.md §1)
//   G. 送信 JSON に絶対座標のキーが 1 つも無いこと
//
// 終了コードは 0 = 全部通った / 1 = どれか落ちた。

import { createHmac, createHash } from 'node:crypto';

const HTTP = process.env.PV_HTTP || 'http://localhost:8080';
const SECRET = process.env.PV_HMAC_SECRET || 'dev';

// ---------------------------------------------------------------------------
// テストの土台
// ---------------------------------------------------------------------------

let failed = 0;
const results = [];

function check(name, ok, detail = '') {
  results.push({ name, ok, detail });
  if (!ok) failed++;
  const mark = ok ? '  ok  ' : ' FAIL ';
  console.log(`[${mark}] ${name}${detail ? '  — ' + detail : ''}`);
}

function section(title) {
  console.log(`\n=== ${title} ===`);
}

// ---------------------------------------------------------------------------
// A. プラグインの HMAC-SHA256 を JS に写したもの
//
// ProximityVoice.cs の `private static class Hmac` を 1 行ずつ移した。
// 変数名も向こうに揃えてある。ここが node:crypto と一致すれば、C# 側の算術も
// 同じ値を出す (どちらも uint32 のラップアラウンド算術しか使っていない)。
// ---------------------------------------------------------------------------

const K = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

const ror = (x, n) => ((x >>> n) | (x << (32 - n))) >>> 0;
const add = (...xs) => xs.reduce((a, b) => (a + b) >>> 0, 0);

/** ProximityVoice.Hmac.Sha256 と同じ手順。 */
function sha256(msg) {
  let h0 = 0x6a09e667, h1 = 0xbb67ae85, h2 = 0x3c6ef372, h3 = 0xa54ff53a;
  let h4 = 0x510e527f, h5 = 0x9b05688c, h6 = 0x1f83d9ab, h7 = 0x5be0cd19;

  const bitLen = BigInt(msg.length) * 8n;
  const padded = Math.floor((msg.length + 9 + 63) / 64) * 64;
  const m = new Uint8Array(padded);
  m.set(msg, 0);
  m[msg.length] = 0x80;
  for (let i = 0; i < 8; i++) m[padded - 1 - i] = Number((bitLen >> BigInt(8 * i)) & 0xffn);

  const w = new Uint32Array(64);
  for (let off = 0; off < padded; off += 64) {
    for (let i = 0; i < 16; i++) {
      const j = off + i * 4;
      w[i] = ((m[j] << 24) | (m[j + 1] << 16) | (m[j + 2] << 8) | m[j + 3]) >>> 0;
    }
    for (let i = 16; i < 64; i++) {
      const s0 = (ror(w[i - 15], 7) ^ ror(w[i - 15], 18) ^ (w[i - 15] >>> 3)) >>> 0;
      const s1 = (ror(w[i - 2], 17) ^ ror(w[i - 2], 19) ^ (w[i - 2] >>> 10)) >>> 0;
      w[i] = add(w[i - 16], s0, w[i - 7], s1);
    }

    let a = h0, b = h1, c = h2, d = h3, e = h4, f = h5, g = h6, hh = h7;
    for (let i = 0; i < 64; i++) {
      const bigS1 = (ror(e, 6) ^ ror(e, 11) ^ ror(e, 25)) >>> 0;
      const ch = ((e & f) ^ (~e & g)) >>> 0;
      const t1 = add(hh, bigS1, ch, K[i], w[i]);
      const bigS0 = (ror(a, 2) ^ ror(a, 13) ^ ror(a, 22)) >>> 0;
      const maj = ((a & b) ^ (a & c) ^ (b & c)) >>> 0;
      const t2 = add(bigS0, maj);
      hh = g; g = f; f = e;
      e = add(d, t1);
      d = c; c = b; b = a;
      a = add(t1, t2);
    }

    h0 = add(h0, a); h1 = add(h1, b); h2 = add(h2, c); h3 = add(h3, d);
    h4 = add(h4, e); h5 = add(h5, f); h6 = add(h6, g); h7 = add(h7, hh);
  }

  const out = new Uint8Array(32);
  const hs = [h0, h1, h2, h3, h4, h5, h6, h7];
  for (let i = 0; i < 8; i++) {
    out[i * 4] = (hs[i] >>> 24) & 0xff;
    out[i * 4 + 1] = (hs[i] >>> 16) & 0xff;
    out[i * 4 + 2] = (hs[i] >>> 8) & 0xff;
    out[i * 4 + 3] = hs[i] & 0xff;
  }
  return out;
}

/** ProximityVoice.Hmac.HmacSha256 と同じ手順。 */
function hmacSha256(key, data) {
  const BLOCK = 64;
  if (key.length > BLOCK) key = sha256(key);

  const k = new Uint8Array(BLOCK);
  k.set(key, 0);

  const inner = new Uint8Array(BLOCK + data.length);
  for (let i = 0; i < BLOCK; i++) inner[i] = k[i] ^ 0x36;
  inner.set(data, BLOCK);
  const innerHash = sha256(inner);

  const outer = new Uint8Array(BLOCK + 32);
  for (let i = 0; i < BLOCK; i++) outer[i] = k[i] ^ 0x5c;
  outer.set(innerHash, BLOCK);
  return sha256(outer);
}

const hex = (bytes) => Buffer.from(bytes).toString('hex');
const utf8 = (s) => new Uint8Array(Buffer.from(s, 'utf8'));

/** ProximityVoice.Hmac.HexSign と同じ。署名対象は `timestamp + "." + body`。 */
function pluginSign(secret, timestamp, body) {
  return hex(hmacSha256(utf8(secret), utf8(timestamp + '.' + body)));
}

function verifyCrypto() {
  section('A. プラグインが自前で持つ SHA-256 / HMAC が正しいか');

  // SHA-256 の既知ベクタ + 境界 (55 / 56 / 64 バイトでパディングのブロック数が変わる)
  const vectors = [
    '',
    'abc',
    'a'.repeat(55),
    'a'.repeat(56),
    'a'.repeat(64),
    'a'.repeat(1000),
    '{"server_id":"main","seq":1756180000123,"ts":1756180000,"eligible":["76561198000000001"]}',
  ];
  let allOk = true;
  for (const v of vectors) {
    const mine = hex(sha256(utf8(v)));
    const ref = createHash('sha256').update(v, 'utf8').digest('hex');
    if (mine !== ref) { allOk = false; console.log(`      sha256 mismatch len=${v.length}`); }
  }
  check('SHA-256 が node:crypto と一致する', allOk, `${vectors.length} 本のベクタ`);

  // HMAC。鍵がブロック長 (64B) を超えると先に SHA-256 で潰す分岐がある。
  const keys = ['', 'dev', 'x'.repeat(63), 'x'.repeat(64), 'x'.repeat(65), 'x'.repeat(200)];
  let hmacOk = true;
  for (const key of keys) {
    for (const body of ['', 'abc', 'a'.repeat(500)]) {
      const mine = hex(hmacSha256(utf8(key), utf8(body)));
      const ref = createHmac('sha256', Buffer.from(key, 'utf8')).update(body, 'utf8').digest('hex');
      if (mine !== ref) { hmacOk = false; console.log(`      hmac mismatch key=${key.length} body=${body.length}`); }
    }
  }
  check('HMAC-SHA256 が node:crypto と一致する (鍵長 0/63/64/65/200 を含む)', hmacOk);

  // 署名対象の組み立て方 (timestamp + "." + body) が relay/src/auth.rs::sign_raw と同じか。
  const ts = '1756180000';
  const body = '{"server_id":"main","seq":7,"ts":1756180000,"eligible":[]}';
  const mine = pluginSign(SECRET, ts, body);
  const ref = createHmac('sha256', SECRET).update(`${ts}.${body}`).digest('hex');
  check('署名対象が timestamp + "." + body になっている', mine === ref);
}

// ---------------------------------------------------------------------------
// B. プラグインが作るのと同じ本文
//
// ProximityVoice.Channel.SendState は本文をこの順で組み立てる:
//   {"server_id":<json>,"seq":<n>,"ts":<n>,"<key>":<fragment>}
// talk だけは Newtonsoft の匿名型なので server_id, seq, ts, id, talking の順。
// serde 的には順序は関係ないが、**署名するのはこの文字列そのもの**なので、
// ここで実物と同じ組み立てを再現しておく意味がある。
// ---------------------------------------------------------------------------

// リレーの seq guard はプロセスが生きているあいだ (server_id, endpoint) ごとの
// 最終 seq を覚えている。同じ server_id を使い回すと 2 回目の実行が巻き戻し扱いに
// なるので、**実行ごとに別の server_id** にする。
const RUN = Date.now().toString(36);

const A = '76561198000000001';
const B = '76561198000000042';
const C = '76561198000000077';

function envelope(serverId, seq, ts, key, fragment) {
  return `{"server_id":${JSON.stringify(serverId)},"seq":${seq},"ts":${ts},"${key}":${fragment}}`;
}

function rosterBody(serverId, seq, ts, ids) {
  return envelope(serverId, seq, ts, 'eligible', JSON.stringify(ids));
}

/** GraphTick が組む listeners。`hears` は **距離の近い順**、同距離は SteamID 順。 */
function graphBody(serverId, seq, ts, listeners) {
  const frag = JSON.stringify(
    listeners.map(([id, hears]) => ({
      id,
      hears: hears
        .slice()
        .sort((x, y) => x.d - y.d || (x.id < y.id ? -1 : x.id > y.id ? 1 : 0))
        .map((h) => ({ id: h.id, d: h.d, b: h.b, sub: h.d <= 75 })),
    })),
  );
  return envelope(serverId, seq, ts, 'listeners', frag);
}

function yawBody(serverId, seq, ts, yaws) {
  return envelope(serverId, seq, ts, 'yaws', JSON.stringify(yaws));
}

function talkBody(serverId, seq, ts, id, talking) {
  return JSON.stringify({ server_id: serverId, seq, ts, id, talking });
}

// ---------------------------------------------------------------------------
// 送信
// ---------------------------------------------------------------------------

const now = () => Math.floor(Date.now() / 1000);

async function post(path, body, { ts = now(), tamper = false } = {}) {
  const tsStr = String(ts);
  let sig = pluginSign(SECRET, tsStr, body);
  if (tamper) {
    // 1 バイトだけ変える (16 進の 1 文字を別の文字に)
    const ch = sig[0] === '0' ? '1' : '0';
    sig = ch + sig.slice(1);
  }
  const r = await fetch(HTTP + path, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'X-PV-Timestamp': tsStr,
      'X-PV-Signature': sig,
    },
    body,
  });
  return r.status;
}

// ---------------------------------------------------------------------------
// G. 絶対座標のキーが無いこと
// ---------------------------------------------------------------------------

const FORBIDDEN = /"(pos|position|coord|coords|world_pos)"/;

function assertNoCoords(label, bodies) {
  const hit = bodies.find((b) => FORBIDDEN.test(b));
  check(`送信 JSON に絶対座標のキーが無い (${label})`, !hit, hit ? `見つかった: ${hit.slice(0, 120)}` : '');
}

// ---------------------------------------------------------------------------
// 本体
// ---------------------------------------------------------------------------

async function main() {
  verifyCrypto();

  // リレーが居るか
  let up = false;
  try {
    const r = await fetch(HTTP + '/internal/roster', { method: 'POST', body: '{}' });
    up = r.status !== undefined;
  } catch (e) {
    up = false;
  }
  if (!up) {
    console.log(`\nリレー (${HTTP}) に繋がらない。先に:\n  cargo run -p relay --example dev_relay\n`);
    process.exit(1);
  }

  // ---- B. 4 endpoint に正しい署名 → 204 ----
  section('B. 4 endpoint すべてが正しい署名で 204 を返すか');

  const SID = `wire-${RUN}`;
  // seq はプラグインと同じく **unix ミリ秒**を種にする。endpoint ごとに独立。
  const seed = Date.now();
  const seq = { roster: seed, graph: seed, yaw: seed, talk: seed };

  const bodies = [];

  // ★ roster を先に送る。リレーは roster 未受信の server_id の graph / yaw を捨てる。
  let body = rosterBody(SID, ++seq.roster, now(), [A, B, C]);
  bodies.push(body);
  check('POST /internal/roster → 204', (await post('/internal/roster', body)) === 204);

  body = graphBody(SID, ++seq.graph, now(), [
    [A, [{ id: C, d: 61, b: 340 }, { id: B, d: 23, b: 145 }]],
    [B, [{ id: A, d: 23, b: 325 }]],
    [C, []], // 誰にも聞こえなくなった聞き手も必ず載せる (載せないと古い購読が残る)
  ]);
  bodies.push(body);
  check('POST /internal/graph → 204', (await post('/internal/graph', body)) === 204);

  body = yawBody(SID, ++seq.yaw, now(), [[A, 145], [B, 200], [C, 0]]);
  bodies.push(body);
  check('POST /internal/yaw → 204', (await post('/internal/yaw', body)) === 204);

  body = talkBody(SID, ++seq.talk, now(), A, true);
  bodies.push(body);
  check('POST /internal/talk → 204', (await post('/internal/talk', body)) === 204);

  // hears が距離の近い順に並んでいること (リレーは先頭 SLOTS=16 件に切り詰めるだけ)
  const g = JSON.parse(bodies[1]);
  const ds = g.listeners[0].hears.map((h) => h.d);
  check(
    'graph の hears が距離の近い順に並んでいる',
    ds.every((d, i) => i === 0 || ds[i - 1] <= d),
    JSON.stringify(ds),
  );

  assertNoCoords('B で送った 4 本', bodies);

  // ---- C. 署名を 1 バイト変える → 401 ----
  section('C/D/E. 3 点の検証が 1 つでも欠けたら 401');

  body = yawBody(SID, ++seq.yaw, now(), [[A, 10]]);
  check('署名を 1 バイト変えると 401', (await post('/internal/yaw', body, { tamper: true })) === 401);

  // ---- D. ts を 31 秒ずらす → 401 (HMAC_SKEW_S = 30) ----
  body = yawBody(SID, ++seq.yaw, now() - 31, [[A, 11]]);
  check('ts を 31 秒 過去にずらすと 401', (await post('/internal/yaw', body, { ts: now() - 31 })) === 401);

  body = yawBody(SID, ++seq.yaw, now() + 31, [[A, 12]]);
  check('ts を 31 秒 未来にずらすと 401', (await post('/internal/yaw', body, { ts: now() + 31 })) === 401);

  // 境界の内側 (29 秒) は通る = 「ずれ自体を弾いている」のであって別の理由ではないこと
  body = yawBody(SID, ++seq.yaw, now() - 29, [[A, 13]]);
  check('ts を 29 秒ずらすのは通る (204)', (await post('/internal/yaw', body, { ts: now() - 29 })) === 204);

  // ---- E. seq を巻き戻す → 401 ----
  const RB = `rollback-${RUN}`;
  body = rosterBody(RB, 1000, now(), [A]);
  check('巻き戻し用の server_id で roster seq=1000 → 204', (await post('/internal/roster', body)) === 204);

  body = rosterBody(RB, 999, now(), [A]);
  check('seq を 1000 → 999 に巻き戻すと 401', (await post('/internal/roster', body)) === 401);

  body = rosterBody(RB, 1000, now(), [A]);
  check('同じ seq の再送 (1000) も 401', (await post('/internal/roster', body)) === 401);

  body = rosterBody(RB, 1001, now(), [A]);
  check('seq を進めれば通る (1001 → 204)', (await post('/internal/roster', body)) === 204);

  // ---- F. ★ seq を endpoint ごとに独立させないと 401 が出る ----
  section('F. ★ seq は (server_id, endpoint) ごとに独立していないと 401 が出る');

  // F-1. 正しいやり方。endpoint ごとに独立した counter なので、20Hz の yaw が
  //      0.5Hz の roster をどれだけ追い越しても、互いに干渉しない。
  const IND = `independent-${RUN}`;
  const ind = { roster: 500, graph: 500, yaw: 500, talk: 500 };
  let indOk = (await post('/internal/roster', rosterBody(IND, ++ind.roster, now(), [A, B]))) === 204;

  // yaw だけを 40 回進める (20Hz が 2 秒ぶん走ったのに相当)
  for (let i = 0; i < 40; i++) {
    const st = await post('/internal/yaw', yawBody(IND, ++ind.yaw, now(), [[A, i * 9 % 360]]));
    if (st !== 204) indOk = false;
  }
  // その後に roster が来ても、roster の counter は 502 のままでよい
  if ((await post('/internal/roster', rosterBody(IND, ++ind.roster, now(), [A, B]))) !== 204) indOk = false;
  if ((await post('/internal/graph', graphBody(IND, ++ind.graph, now(), [[A, [{ id: B, d: 5, b: 90 }]]]))) !== 204) indOk = false;
  if ((await post('/internal/talk', talkBody(IND, ++ind.talk, now(), A, true))) !== 204) indOk = false;

  check(
    'endpoint ごとに独立した counter なら、yaw が roster を 40 も追い越しても全部 204',
    indOk,
    `roster=${ind.roster} yaw=${ind.yaw}`,
  );

  // F-2. 1 本の counter で見張るとどうなるか。
  //
  //   リレー側の guard の鍵は `"<endpoint>:<server_id>"` (relay/src/auth.rs の
  //   SeqGuard::accept_stream)。**これを `"<server_id>"` だけにすると**、
  //   4 本の独立した counter が 1 つの鍵に混ざる。その状態を、実物の guard 1 本
  //   (roster:collapsed) に同じ数列を流して再現する。
  //   roster=0.5Hz / yaw=20Hz なので、yaw が進むたびに roster が巻き戻り扱いされる。
  const CO = `collapsed-${RUN}`;
  const co = { roster: 8412, yaw: 55120 }; // docs/protocol.md §1 の例と同じ桁感
  let rejected = 0;
  let accepted = 0;
  const interleaved = [];
  for (let i = 0; i < 6; i++) {
    interleaved.push(++co.yaw);    // 20Hz の stream
    interleaved.push(++co.roster); // 0.5Hz の stream
  }
  for (const s of interleaved) {
    const st = await post('/internal/roster', rosterBody(CO, s, now(), [A]));
    if (st === 401) rejected++;
    else if (st === 204) accepted++;
  }
  check(
    '2 本の counter を 1 つの guard に混ぜると 401 が出続ける',
    rejected >= 6,
    `${interleaved.length} 通中 401=${rejected} / 204=${accepted} (roster 側が毎回 yaw に巻き戻し扱いされる)`,
  );

  // F-3. プラグイン再起動で counter が 0 に戻ると同じことが起きる。
  //      ProximityVoice.cs が seq の種に unix ミリ秒を使っているのはこれを避けるため。
  const RS = `restart-${RUN}`;
  check(
    '再起動前: seq に unix ミリ秒を使う → 204',
    (await post('/internal/roster', rosterBody(RS, Date.now(), now(), [A]))) === 204,
  );
  check(
    '再起動で counter が 1 に戻ると 401 (だから種に壁時計を使う)',
    (await post('/internal/roster', rosterBody(RS, 1, now(), [A]))) === 401,
  );
  check(
    '種が壁時計なら再起動後も前回より大きい → 204',
    (await post('/internal/roster', rosterBody(RS, Date.now() + 1, now(), [A]))) === 204,
  );

  // ---- 締め ----
  section('結果');
  console.log(`${results.length - failed} / ${results.length} 通過`);
  if (failed) {
    console.log('落ちたもの:');
    for (const r of results) if (!r.ok) console.log(`  - ${r.name}`);
  }
  process.exit(failed ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
