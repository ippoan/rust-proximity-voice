//! 起動設定。**親が所有する。**

use serde::Deserialize;

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
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(_path: &str) -> anyhow::Result<Self> {
        todo!("#1-5: config.toml の読み込み")
    }
}
