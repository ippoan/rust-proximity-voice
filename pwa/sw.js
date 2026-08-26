// 最小の service worker。**app shell を配るだけ**で、それ以外には一切触らない。
//
// 触らないもの (どれも取り違えると事故になる):
//   - /ws        … WebSocket の upgrade。fetch を通らないが、念のため除外する
//   - /auth/*    … Steam OpenID のリダイレクト。キャッシュすると認証が壊れる
//   - GET 以外   … /internal/* を含め、副作用のあるものをキャッシュしない
//   - 別オリジン … opaque response を溜め込まない
var VERSION = 'pv-v1';
var SHELL = [
  './',
  './index.html',
  './dev.html',
  './css/app.css',
  './js/protocol.js',
  './js/audio.js',
  './js/signal.js',
  './js/rtc.js',
  './js/mic.js',
  './js/mic-worklet.js',
  './js/app.js',
  './js/dev.js',
  './manifest.json',
  './icons/icon-192.png',
  './icons/icon-512.png'
];

self.addEventListener('install', function (e) {
  e.waitUntil(caches.open(VERSION).then(function (c) { return c.addAll(SHELL); }).then(function () {
    return self.skipWaiting();
  }));
});

self.addEventListener('activate', function (e) {
  e.waitUntil(caches.keys().then(function (keys) {
    return Promise.all(keys.filter(function (k) { return k !== VERSION; })
      .map(function (k) { return caches.delete(k); }));
  }).then(function () { return self.clients.claim(); }));
});

self.addEventListener('fetch', function (e) {
  var req = e.request;
  if (req.method !== 'GET') return;
  var url = new URL(req.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname === '/ws' || url.pathname.indexOf('/auth') === 0 || url.pathname.indexOf('/internal') === 0) return;

  // stale-while-revalidate: 手元のを即返しつつ、裏で新しいものを取っておく。
  // リレーが配る静的ファイルが更新されても、次の起動で追いつく
  e.respondWith(caches.open(VERSION).then(function (cache) {
    return cache.match(req).then(function (hit) {
      var net = fetch(req).then(function (res) {
        if (res && res.status === 200 && res.type === 'basic') cache.put(req, res.clone());
        return res;
      }).catch(function () {
        // オフライン。ページ遷移なら app shell を返す
        return hit || (req.mode === 'navigate' ? cache.match('./index.html') : Promise.reject(new Error('offline')));
      });
      return hit || net;
    });
  }));
});
