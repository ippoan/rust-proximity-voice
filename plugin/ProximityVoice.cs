// ProximityVoice — Rust (Facepunch) / Oxide プラグイン。担当 #1-4。
//
// ゲームサーバー側の唯一の役目は「リレーに、ゲームが既に与えている以上の情報を渡さずに、
// 誰が誰の声を聞いてよいかを教えること」。契約は ../docs/protocol.md、型の正本は
// ../relay/src/proto.rs。
//
//   POST /internal/roster  2 秒ごと (+ 接続 / 切断で即時)   … 認可
//   POST /internal/graph   2 Hz、**変化時のみ**             … 配信範囲 (距離と世界方位だけ)
//   POST /internal/yaw     20 Hz                            … 聞き手の向き
//   POST /internal/talk    PTT の変化の都度                 … V キー
//
// ★ 絶対座標を送らない。これは機能ではなく安全要件 (README「セキュリティ設計」)。
//   座標は距離と方位の計算に使うが、送信 JSON には距離 (1m 刻み) と世界方位 (5° 刻み)
//   しか載せない。ゲーム内 VC はもともと定位しているので、この 2 つは新しい情報ではない。
//   絶対座標を渡した瞬間に、改造クライアントが位置を抜ける (= ESP) 経路を作ってしまう。
//   CI (.github/workflows/ci.yml の no-absolute-coords) が機械検査している。
//
// ★ このマシンには dotnet が無く、コンパイル検証はしていない。Oxide の API 名や
//   シグネチャに確信が持てない箇所には `// ASSUMPTION:` を付けてある。実サーバーで潰すこと。
//   ワイヤ形式 (JSON + HMAC) のほうは plugin/verify_wire.mjs で実物のリレーに当てて検証済み。

using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

using Newtonsoft.Json;
using Oxide.Core.Libraries;
using Oxide.Core.Plugins;
using Oxide.Game.Rust.Cui;
using UnityEngine;

// Oxide.Core.Libraries にも Time ライブラリが居て UnityEngine.Time と衝突するので、
// どちらを指しているかを固定する。
using Time = UnityEngine.Time;

namespace Oxide.Plugins
{
    [Info("Proximity Voice", "ippoan", "1.0.0")]
    [Description("近接VC リレーへ可聴グラフ・向き・PTT・名簿を push する (絶対座標は送らない)")]
    public class ProximityVoice : RustPlugin
    {
        // ---- 設定 ----

        public class PluginConfig
        {
            /// リレーの根 URL。末尾のスラッシュは付けても付けなくてもよい。
            public string relay_url = "http://127.0.0.1:8080";

            /// HMAC の共有秘密。**リレーの PV_HMAC_SECRET と同じ値**にする。
            public string hmac_secret = "";

            /// この Rust サーバーの識別子。リレー側の seq / 名簿はこの単位で持たれる。
            public string server_id = "main";

            /// 死亡中に失効させるか。
            /// TODO: 実サーバーで「死亡中にネイティブ VC が聞こえるか」を確認して既定値を決める。
            ///       docs/protocol.md §0 のとおり、判断の基準は「ゲーム内 VC が聞かせる以上を
            ///       聞かせない」。ネイティブが死亡中も聞かせるなら false が正しい。
            ///       実測できない環境なので既定は安全側 (true = 切る) に倒してある。
            public bool revoke_on_death = true;

            /// 声が届く距離 (m)。AUDIBLE_M。gain を計算するのは PWA なので、プラグインは
            /// この値を使わない。設定ファイルに置いてあるのは「リレー / PWA と数字を
            /// 突き合わせるときの控え」であって、変えても送信内容は変わらない。
            public float audible_m = 60f;

            /// 購読を張る距離 (m)。SUBSCRIBE_M。この距離以内の相手だけを graph に載せる。
            /// 60〜75m は購読済みだが gain 0 (ヒステリシス)。
            public float subscribe_m = 75f;

            /// 可聴グラフの更新レート。GRAPH_HZ。
            public float graph_hz = 2f;

            /// 聞き手の向きの更新レート。YAW_HZ。
            public float yaw_hz = 20f;

            /// 名簿の push 間隔 (秒)。ROSTER_INTERVAL_S。リレーの TTL は 10 秒。
            public float roster_interval_s = 2f;

            /// 距離の量子化幅 (m)。DIST_QUANT_M。
            public float dist_quant_m = 1f;

            /// 方位の量子化幅 (度)。BEARING_QUANT_DEG。
            public float bearing_quant_deg = 5f;

            /// OnPlayerVoice がこの時間だけ来なくなったら「V を離した」と見なす (ミリ秒)。
            public float ptt_release_ms = 200f;

            /// ネイティブの近接ブロードキャストを抑止するか。
            /// **既定 true。** false にするとゲーム内 VC とリレー越しの声が二重に聞こえる。
            /// リレーが落ちているあいだだけ暫定でゲーム内 VC に戻す、といった運用のための逃げ道。
            public bool suppress_native_voice = true;

            /// CUI の HUD を出すか。
            public bool hud_enabled = true;

            /// HUD の更新レート (Hz)。仕様は 2〜4 Hz。
            public float hud_hz = 3f;

            /// 変化が無くても、この秒数が経ったら graph / yaw を再送する。0 で無効。
            ///
            /// **自己修復しない無音を潰すための保険。** 効くのは 3 つの場面:
            /// リレーが古くて WS 接続時の撒き直しを持たない / 独自のリレーと繋ぐ /
            /// **リレーだけが再起動した**。3 つ目が本命で、プラグインは走り続けているので
            /// 名簿は 2 秒ごとに無条件で飛んで復旧するが、graph は変化時のみなのでリレー側の
            /// `hears` は空のまま埋まらない。ブラウザは再接続して `resync` も受け取る
            /// (接続自体は起きる) が、**リレーに撒くものが無い**。
            /// **リレーの状態を埋め直せるのはプラグインだけ**なので、クライアント側では直せない。
            /// 人が動き続けていればすぐ直るが、全員が拠点で静止していると止まったままになる。
            ///
            /// ★ **30 秒である理由。短くしてはいけない。**
            /// この再送は、リレーや PWA 側の「1 通取りこぼすと次が来ない」たぐいのバグを
            /// **覆い隠す**。取りこぼしは本来「永久に無音」として現れるから見つかるのであって、
            /// 5 秒の再送があると同じバグが「入室時に数秒だけ無音」になり、運用では
            /// 「ちょっと出遅れた」に見えて報告されない。30 秒なら「最大 30 秒無音」として
            /// 現れ、これは**報告される長さ**になる。隠れてしまう競合の実例が
            /// `graph` → `peer` の到着順。自己修復しない経路だけを潰し、
            /// 秒未満の競合は隠さない、という線引き。
            ///
            /// 代償は小さい: 10 人で 60 バイト × 10 ÷ 30 秒 ≒ 20 B/s。
            /// 「静止中に 2 Hz で流し続けない」という本来の意図も保たれる。
            ///
            /// 0 にすると完全な変化検出になり、静止中のトラフィックはゼロになる。
            /// 代わりに上の 3 場面で**永久に無音になる経路が残る**。
            public float resend_unchanged_s = 30f;

            /// HTTP のタイムアウト (秒)。
            public float http_timeout_s = 2f;
        }

        private PluginConfig _cfg;

        protected override void LoadDefaultConfig()
        {
            _cfg = new PluginConfig();
        }

        protected override void LoadConfig()
        {
            base.LoadConfig();
            try
            {
                _cfg = Config.ReadObject<PluginConfig>();
                if (_cfg == null) throw new Exception("null");
            }
            catch (Exception e)
            {
                PrintWarning("config が読めないので既定値で作り直す: " + e.Message);
                LoadDefaultConfig();
            }
            SaveConfig();
        }

        protected override void SaveConfig()
        {
            Config.WriteObject(_cfg, true);
        }

        // ---- 状態 ----

        private const string HudPanel = "pv.hud";

        /// endpoint ごとの送信口。**seq は endpoint ごとに独立した counter。**
        /// 1 本にすると 0.5Hz の roster と 20Hz の yaw が互いを巻き戻し扱いして 401 が出続ける。
        private Channel _chRoster, _chGraph, _chYaw, _chTalk;

        /// V キーが押されている人 (SteamID64 文字列)。
        private readonly HashSet<string> _talking = new HashSet<string>();

        /// 最後に OnPlayerVoice が来た時刻 (realtimeSinceStartup)。
        private readonly Dictionary<string, float> _lastVoiceAt = new Dictionary<string, float>();

        /// 直近の graph tick で「その聞き手に聞こえていた相手」。HUD だけが使う。
        private readonly Dictionary<string, List<string>> _audible = new Dictionary<string, List<string>>();

        /// HUD の再描画を抑えるための直近の中身。
        private readonly Dictionary<string, string> _hudLast = new Dictionary<string, string>();

        /// 直近の push が通ったか。HUD の接続状態表示に使う。
        private bool _relayOk;
        private string _relayNote = "未接続";
        private readonly Dictionary<string, float> _lastWarnAt = new Dictionary<string, float>();

        private Timer.TimerInstance _tRoster, _tGraph, _tYaw, _tPtt, _tHud;

        /// 使い回しのバッファ (毎 tick の割り当てを減らす)。
        private readonly List<BasePlayer> _live = new List<BasePlayer>();
        private readonly List<string> _ids = new List<string>();
        private readonly List<string> _names = new List<string>();
        private readonly Dictionary<string, string> _nameOf = new Dictionary<string, string>();

        // ---- ライフサイクル ----

        private void OnServerInitialized()
        {
            if (string.IsNullOrEmpty(_cfg.hmac_secret))
            {
                PrintError("hmac_secret が空。oxide/config/ProximityVoice.json にリレーと同じ秘密を書いて reload すること。push は行わない");
                return;
            }

            // seq の初期値は **unix ミリ秒**。リレーの seq guard はプロセスが生きているあいだ
            // 最終 seq を覚えているので、プラグインを reload するたびに 0 から始めると
            // 「巻き戻り」と見なされて 401 が出続ける。壁時計のミリ秒なら、いちばん速い
            // yaw (20 Hz = 秒 20 進む) でも 秒 1000 進む seed に追い抜かれないので、
            // 再起動後は必ず前回より大きい値から始まる。
            long seed = UnixMillis();
            _chRoster = new Channel(this, "roster", "/internal/roster", seed);
            _chGraph = new Channel(this, "graph", "/internal/graph", seed);
            _chYaw = new Channel(this, "yaw", "/internal/yaw", seed);
            _chTalk = new Channel(this, "talk", "/internal/talk", seed);

            // ★ 名簿を先に送る。リレーは roster 未受信の server_id の graph / yaw を
            //   まるごと捨てる (relay/src/roster.rs の apply_graph / apply_yaw)。
            PushRoster();

            _tRoster = timer.Every(Math.Max(0.25f, _cfg.roster_interval_s), () => PushRoster(null));
            _tGraph = timer.Every(HzToSec(_cfg.graph_hz, 2f), GraphTick);
            // ASSUMPTION: Oxide の timer はサーバーのフレームで駆動されるので、0.05s (=20Hz) の
            //   分解能はサーバー FPS (通常 30 前後) に律速される。実測して届かないようなら
            //   yaw_hz を 10 まで落とす (2 バイトの値なので追従性以外に失うものが無い)。
            _tYaw = timer.Every(HzToSec(_cfg.yaw_hz, 20f), YawTick);
            _tPtt = timer.Every(0.1f, PttSweep);
            if (_cfg.hud_enabled) _tHud = timer.Every(HzToSec(_cfg.hud_hz, 3f), HudTick);

            Puts("ProximityVoice 起動: relay=" + TrimUrl(_cfg.relay_url)
                 + " server_id=" + _cfg.server_id
                 + " graph=" + _cfg.graph_hz + "Hz yaw=" + _cfg.yaw_hz + "Hz");
        }

        private void Unload()
        {
            if (_tRoster != null) _tRoster.Destroy();
            if (_tGraph != null) _tGraph.Destroy();
            if (_tYaw != null) _tYaw.Destroy();
            if (_tPtt != null) _tPtt.Destroy();
            if (_tHud != null) _tHud.Destroy();

            foreach (var p in BasePlayer.activePlayerList)
            {
                if (p != null) CuiHelper.DestroyUi(p, HudPanel);
            }

            // ここで空の名簿を push して即座に失効させたいところだが、Unload 中に
            // webrequest を投げても Oxide がプラグインごと畳むので届く保証が無い。
            // 代わりにリレーの ROSTER_TTL_S (10 秒) の fail closed に任せる。
            // TTL はまさにこの「プラグインが黙った」ケースのためにある。
        }

        // ---- ゲーム側のフック ----

        private void OnPlayerConnected(BasePlayer player)
        {
            // 名簿に載るのが 2 秒遅れると、その間だけ WS が not_eligible で弾かれる。即時 push。
            PushRoster();
        }

        private void OnPlayerDisconnected(BasePlayer player, string reason)
        {
            if (player == null) return;
            string id = player.UserIDString;

            // §0「切断 / Alt-F4 は必須で失効」。2 秒を待たずに名簿から外して即時 push する。
            _lastVoiceAt.Remove(id);
            if (_talking.Remove(id)) PushTalk(id, false);
            _audible.Remove(id);
            _hudLast.Remove(id);
            CuiHelper.DestroyUi(player, HudPanel);

            // activePlayerList からいつ消えるかはフックの呼ばれ方次第なので、明示的に外す。
            PushRoster(id);
        }

        // ASSUMPTION: 死亡 / リスポーンのフック名とシグネチャ。名簿は 2 秒ごとに
        //   フル置換されるので、これが動かなくても最大 2 秒遅れるだけで壊れはしない。
        private void OnPlayerDeath(BasePlayer player, HitInfo info)
        {
            if (_cfg.revoke_on_death) PushRoster();
        }

        private void OnPlayerRespawned(BasePlayer player)
        {
            if (_cfg.revoke_on_death) PushRoster();
        }

        /// V キーの検出。**ゲーム内 VC を、音声としてではなく入力デバイスとして使う。**
        /// プレイヤーの操作は従来どおり V キーで、覚え直すものは無い。
        ///
        /// ASSUMPTION: フックのシグネチャは `object OnPlayerVoice(BasePlayer, byte[])` で、
        ///   **非 null を返すと既定の近接ブロードキャストが抑止される**。
        ///   根拠は Team Voice Chat / VoiceLimiter といった既存プラグインが同じことを
        ///   しているため (README「ゲーム内 VC をプラグインで改善する」)。
        ///   抑止できていなければゲーム内 VC とリレー越しの声が二重に聞こえるので、
        ///   実サーバーで一聴すれば分かる。効かなかった場合の代替は実機で確認するまで断定しない。
        private object OnPlayerVoice(BasePlayer player, byte[] data)
        {
            if (player == null) return null;
            string id = player.UserIDString;

            _lastVoiceAt[id] = Time.realtimeSinceStartup;
            if (_talking.Add(id)) PushTalk(id, true);

            return _cfg.suppress_native_voice ? (object)true : null;
        }

        /// 「離した」の判定。OnPlayerVoice は押しているあいだ連続で来るので、
        /// 一定時間来なくなったら離したと見なす。
        private void PttSweep()
        {
            float now = Time.realtimeSinceStartup;
            float release = Math.Max(0.05f, _cfg.ptt_release_ms / 1000f);

            _ids.Clear();
            foreach (var kv in _lastVoiceAt)
            {
                if (now - kv.Value > release) _ids.Add(kv.Key);
            }
            for (int i = 0; i < _ids.Count; i++)
            {
                string id = _ids[i];
                _lastVoiceAt.Remove(id);
                if (_talking.Remove(id)) PushTalk(id, false);
            }
        }

        // ---- 名簿 ----

        /// 接続中のプレイヤーの SteamID64 一覧。**差分ではなく毎回フル。**
        /// `skip` は「たった今切断したが activePlayerList にまだ残っているかもしれない人」。
        private void PushRoster(string skip = null)
        {
            if (_chRoster == null) return;

            _ids.Clear();
            foreach (var p in BasePlayer.activePlayerList)
            {
                if (!Eligible(p)) continue;
                string id = p.UserIDString;
                if (skip != null && id == skip) continue;
                _ids.Add(id);
            }
            _ids.Sort(StringComparer.Ordinal);

            // 名簿は TTL (10 秒) の heartbeat も兼ねているので、中身が同じでも必ず送る。
            _chRoster.SendState("eligible", JsonConvert.SerializeObject(_ids), true);
        }

        /// 名簿と graph に載せてよい人か。
        ///
        /// - **スリープは自動的に外れる。** スリーパーはクライアントが切れているので
        ///   activePlayerList に居ない (§0「未接続なので名簿に載らない」)。
        ///   逆に「接続済みだがまだ寝ている」(参加直後の起き上がり待ち) は接続しているので
        ///   ここでは落とさない。ネイティブ VC もその状態の人に聞かせるため。
        /// - 死亡は revoke_on_death 次第。PluginConfig の TODO を参照。
        private bool Eligible(BasePlayer p)
        {
            if (p == null || !p.IsConnected) return false;
            // ASSUMPTION: BaseCombatEntity.IsDead()。
            if (_cfg.revoke_on_death && p.IsDead()) return false;
            return true;
        }

        // ---- 可聴グラフ ----

        /// 聞き手ごとに購読距離以内の相手を集めて push する。**変化が無ければ送らない。**
        private void GraphTick()
        {
            if (_chGraph == null) return;

            _live.Clear();
            foreach (var p in BasePlayer.activePlayerList)
            {
                if (Eligible(p)) _live.Add(p);
            }
            // 聞き手の並びを固定する。並びが tick ごとに揺れると、中身が同じでも
            // 「変化した」と誤判定して静止中に送り続けてしまう。
            _live.Sort(CompareById);

            int subM = (int)Math.Max(1f, _cfg.subscribe_m);
            float quant = _cfg.dist_quant_m <= 0f ? 1f : _cfg.dist_quant_m;
            // 量子化で境界をまたぐぶん、粗く拾ってから量子化後の距離で切る。
            float cutoff = subM + quant;
            float cutoffSq = cutoff * cutoff;

            var listeners = new List<object>(_live.Count);

            for (int i = 0; i < _live.Count; i++)
            {
                BasePlayer listener = _live[i];
                Vector3 lp = listener.transform.position;
                string lid = listener.UserIDString;

                var heard = new List<Entry>();
                for (int j = 0; j < _live.Count; j++)
                {
                    if (i == j) continue;
                    BasePlayer speaker = _live[j];
                    Vector3 sp = speaker.transform.position;

                    float dx = sp.x - lp.x;
                    float dy = sp.y - lp.y;
                    float dz = sp.z - lp.z;
                    float sq = dx * dx + dy * dy + dz * dz;
                    if (sq > cutoffSq) continue;

                    int d = QuantDist(Mathf.Sqrt(sq));
                    if (d > subM) continue;

                    // ★ ここが「渡してよい情報」の境界。dx / dz は方位を出すためだけに使い、
                    //   角度と距離に潰してから外へ出す。差分ベクトルそのものは絶対に載せない。
                    //   Rust / Unity は +Z が北・+X が東で、Atan2(dx, dz) が「+Z を 0° と
                    //   する時計回りの方位」になる。yaw (viewAngles.y) と同じ基準なので、
                    //   PWA の (b - yaw + 360) % 360 がそのまま相対方位になる。
                    int b = QuantBearing(Mathf.Atan2(dx, dz) * Mathf.Rad2Deg);

                    heard.Add(new Entry
                    {
                        id = speaker.UserIDString,
                        d = d,
                        b = b,
                        // 集める段階で購読距離以内に絞ってあるので実質いつも true。
                        // それでも導出式のまま書いておく (集める半径を変えたときに追随するように)。
                        sub = d <= subM,
                    });
                }

                // ★ 距離の近い順。リレーは先頭 SLOTS(=16) 件に切り詰めるだけなので、
                //   並んでいないと「近い人が切られて遠い人が残る」。
                //   同距離は SteamID 順 — 並びが tick ごとに揺れるとスロットが無用に動く
                //   (relay/src/roster.rs の speakers_by_distance と同じ規則)。
                heard.Sort(CompareHeard);

                // HUD 用に控える。
                List<string> ids;
                if (!_audible.TryGetValue(lid, out ids))
                {
                    ids = new List<string>();
                    _audible[lid] = ids;
                }
                ids.Clear();
                for (int k = 0; k < heard.Count; k++) ids.Add(heard[k].id);

                // 聞こえる相手が居なくなった聞き手も **必ず載せる**。リレーの apply_graph は
                // push に載っている聞き手だけを更新するので、黙って外すと古い購読が残る。
                listeners.Add(new { id = lid, hears = heard });
            }

            _chGraph.SendState("listeners", JsonConvert.SerializeObject(listeners), false);
        }

        private static int CompareById(BasePlayer a, BasePlayer b)
        {
            return string.CompareOrdinal(a.UserIDString, b.UserIDString);
        }

        private static int CompareHeard(Entry x, Entry y)
        {
            int c = x.d.CompareTo(y.d);
            return c != 0 ? c : string.CompareOrdinal(x.id, y.id);
        }

        /// 送信される 1 件。匿名型にしないのは並べ替えのため。
        /// **キー名がそのままワイヤ形式** (relay/src/proto.rs の Heard) なので、
        /// フィールド名の綴りに寄りかからず JsonProperty で固定しておく。
        private class Entry
        {
            [JsonProperty("id")] public string id;
            /// 距離 (m)、DIST_QUANT_M 刻み
            [JsonProperty("d")] public int d;
            /// **世界座標系での**方位 (度 0-355)、BEARING_QUANT_DEG 刻み
            [JsonProperty("b")] public int b;
            /// 購読すべきか (d <= SUBSCRIBE_M)
            [JsonProperty("sub")] public bool sub;
        }

        // ---- 聞き手の向き ----

        /// 1 tick = 1 リクエストに全聞き手ぶんを詰める (プレイヤーごとに POST しない)。
        ///
        /// graph と分けるのは変化の速さが違うから。振り向きは一瞬で 180° 変わるので
        /// 追従性が要るが、距離と可聴集合は歩行速度でしか変わらない。yaw は 1 人あたり
        /// 2 バイトなので 20 Hz でも軽い。
        private void YawTick()
        {
            if (_chYaw == null) return;

            var yaws = new List<object[]>();
            foreach (var p in BasePlayer.activePlayerList)
            {
                if (!Eligible(p)) continue;
                yaws.Add(new object[] { p.UserIDString, YawOf(p) });
            }
            if (yaws.Count == 0) return;

            _chYaw.SendState("yaws", JsonConvert.SerializeObject(yaws), false);
        }

        /// 世界座標系での向き (度 0-359)。
        ///
        /// ASSUMPTION: `BasePlayer.viewAngles` はクライアントから届く視点角で、y が yaw。
        ///   効かない場合の代替は `player.eyes.rotation.eulerAngles.y`。
        ///   どちらも「+Z を 0° として時計回り」で、上の方位の取り方と同じ基準。
        private int YawOf(BasePlayer p)
        {
            float y = p.viewAngles.y % 360f;
            if (y < 0f) y += 360f;
            int deg = (int)Math.Round(y, MidpointRounding.AwayFromZero) % 360;
            return deg < 0 ? deg + 360 : deg;
        }

        // ---- PTT ----

        private void PushTalk(string steamId, bool talking)
        {
            if (_chTalk == null) return;
            _chTalk.SendTalk(steamId, talking);
        }

        // ---- 量子化 ----

        private int QuantDist(float raw)
        {
            float q = _cfg.dist_quant_m <= 0f ? 1f : _cfg.dist_quant_m;
            double d = Math.Round(raw / q, MidpointRounding.AwayFromZero) * q;
            if (d < 0d) d = 0d;
            if (d > 65535d) d = 65535d;
            return (int)d;
        }

        /// 度 (任意の実数) → 0-355 の 5° 刻み。
        private int QuantBearing(float deg)
        {
            float q = _cfg.bearing_quant_deg <= 0f ? 5f : _cfg.bearing_quant_deg;
            deg %= 360f;
            if (deg < 0f) deg += 360f;
            int b = (int)(Math.Round(deg / q, MidpointRounding.AwayFromZero) * q);
            b %= 360;
            return b < 0 ? b + 360 : b;
        }

        // ---- HUD (CUI) ----

        /// 画面端のリスト。**位置は描かない。**
        ///
        /// 壁の向こうの相手が描かれたら、配信画面がそのまま ESP になる (README「セキュリティ設計」)。
        /// 頭上追従もやらない — world→screen の投影には相手の FOV と解像度が要るが、
        /// サーバーはそれを知らないため。
        private void HudTick()
        {
            // 名前は 1 tick に 1 回だけ引く (聞き手ごとに全員を舐めると人数の 2 乗になる)。
            _nameOf.Clear();
            foreach (var p in BasePlayer.activePlayerList)
            {
                if (p != null) _nameOf[p.UserIDString] = Sanitize(p.displayName);
            }

            foreach (var p in BasePlayer.activePlayerList)
            {
                if (p == null || !p.IsConnected) continue;
                string id = p.UserIDString;

                _names.Clear();
                List<string> audible;
                if (_audible.TryGetValue(id, out audible))
                {
                    for (int i = 0; i < audible.Count; i++)
                    {
                        string other = audible[i];
                        if (!_talking.Contains(other)) continue;
                        string name;
                        _names.Add(_nameOf.TryGetValue(other, out name) ? name : other);
                    }
                }

                string body = _names.Count == 0 ? "" : string.Join("\n", _names.ToArray());
                string status = _relayOk
                    ? "<color=#7ddc7d>●</color> リレー接続"
                    : "<color=#dc7d7d>●</color> リレー" + _relayNote;
                string content = status + "\n" + body;

                string prev;
                if (_hudLast.TryGetValue(id, out prev) && prev == content) continue;
                _hudLast[id] = content;

                CuiHelper.DestroyUi(p, HudPanel);

                var c = new CuiElementContainer();
                string panel = c.Add(new CuiPanel
                {
                    Image = { Color = "0 0 0 0.40" },
                    RectTransform = { AnchorMin = "0.815 0.32", AnchorMax = "0.995 0.60" },
                    CursorEnabled = false,
                }, "Hud", HudPanel);

                c.Add(new CuiLabel
                {
                    Text =
                    {
                        Text = status,
                        FontSize = 11,
                        Align = TextAnchor.UpperLeft,
                        Color = "1 1 1 0.85",
                    },
                    RectTransform = { AnchorMin = "0.06 0.84", AnchorMax = "0.97 0.99" },
                }, panel);

                c.Add(new CuiLabel
                {
                    Text =
                    {
                        Text = body,
                        FontSize = 13,
                        Align = TextAnchor.UpperLeft,
                        Color = "1 1 1 0.95",
                    },
                    RectTransform = { AnchorMin = "0.06 0.02", AnchorMax = "0.97 0.82" },
                }, panel);

                // ASSUMPTION: `CuiHelper.AddUi(BasePlayer, CuiElementContainer)` は
                //   `CommunityEntity.ServerInstance.ClientRPC(..., "AddUI", json)` の薄い包み。
                //   ClientRPC のシグネチャは Rust の更新でたびたび変わる (第 1 引数が
                //   Connection の世代と RpcTarget の世代がある) ので、追随済みの
                //   CuiHelper 側を呼んでいる。直接叩きたい場合はここを差し替える。
                CuiHelper.AddUi(p, c);
            }
        }

        /// CUI のテキストは TextMeshPro のタグを解釈するので、名前からタグを落とす。
        private static string Sanitize(string s)
        {
            if (string.IsNullOrEmpty(s)) return "?";
            var sb = new StringBuilder(s.Length);
            foreach (char ch in s)
            {
                if (ch == '<' || ch == '>' || ch < ' ') continue;
                sb.Append(ch);
                if (sb.Length >= 24) break;
            }
            return sb.Length == 0 ? "?" : sb.ToString();
        }

        // ---- 送信 ----

        /// unix epoch。DateTimeOffset の ToUnixTime 系は .NET 4.6 以降にしか無く、
        /// Rust サーバーが載っている Mono の版に依存したくないので自前で引く。
        private static readonly DateTime Epoch = new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc);

        private static long UnixSeconds()
        {
            return (long)(DateTime.UtcNow - Epoch).TotalSeconds;
        }

        private static long UnixMillis()
        {
            return (long)(DateTime.UtcNow - Epoch).TotalMilliseconds;
        }

        private static float HzToSec(float hz, float fallback)
        {
            float h = hz > 0f ? hz : fallback;
            return 1f / h;
        }

        private static string TrimUrl(string url)
        {
            return (url ?? "").TrimEnd('/');
        }

        private void OnPushResult(string name, int code, string response)
        {
            if (code == 204 || code == 200)
            {
                if (!_relayOk) Puts("リレーへの " + name + " push が通った (" + code + ")");
                _relayOk = true;
                _relayNote = "接続";
                return;
            }

            _relayOk = false;
            _relayNote = code == 401 ? "認証失敗" : (code == 0 ? "未接続" : "エラー " + code);

            // yaw は 20 Hz なので、リレーが落ちているあいだ素直に出すとコンソールが
            // 毎秒 20 行で埋まる。endpoint ごとに 5 秒に 1 行へ間引く。
            float now = Time.realtimeSinceStartup;
            float last;
            if (_lastWarnAt.TryGetValue(name, out last) && now - last < 5f) return;
            _lastWarnAt[name] = now;

            // 401 が出続けるときの原因はだいたい 3 つ:
            //   1. hmac_secret の不一致
            //   2. サーバー時刻のずれ (許容は HMAC_SKEW_S = 30 秒)
            //   3. seq の巻き戻り — endpoint ごとに独立した counter を使っていない、
            //      またはプラグイン reload で counter が 0 に戻った
            string tail = response ?? "";
            if (tail.Length > 200) tail = tail.Substring(0, 200);
            PrintWarning(name + " push が失敗: code=" + code + " body=" + tail);
        }

        /// ★ HTTP は Oxide の webrequest を使う。プラグインは制限モードで
        ///   System.Net / System.IO を塞がれている
        ///   (UnauthorizedAccessException: System access is restricted)。
        ///   同じ理由でログもファイルへ直接書かず Puts / PrintWarning に流す。
        ///
        /// ASSUMPTION: `webrequest.Enqueue(url, body, Action<int,string>, Plugin,
        ///   RequestMethod, Dictionary<string,string>, float timeoutSeconds)`。
        private void Enqueue(string url, string body, Dictionary<string, string> headers, Action<int, string> callback)
        {
            webrequest.Enqueue(url, body, (code, response) => callback(code, response), this,
                RequestMethod.POST, headers, Math.Max(0.5f, _cfg.http_timeout_s));
        }

        /// endpoint 1 本ぶんの送信口。
        ///
        /// ★ **1 本につき同時に 1 リクエストしか投げない。** seq の単調増加はリレーが
        ///   *受信した順* に見るので、2 本が同時に飛ぶと到着が入れ替わったときに
        ///   古いほうが 401 で落ちる。状態を運ぶ graph / yaw / roster は tick を落として
        ///   次の tick で収束させ、イベントである talk だけは小さなキューに積む。
        private class Channel
        {
            private readonly ProximityVoice _p;
            private readonly string _name;
            private readonly string _path;
            private long _seq;
            private bool _inFlight;
            private string _lastFragment;
            private float _lastSentAt = -9999f;
            private readonly Queue<KeyValuePair<string, bool>> _talkQueue =
                new Queue<KeyValuePair<string, bool>>();

            public Channel(ProximityVoice p, string name, string path, long seed)
            {
                _p = p;
                _name = name;
                _path = path;
                _seq = seed;
            }

            /// 状態を運ぶ endpoint (roster / graph / yaw)。
            /// `fragment` が前回と同じなら送らない。`always` を立てると変化検出を飛ばす。
            public void SendState(string key, string fragment, bool always)
            {
                if (_inFlight) return;

                if (!always)
                {
                    float keep = _p._cfg.resend_unchanged_s;
                    bool stale = keep > 0f && (Time.realtimeSinceStartup - _lastSentAt) >= keep;
                    if (fragment == _lastFragment && !stale) return;
                }

                long ts = UnixSeconds();
                long seq = ++_seq;

                var sb = new StringBuilder(fragment.Length + 96);
                sb.Append("{\"server_id\":").Append(JsonConvert.SerializeObject(_p._cfg.server_id));
                sb.Append(",\"seq\":").Append(seq.ToString(CultureInfo.InvariantCulture));
                sb.Append(",\"ts\":").Append(ts.ToString(CultureInfo.InvariantCulture));
                sb.Append(",\"").Append(key).Append("\":").Append(fragment).Append('}');

                _lastFragment = fragment;
                _lastSentAt = Time.realtimeSinceStartup;
                Post(sb.ToString(), ts);
            }

            public void SendTalk(string steamId, bool talking)
            {
                // PTT は状態ではなくイベント。落とすと「押しっぱなし」や「無音」が残るので積む。
                _talkQueue.Enqueue(new KeyValuePair<string, bool>(steamId, talking));
                while (_talkQueue.Count > 64) _talkQueue.Dequeue();
                DrainTalk();
            }

            private void DrainTalk()
            {
                if (_inFlight || _talkQueue.Count == 0) return;
                var item = _talkQueue.Dequeue();

                long ts = UnixSeconds();
                long seq = ++_seq;
                string body = JsonConvert.SerializeObject(new
                {
                    server_id = _p._cfg.server_id,
                    seq = seq,
                    ts = ts,
                    id = item.Key,
                    talking = item.Value,
                });
                Post(body, ts);
            }

            private void Post(string body, long ts)
            {
                string tsStr = ts.ToString(CultureInfo.InvariantCulture);
                string sig = Hmac.HexSign(_p._cfg.hmac_secret, tsStr, body);

                var headers = new Dictionary<string, string>
                {
                    { "Content-Type", "application/json" },
                    { "X-PV-Timestamp", tsStr },
                    { "X-PV-Signature", sig },
                };

                _inFlight = true;
                string url = TrimUrl(_p._cfg.relay_url) + _path;

                _p.Enqueue(url, body, headers, (code, response) =>
                {
                    _inFlight = false;
                    _p.OnPushResult(_name, code, response);
                    DrainTalk();
                });
            }
        }

        // ---- HMAC-SHA256 ----

        /// `hex(hmac_sha256(secret, timestamp + "." + body))`。
        ///
        /// **SHA-256 を自前で持っている理由**: Oxide のプラグインは制限モードで
        /// `System.Security` を含むいくつかの名前空間を塞ぐことがあり、
        /// `System.Security.Cryptography.HMACSHA256` が使える保証が無い。押し切ると
        /// 起動時に例外で落ちるが、ここは全 push の必須経路なので賭けない。
        /// 使うのは uint の算術と配列だけ。
        ///
        /// 速度は問題にならない: いちばん重い graph でも 2 Hz、いちばん速い yaw は
        /// 1 リクエスト数百バイト。
        ///
        /// **この実装は plugin/verify_wire.mjs が同じ手順を JS に写して
        /// node:crypto と突き合わせて検証している** (dotnet が無くコンパイルできないため)。
        private static class Hmac
        {
            public static string HexSign(string secret, string timestamp, string body)
            {
                byte[] key = Encoding.UTF8.GetBytes(secret ?? "");
                // 署名対象は timestamp + "." + body。timestamp を含めることで
                // 「署名は正しいが時刻ヘッダだけ差し替える」リプレイを塞ぐ。
                byte[] data = Encoding.UTF8.GetBytes(timestamp + "." + body);
                return Hex(HmacSha256(key, data));
            }

            private static byte[] HmacSha256(byte[] key, byte[] data)
            {
                const int Block = 64;
                if (key.Length > Block) key = Sha256(key);

                byte[] k = new byte[Block];
                Buffer.BlockCopy(key, 0, k, 0, key.Length);

                byte[] inner = new byte[Block + data.Length];
                for (int i = 0; i < Block; i++) inner[i] = (byte)(k[i] ^ 0x36);
                Buffer.BlockCopy(data, 0, inner, Block, data.Length);
                byte[] innerHash = Sha256(inner);

                byte[] outer = new byte[Block + 32];
                for (int i = 0; i < Block; i++) outer[i] = (byte)(k[i] ^ 0x5c);
                Buffer.BlockCopy(innerHash, 0, outer, Block, 32);
                return Sha256(outer);
            }

            private static readonly uint[] K =
            {
                0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
                0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
                0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
                0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
                0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
                0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
                0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
                0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
            };

            private static uint Ror(uint x, int n)
            {
                return (x >> n) | (x << (32 - n));
            }

            private static byte[] Sha256(byte[] msg)
            {
                uint h0 = 0x6a09e667, h1 = 0xbb67ae85, h2 = 0x3c6ef372, h3 = 0xa54ff53a;
                uint h4 = 0x510e527f, h5 = 0x9b05688c, h6 = 0x1f83d9ab, h7 = 0x5be0cd19;

                long bitLen = (long)msg.Length * 8L;
                int padded = ((msg.Length + 9 + 63) / 64) * 64;
                byte[] m = new byte[padded];
                Buffer.BlockCopy(msg, 0, m, 0, msg.Length);
                m[msg.Length] = 0x80;
                for (int i = 0; i < 8; i++) m[padded - 1 - i] = (byte)(bitLen >> (8 * i));

                uint[] w = new uint[64];
                for (int off = 0; off < padded; off += 64)
                {
                    for (int i = 0; i < 16; i++)
                    {
                        int j = off + i * 4;
                        w[i] = ((uint)m[j] << 24) | ((uint)m[j + 1] << 16) | ((uint)m[j + 2] << 8) | m[j + 3];
                    }
                    for (int i = 16; i < 64; i++)
                    {
                        uint s0 = Ror(w[i - 15], 7) ^ Ror(w[i - 15], 18) ^ (w[i - 15] >> 3);
                        uint s1 = Ror(w[i - 2], 17) ^ Ror(w[i - 2], 19) ^ (w[i - 2] >> 10);
                        w[i] = unchecked(w[i - 16] + s0 + w[i - 7] + s1);
                    }

                    uint a = h0, b = h1, c = h2, d = h3, e = h4, f = h5, g = h6, hh = h7;
                    for (int i = 0; i < 64; i++)
                    {
                        uint bigS1 = Ror(e, 6) ^ Ror(e, 11) ^ Ror(e, 25);
                        uint ch = (e & f) ^ (~e & g);
                        uint t1 = unchecked(hh + bigS1 + ch + K[i] + w[i]);
                        uint bigS0 = Ror(a, 2) ^ Ror(a, 13) ^ Ror(a, 22);
                        uint maj = (a & b) ^ (a & c) ^ (b & c);
                        uint t2 = unchecked(bigS0 + maj);
                        hh = g; g = f; f = e;
                        e = unchecked(d + t1);
                        d = c; c = b; b = a;
                        a = unchecked(t1 + t2);
                    }

                    h0 = unchecked(h0 + a); h1 = unchecked(h1 + b);
                    h2 = unchecked(h2 + c); h3 = unchecked(h3 + d);
                    h4 = unchecked(h4 + e); h5 = unchecked(h5 + f);
                    h6 = unchecked(h6 + g); h7 = unchecked(h7 + hh);
                }

                byte[] outp = new byte[32];
                uint[] hs = { h0, h1, h2, h3, h4, h5, h6, h7 };
                for (int i = 0; i < 8; i++)
                {
                    outp[i * 4] = (byte)(hs[i] >> 24);
                    outp[i * 4 + 1] = (byte)(hs[i] >> 16);
                    outp[i * 4 + 2] = (byte)(hs[i] >> 8);
                    outp[i * 4 + 3] = (byte)hs[i];
                }
                return outp;
            }

            private static string Hex(byte[] b)
            {
                var sb = new StringBuilder(b.Length * 2);
                for (int i = 0; i < b.Length; i++) sb.Append(b[i].ToString("x2", CultureInfo.InvariantCulture));
                return sb.ToString();
            }
        }
    }
}
