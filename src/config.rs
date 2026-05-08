use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use url::Url;

/// MCP server 全体の設定です。
///
/// `sources.toml` を source registry として読み込みます。
/// OPML のような単純な購読リストではなく、priority / topics / freshness policy など、
/// MCP 側で判断するための metadata を持たせます。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    /// tool 呼び出し時に `since_hours` が省略された場合の既定値です。
    #[serde(default = "default_freshness_hours")]
    pub default_freshness_hours: i64,

    /// 1 source から読む item 数の上限です。
    ///
    /// feed に大量の item がある場合でも、MCP に渡す候補を無制限に増やさないための措置です。
    #[serde(default = "default_max_items_per_source")]
    pub max_items_per_source: usize,

    /// feed response body の最大 byte 数です。
    ///
    /// 壊れた feed や想定外に巨大な response で memory を消費しないように制限します。
    #[serde(default = "default_max_feed_bytes")]
    pub max_feed_bytes: usize,

    /// HTTP request timeout 秒数です。
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,

    /// process-local cache の TTL 秒数です。
    ///
    /// MCP client が連続して tool を呼ぶことがあるため、同じ feed を毎回取りに行かないようにします。
    #[serde(default = "default_cache_ttl_seconds")]
    pub cache_ttl_seconds: i64,

    /// 要約の最大文字数です。
    #[serde(default = "default_max_summary_chars")]
    pub max_summary_chars: usize,

    /// 取得対象 source の一覧です。
    #[serde(default)]
    pub sources: Vec<SourceConfig>,

    /// topic 検出ルールとスコア加算の定義です。
    /// sources.toml の [[topic_rules]] セクションで管理します。
    #[serde(default)]
    pub topic_rules: Vec<TopicRule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TopicRule {
    /// topic の識別名です。例: "rust", "mcp"
    pub topic: String,

    /// この topic と判定するキーワード群です。title / summary に含まれるか確認します。
    #[serde(default)]
    pub keywords: Vec<String>,

    /// スコアへの加算値です。
    #[serde(default)]
    pub boost: i32,
}

/// 1 つの RSS / Atom source の設定です。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SourceConfig {
    /// source を一意に識別する ID です。
    ///
    /// cache key / log / result に出すため、英数字 / `-` / `_` のみに制限します。
    pub id: String,

    /// 人間向けの表示名です。
    pub name: String,

    /// source の種類です。
    ///
    /// 現時点では RSS / Atom を `feed-rs` でまとめて parse するため、実装上は同じ扱いです。
    #[allow(dead_code)]
    pub kind: SourceKind,

    /// RSS / Atom feed の URL です。
    pub url: String,

    /// 元サイトの URL です。
    #[serde(default)]
    pub homepage: Option<String>,

    /// source の信頼度、性格です。
    ///
    /// scoring では official / security を高めに評価します。
    pub source_type: SourceType,

    /// source 全体の基本優先度です。0〜100 を想定します。
    #[serde(default = "default_source_priority")]
    pub priority: i32,

    /// source に関連するトピック群です。
    ///
    /// item title / summary から推定したトピックとマージして使います。
    #[serde(default)]
    pub topics: Vec<String>,

    /// false の source は取得対象から外します。
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// true の場合、published / updated が取れない item を drop します。
    ///
    /// 「最新ニュース」用途では日付が取れない item は信用しにくいため、既定では true にしています。
    #[serde(default = "default_require_published_at")]
    pub require_published_at: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum SourceKind {
    Rss,
    Atom,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceType {
    Official,
    Changelog,
    Community,
    Security,
    Media,
}

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("設定ファイルを読めませんでした: {}", path.as_ref().display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("TOML の parse に失敗しました: {}", path.as_ref().display()))?;

        config.validate()?;

        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.sources.is_empty() {
            bail!("少なくとも 1 つの source が必要です");
        }

        if self.default_freshness_hours <= 0 {
            bail!("default_freshness_hours は正の整数である必要があります");
        }

        if self.max_items_per_source == 0 {
            bail!("max_items_per_source は正の整数である必要があります");
        }

        if self.max_feed_bytes == 0 {
            bail!("max_feed_bytes は正の整数である必要があります");
        }

        if self.max_summary_chars == 0 {
            bail!("max_summary_chars は正の整数である必要があります");
        }

        if self.request_timeout_seconds == 0 {
            bail!("request_timeout_seconds は正の整数である必要があります");
        }

        if self.cache_ttl_seconds <= 0 {
            bail!("cache_ttl_seconds は正の整数である必要があります");
        }

        for source in &self.sources {
            source.validate()?;
        }

        Ok(())
    }
}

impl SourceConfig {
    fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("source id は空にできません");
        }

        if !is_safe_id(&self.id) {
            bail!("source id に使えない文字が含まれています: {}", self.id);
        }

        if self.name.trim().is_empty() {
            bail!("source name は空にできません: {}", self.id);
        }

        let url = Url::parse(&self.url)
            .with_context(|| format!("source URL が不正です: {}", self.id))?;

        match url.scheme() {
            "https" | "http" => {}
            scheme => bail!(
                "source URL の scheme は http / https のみ対応です: {} ({})",
                self.id,
                scheme
            ),
        }

        if self.priority < 0 || self.priority > 100 {
            bail!("priority は 0..=100 である必要があります: {}", self.id);
        }

        Ok(())
    }
}

/// source id を cache key や log に安全に使うため、文字種を制限します。
fn is_safe_id(value: &str) -> bool {
    value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn default_freshness_hours() -> i64 {
    24
}

fn default_max_items_per_source() -> usize {
    50
}

fn default_max_feed_bytes() -> usize {
    10 * 1024 * 1024
}

fn default_request_timeout_seconds() -> u64 {
    5
}

fn default_cache_ttl_seconds() -> i64 {
    15 * 60
}

fn default_max_summary_chars() -> usize {
    1000
}

fn default_source_priority() -> i32 {
    50
}

fn default_enabled() -> bool {
    true
}

fn default_require_published_at() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_id_accepts_expected_chars() {
        assert!(is_safe_id("github-changelog"));
        assert!(is_safe_id("zenn_mcp"));
        assert!(!is_safe_id("../secret"));
        assert!(!is_safe_id("has space"));
    }
}
