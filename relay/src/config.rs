//! 起動設定と TLS 終端。**#1-5 が所有する** (OWNERSHIP.md)。
//!
//! 設定は「起動時に全部検証して落ちる」方針。名簿 push が 401 の山になってから
//! 気づくより、systemd が起動に失敗したほうが早い。

use std::net::SocketAddr;
use std::path::PathBuf;

use futures::StreamExt;
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, is_tls_alpn_challenge};
use serde::Deserialize;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::LazyConfigAcceptor;

/// TLS-ALPN-01 は **443 でしか検証されない**。ここを設定可能にすると
/// 「証明書が取れない」という形でしか失敗が見えないので、定数で固定する。
/// (80 番が要らないのはこの方式を選んだ結果 — docs/deploy.md)
const HTTPS_PORT: u16 = 443;

/// ACME キャッシュの既定の置き場。`.gitignore` 済み。
const DEFAULT_ACME_CACHE_DIR: &str = ".acme-cache";

/// PWA は HTTP/2 で繋ぎたい。ALPN を出さないと HTTP/1.1 に落ちる。
const ALPN_PROTOCOLS: [&[u8]; 2] = [b"h2", b"http/1.1"];

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// 証明書を取る DNS 名
    pub domain: String,
    /// プラグインとの共有シークレット (HMAC-SHA256)
    pub hmac_secret: String,
    /// Steam Web API key (OpenID の検証には不要だが、表示名の取得に使う場合)
    pub steam_api_key: Option<String>,
    /// 音声用 UDP ポート (str0m は 1 本に多重化する)
    pub udp_port: u16,
    /// 死亡時に失効させるか。ネイティブの挙動を実測して決める (docs/protocol.md §0)
    #[serde(default = "default_true")]
    pub revoke_on_death: bool,
    /// 静的 whitelist。空なら無効 (名簿のみで認可)
    #[serde(default)]
    pub whitelist: Vec<String>,

    // ---- 以下 #1-5 が追加。すべて省略可 ----
    /// Let's Encrypt に登録する連絡先。証明書の期限切れ警告が届く。
    /// `mailto:` は付けなくてよい (無ければ補う)。
    #[serde(default)]
    pub acme_contact: Option<String>,
    /// staging ディレクトリを使うか。**本番は false (既定)**。
    /// 本番のレート制限 (同一ホストで 1 時間 5 失敗 / 1 週 50 証明書) は
    /// デプロイ検証で簡単に使い切るので、初回の疎通確認は true で行う。
    #[serde(default)]
    pub acme_staging: bool,
    /// 証明書とアカウント鍵のキャッシュ先。**消すと毎回発行しに行く**ので、
    /// 再起動をまたいで残る場所を指す必要がある。
    #[serde(default = "default_acme_cache_dir")]
    pub acme_cache_dir: PathBuf,
}

fn default_true() -> bool {
    true
}

fn default_acme_cache_dir() -> PathBuf {
    PathBuf::from(DEFAULT_ACME_CACHE_DIR)
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("設定ファイル {path} を読めない: {e}"))?;
        Self::from_toml_str(&text)
    }

    /// `load` の中身。テストから直接叩けるように分けてある。
    ///
    /// 環境変数の上書きもここに入れる — 「ファイルを読んだ結果」ではなく
    /// 「起動設定」が最終形であってほしいので、両方を通る 1 か所にまとめる。
    pub fn from_toml_str(text: &str) -> anyhow::Result<Self> {
        // serde の `Option` は「欠けていても Ok」なので、必須項目は
        // 素直に非 Option にしてある。欠けていればここで Err になる。
        let mut cfg: Config = toml::from_str(text)?;

        // 設定ファイルに秘密を置きたくない運用のための逃げ道。
        // systemd なら EnvironmentFile= を 0600 で置く (docs/deploy.md)。
        if let Ok(secret) = std::env::var("PV_HMAC_SECRET") {
            cfg.hmac_secret = secret;
        }

        cfg.validate()?;
        Ok(cfg)
    }

    /// 型では表せない条件。**空文字は「書き忘れ」であって有効な値ではない。**
    fn validate(&self) -> anyhow::Result<()> {
        if self.domain.trim().is_empty() {
            anyhow::bail!("domain が空。証明書を取る DNS 名が要る");
        }
        if self.hmac_secret.trim().is_empty() {
            anyhow::bail!("hmac_secret が空。config.toml か PV_HMAC_SECRET に設定する");
        }
        if self.udp_port == 0 {
            anyhow::bail!("udp_port が 0。音声用の UDP ポートを 1 本決める");
        }
        Ok(())
    }

    /// ACME の連絡先を `mailto:` 付きで返す。未設定なら空。
    fn acme_contacts(&self) -> Vec<String> {
        self.acme_contact
            .iter()
            .map(|c| match c.contains(':') {
                true => c.clone(),
                false => format!("mailto:{c}"),
            })
            .collect()
    }
}

/// 443 を bind し、ACME (TLS-ALPN-01) で証明書を自動取得・自動更新する listener を返す。
///
/// `axum::serve(bind_tls(&cfg).await?, app)` で使える。
///
/// **80 番は開けない。** TLS-ALPN-01 は 443 の TLS ハンドシェイクの中で検証が完結する。
/// 証明書とアカウント鍵は `cfg.acme_cache_dir` に置く。更新は
/// `AcmeState` を回している常駐タスクが勝手にやる。
pub async fn bind_tls(cfg: &Config) -> anyhow::Result<TlsListener> {
    let tcp = TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, HTTPS_PORT))
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "0.0.0.0:{HTTPS_PORT} を bind できない: {e} \
                 (非 root なら AmbientCapabilities=CAP_NET_BIND_SERVICE が要る)"
            )
        })?;
    let local_addr = tcp.local_addr()?;

    let mut state = AcmeConfig::new([&cfg.domain])
        .contact(cfg.acme_contacts())
        .cache(DirCache::new(cfg.acme_cache_dir.clone()))
        .directory_lets_encrypt(!cfg.acme_staging)
        .state();

    // 平文と challenge で ServerConfig が違う。ClientHello を見て振り分ける。
    let challenge_config = state.challenge_rustls_config();
    let mut serve_config = (*state.default_rustls_config()).clone();
    serve_config.alpn_protocols = ALPN_PROTOCOLS.iter().map(|p| p.to_vec()).collect();
    let serve_config = std::sync::Arc::new(serve_config);

    tracing::info!(
        domain = %cfg.domain,
        staging = cfg.acme_staging,
        cache = %cfg.acme_cache_dir.display(),
        "ACME (TLS-ALPN-01) を開始"
    );

    // 証明書の取得と更新は、この state を回し続けることで進む。
    tokio::spawn(async move {
        loop {
            match state.next().await {
                Some(Ok(ok)) => tracing::info!(event = ?ok, "acme"),
                Some(Err(err)) => tracing::error!(error = ?err, "acme"),
                // Stream としては終端しない実装だが、念のため。
                None => {
                    tracing::error!("ACME state が終了した。証明書は更新されない");
                    return;
                }
            }
        }
    });

    let (tx, rx) = mpsc::channel(128);

    // accept とハンドシェイクを分ける。ハンドシェイクを accept ループ上でやると、
    // 遅い 1 本が後続の accept を止める (公開 443 では効く攻撃になる)。
    tokio::spawn(async move {
        loop {
            let (stream, peer) = match tcp.accept().await {
                Ok(v) => v,
                // EMFILE などは一過性。ここで抜けると listener ごと死ぬ。
                Err(e) => {
                    tracing::warn!(error = %e, "accept 失敗");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let challenge_config = challenge_config.clone();
            let serve_config = serve_config.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                handshake(stream, peer, challenge_config, serve_config, tx).await;
            });
        }
    });

    Ok(TlsListener { local_addr, rx })
}

async fn handshake(
    stream: TcpStream,
    peer: SocketAddr,
    challenge_config: std::sync::Arc<tokio_rustls::rustls::ServerConfig>,
    serve_config: std::sync::Arc<tokio_rustls::rustls::ServerConfig>,
    tx: mpsc::Sender<(TlsStream, SocketAddr)>,
) {
    let start = match LazyConfigAcceptor::new(Default::default(), stream).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(%peer, error = %e, "ClientHello の読み取りに失敗");
            return;
        }
    };

    // Let's Encrypt からの検証接続。HTTP は喋らないのでここで閉じる。
    if is_tls_alpn_challenge(&start.client_hello()) {
        tracing::info!(%peer, "acme: TLS-ALPN-01 の検証要求を受けた");
        if let Err(e) = start.into_stream(challenge_config).await {
            tracing::warn!(%peer, error = %e, "acme: TLS-ALPN-01 の応答に失敗");
        }
        return;
    }

    match start.into_stream(serve_config).await {
        Ok(tls) => {
            // 落ちても握った側が閉じるだけ。
            let _ = tx.send((tls, peer)).await;
        }
        Err(e) => tracing::debug!(%peer, error = %e, "TLS ハンドシェイク失敗"),
    }
}

type TlsStream = tokio_rustls::server::TlsStream<TcpStream>;

/// `axum::serve` に渡せる TLS listener。
///
/// ハンドシェイク済みの接続だけが流れてくる。**peer アドレスは保持する** —
/// ここで捨てると `ConnectInfo<SocketAddr>` が使えなくなり、
/// web.rs 側 (#1-1) からは復旧できない。
pub struct TlsListener {
    local_addr: SocketAddr,
    rx: mpsc::Receiver<(TlsStream, SocketAddr)>,
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(v) => v,
            // accept ループのタスクが死んだときだけ来る。Listener の契約上
            // エラーを返せないので、上位を巻き込まずここで止まる。
            None => {
                tracing::error!("TLS accept ループが停止した");
                std::future::pending().await
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 環境変数はプロセス全体で共有なので、触るテストは直列化する。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `bind_tls` の存在意義は「main.rs がそのまま `axum::serve` に渡せる」こと。
    /// 実際に 443 を握らずにその契約だけを検証する。
    #[test]
    fn tls_listener_is_an_axum_listener() {
        fn assert_listener<L: axum::serve::Listener<Addr = SocketAddr>>() {}
        assert_listener::<TlsListener>();
    }

    fn example_toml() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/config.example.toml"
        ))
        .expect("deploy/config.example.toml が無い")
    }

    /// 配布する例が読めなければ、手順書として意味が無い。
    #[test]
    fn example_config_parses() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("PV_HMAC_SECRET") };

        let cfg = Config::from_toml_str(&example_toml()).expect("example がパースできない");
        assert_eq!(cfg.domain, "vc.example.com");
        assert_eq!(cfg.udp_port, 40000);
        assert!(cfg.revoke_on_death);
        assert!(!cfg.acme_staging);
    }

    #[test]
    fn missing_domain_is_err() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("PV_HMAC_SECRET") };

        let err = Config::from_toml_str(
            r#"
            hmac_secret = "s"
            udp_port = 40000
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("domain"), "{err}");
    }

    #[test]
    fn missing_hmac_secret_is_err() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("PV_HMAC_SECRET") };

        let err = Config::from_toml_str(
            r#"
            domain = "vc.example.com"
            udp_port = 40000
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("hmac_secret"), "{err}");
    }

    /// 空文字は「書き忘れ」。型では拾えないのでここで落とす。
    #[test]
    fn empty_hmac_secret_is_err() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("PV_HMAC_SECRET") };

        let err = Config::from_toml_str(
            r#"
            domain = "vc.example.com"
            hmac_secret = ""
            udp_port = 40000
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("hmac_secret"), "{err}");
    }

    #[test]
    fn env_overrides_hmac_secret() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("PV_HMAC_SECRET", "from-env") };

        let cfg = Config::from_toml_str(
            r#"
            domain = "vc.example.com"
            hmac_secret = "from-file"
            udp_port = 40000
            "#,
        )
        .unwrap();

        unsafe { std::env::remove_var("PV_HMAC_SECRET") };
        assert_eq!(cfg.hmac_secret, "from-env");
    }

    /// ファイル側が空でも環境変数で満たせる (秘密を repo に置かない運用)。
    #[test]
    fn env_satisfies_empty_file_secret() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("PV_HMAC_SECRET", "from-env") };

        let cfg = Config::from_toml_str(
            r#"
            domain = "vc.example.com"
            hmac_secret = ""
            udp_port = 40000
            "#,
        )
        .unwrap();

        unsafe { std::env::remove_var("PV_HMAC_SECRET") };
        assert_eq!(cfg.hmac_secret, "from-env");
    }

    #[test]
    fn revoke_on_death_defaults_to_true() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("PV_HMAC_SECRET") };

        let cfg = Config::from_toml_str(
            r#"
            domain = "vc.example.com"
            hmac_secret = "s"
            udp_port = 40000
            "#,
        )
        .unwrap();
        assert!(cfg.revoke_on_death);
        assert!(cfg.whitelist.is_empty());
        assert_eq!(cfg.acme_cache_dir, PathBuf::from(".acme-cache"));
    }

    #[test]
    fn revoke_on_death_can_be_disabled() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("PV_HMAC_SECRET") };

        let cfg = Config::from_toml_str(
            r#"
            domain = "vc.example.com"
            hmac_secret = "s"
            udp_port = 40000
            revoke_on_death = false
            "#,
        )
        .unwrap();
        assert!(!cfg.revoke_on_death);
    }

    #[test]
    fn contact_gets_mailto_prefix() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("PV_HMAC_SECRET") };

        let cfg = Config::from_toml_str(
            r#"
            domain = "vc.example.com"
            hmac_secret = "s"
            udp_port = 40000
            acme_contact = "admin@example.com"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.acme_contacts(), vec!["mailto:admin@example.com"]);
    }

    #[test]
    fn unknown_key_is_not_silently_ignored_as_missing_required() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("PV_HMAC_SECRET") };

        // 綴り間違いで必須が欠けたケースが Err になることの確認。
        let err = Config::from_toml_str(
            r#"
            domian = "vc.example.com"
            hmac_secret = "s"
            udp_port = 40000
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("domain"), "{err}");
    }
}
