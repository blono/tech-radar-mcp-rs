use crate::cache::FeedCache;
use crate::config::{AppConfig, SourceConfig, SourceType, TopicRule};
use crate::scoring::score_item;
use crate::util::{clean_summary, sha256_hex};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use feed_rs::parser;
use futures::future::join_all;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use tracing::{debug, warn};
use wreq::Client;
use wreq::redirect::Policy;
use wreq_util::Emulation;

/// MCP tool の戻り値として LLM に渡す news item です。
///
/// feed の raw item をそのまま返すのではなく、source 情報、topic, score, age を補っています。
#[derive(Debug, Clone, Serialize)]
pub struct NewsItem {
    pub id: String,
    pub title: String,
    pub source_id: String,
    pub source_name: String,
    pub source_type: SourceType,
    pub url: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub age_hours: Option<f64>,
    pub topics: Vec<String>,
    pub summary: Option<String>,
    pub score: i32,
}

/// 「なぜ候補から落ちたか」を説明するための統計です。
///
/// LLM に単に item 一覧を渡すだけだと、「なぜこの記事がないのか」を説明できないので、
/// dropped stats を返しておくことで、日付なし / 古すぎ / 重複などを補足します。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DroppedStats {
    pub too_old: usize,
    pub missing_date: usize,
    pub duplicate: usize,
    pub parse_or_fetch_error: usize,
}

/// source health check 用の結果です。
#[derive(Debug, Clone, Serialize)]
pub struct SourceHealth {
    pub source_id: String,
    pub source_name: String,
    pub ok: bool,
    pub item_count: usize,
    pub error: Option<String>,
}

/// `get_latest_technology_news` の内部結果です。
#[derive(Debug, Clone, Serialize)]
pub struct LatestNewsResult {
    pub generated_at: DateTime<Utc>,
    pub since_hours: Option<i64>,
    /// 実際に適用した絶対下限です。`since` 指定時は overlap 巻き戻し後の値、
    /// `since_hours` パスでは `generated_at - since_hours` になります。
    /// 次回 `since` に何を渡すべきかは、これではなく `generated_at` を使ってください。
    pub effective_since: DateTime<Utc>,
    pub requested_topics: Vec<String>,
    pub items: Vec<NewsItem>,
    pub dropped: DroppedStats,
}

/// 鮮度フィルタの下限の決め方です。
///
/// `since`（絶対時刻）と `since_hours`（相対窓）は本来排他なので、2 つの Option ではなく
/// enum で「どちらか一方」を型レベルで保証します（矛盾状態・未指定を表現不能にする）。
/// args 層（`LatestNewsArgs`）では LLM が片方を省略できるよう 2 つの Option のままにし、
/// server 側で resolve した結果をこの enum で内部に流します。
#[derive(Debug, Clone, Copy)]
pub enum Freshness {
    /// 絶対時刻の下限（前回 `generated_at` 等）。overlap 巻き戻しを適用します。
    Since(DateTime<Utc>),
    /// `now` からの相対窓（時間）。server 側で clamp 済みの値が入ります。
    Hours(i64),
}

/// feed 取得時の options です。
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// 鮮度フィルタの下限です。since / since_hours は排他なので enum に畳んでいます。
    pub freshness: Freshness,
    /// `Freshness::Since` のときに threshold を巻き戻す分数（feed 反映ラグ対策）。
    /// `Freshness::Hours` のときは使いません。
    pub overlap_minutes: i64,
    pub max_items: usize,
    pub topics: Vec<String>,
    pub include_summary: bool,
}

/// Feed fetching / parsing / filtering をまとめる service です。
///
/// rmcp の tool handler から直接 reqwest を触らず、ここに寄せています。
/// これにより、MCP 層は「tool の入出力」を実装し、feed 層は「外部 I/O と正規化」を実装します。
#[derive(Clone)]
pub struct FeedService {
    config: Arc<RwLock<AppConfig>>,
    client: Client,
    cache: FeedCache,
}

impl FeedService {
    pub fn new(config: Arc<RwLock<AppConfig>>) -> Result<Self> {
        // タイムアウトは起動時の設定値で固定します（Client::builder() に焼き込まれてしまうため）。
        // 変更するにはサーバーの再起動が必要です。
        let timeout = config.read().unwrap().request_timeout_seconds;

        let client = Client::builder()
            .emulation(Emulation::Chrome145) // TLS Fingerprinting
            .timeout(std::time::Duration::from_secs(timeout))
            .redirect(Policy::limited(5))
            .build()
            .context("HTTP client の初期化に失敗しました")?;

        Ok(Self {
            config,
            client,
            cache: FeedCache::default(),
        })
    }

    /// 現時点の設定 snapshot を返します。
    ///
    /// RwLock を長時間保持しないよう clone して返します。
    /// 1 回の tool 呼び出し中は同じ snapshot を使い、一貫性を保ちます。
    pub fn config_snapshot(&self) -> AppConfig {
        self.config.read().unwrap().clone()
    }

    /// enabled な source を並列取得し、freshness / topic / dedupe / scoring を適用します。
    ///
    /// この関数が Tech Radar MCP の中心処理です。
    /// LLM に渡す前に、できるだけここでフィルターします。
    pub async fn latest_items(&self, options: FetchOptions) -> LatestNewsResult {
        let config = self.config_snapshot(); // 1 回の tool 呼び出し中は同じ設定 snapshot を使います。
        let now = Utc::now();
        // threshold の決定方針:
        // - Freshness::Since は絶対下限。feed 反映ラグ対策で overlap_minutes だけ過去に巻き戻す。
        //   下限クランプはしない（前回が数分前なら数分窓で正確に取りたいため）。
        // - Freshness::Hours は now からの相対窓（overlap は効かせない）。
        // echo 用に、相対窓を使ったときだけ since_hours を Some で持ちます。
        let (threshold, since_hours) = match options.freshness {
            Freshness::Since(since) => (since - Duration::minutes(options.overlap_minutes), None),
            Freshness::Hours(hours) => (now - Duration::hours(hours), Some(hours)),
        };
        let topic_filter = normalize_topics(&options.topics);

        // source は独立しているため並列取得します。
        // 1 source が遅くても全体が極端に遅くならないように、reqwest client 側の timeout も設定済みです。
        let futures = config
            .sources
            .iter()
            .filter(|source| source.enabled)
            .map(|source| self.fetch_source_items(source, now))
            .collect::<Vec<_>>();
        let results = join_all(futures).await;

        let mut items = Vec::new();
        let mut seen_keys = HashSet::new();
        let mut dropped = DroppedStats::default();

        for result in results {
            match result {
                Ok((source_items, missing_date)) => {
                    dropped.missing_date += missing_date;
                    for mut item in source_items {
                        let published_at = match item.published_at {
                            Some(t) => t,
                            None => {
                                // ここに来るのは require_published_at=false の source のみ
                                // 閾値チェックは不可能なのでスキップして通す
                                if !topic_filter.is_empty() && !matches_topics(&item, &topic_filter)
                                {
                                    continue;
                                }

                                let key = dedupe_key(&item);
                                if !seen_keys.insert(key) {
                                    dropped.duplicate += 1;
                                    continue;
                                }

                                if !options.include_summary {
                                    item.summary = None;
                                }

                                items.push(item);
                                continue;
                            }
                        };

                        if published_at < threshold {
                            dropped.too_old += 1;
                            continue;
                        }

                        if !topic_filter.is_empty() && !matches_topics(&item, &topic_filter) {
                            continue;
                        }

                        let key = dedupe_key(&item);

                        if !seen_keys.insert(key) {
                            dropped.duplicate += 1;
                            continue;
                        }

                        if !options.include_summary {
                            item.summary = None;
                        }

                        items.push(item);
                    }
                }
                Err(error) => {
                    dropped.parse_or_fetch_error += 1;
                    warn!(error = %error, "source の取得または parse に失敗しました");
                }
            }
        }

        // score が高い順、同点なら新しい順、さらに同点なら title 順で安定化します。
        items.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.published_at.cmp(&a.published_at))
                .then_with(|| a.title.cmp(&b.title))
        });

        items.truncate(options.max_items);

        LatestNewsResult {
            generated_at: now,
            since_hours,
            effective_since: threshold,
            requested_topics: options.topics,
            items,
            dropped,
        }
    }

    /// enabled な source を 1 回ずつ取得し、設定ミスや feed 側の障害を見つけます。
    pub async fn source_health(&self) -> Vec<SourceHealth> {
        let config = self.config_snapshot();
        let futures = config
            .sources
            .iter()
            .filter(|source| source.enabled)
            .map(|source| async move {
                let source_id = source.id.clone();
                let source_name = source.name.clone();

                match self.fetch_source_items(source, Utc::now()).await {
                    Ok((items, _)) => SourceHealth {
                        source_id,
                        source_name,
                        ok: true,
                        item_count: items.len(),
                        error: None,
                    },
                    Err(error) => SourceHealth {
                        source_id,
                        source_name,
                        ok: false,
                        item_count: 0,
                        error: Some(error.to_string()),
                    },
                }
            })
            .collect::<Vec<_>>();

        join_all(futures).await
    }

    /// 1 source の feed を取得して、MCP tool の戻り値に使いやすい `NewsItem` に変換します。
    async fn fetch_source_items(
        &self,
        source: &SourceConfig,
        now: DateTime<Utc>,
    ) -> Result<(Vec<NewsItem>, usize)> {
        let config = self.config_snapshot();
        let bytes = self.fetch_source_bytes(source, &config).await?;
        let feed = parser::parse(&bytes[..])
            .with_context(|| format!("feed の parse に失敗しました: {}", source.id))?;
        let mut items = Vec::new();
        let mut missing_date_count = 0usize;

        for entry in feed.entries.into_iter().take(config.max_items_per_source) {
            let title = entry
                .title
                .as_ref()
                .map(|title| title.content.trim().to_string())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| "（タイトルなし）".to_string());
            let url = entry.links.first().map(|link| link.href.clone());

            // RSS / Atom は published / updated のどちらかしかないことがあるため、
            // published を優先し、なければ updated を使います。
            let published_at = entry
                .published
                .or(entry.updated)
                .map(|datetime| datetime.with_timezone(&Utc));

            if published_at.is_none() && source.require_published_at {
                debug!(source_id = %source.id, title = %title, "日付がない item を drop します");
                missing_date_count += 1;
                continue;
            }

            let age_hours = published_at.map(|published_at| {
                let seconds = now.signed_duration_since(published_at).num_seconds().max(0);
                seconds as f64 / 3600.0
            });
            let summary = entry
                .summary
                .as_ref()
                .map(|summary| clean_summary(&summary.content, config.max_summary_chars))
                .filter(|summary| !summary.is_empty());

            // source 固有 topic と item 本文から推定した topic をマージします。
            let mut topics = source.topics.clone();
            extend_topics_from_text(&mut topics, &title, &config.topic_rules);

            if let Some(summary) = &summary {
                extend_topics_from_text(&mut topics, summary, &config.topic_rules);
            }

            topics.sort();
            topics.dedup();

            // entry.id が空の feed もあるため、その場合には URL / title ベースの hash を使用します。
            let id = if !entry.id.trim().is_empty() {
                entry.id
            } else {
                sha256_hex(&format!("{}:{}:{:?}", source.id, title, url))
            };

            let mut item = NewsItem {
                id,
                title,
                source_id: source.id.clone(),
                source_name: source.name.clone(),
                source_type: source.source_type.clone(),
                url,
                published_at,
                age_hours,
                topics,
                summary,
                score: 0,
            };

            item.score = score_item(source, &item, &config.topic_rules);
            items.push(item);
        }

        Ok((items, missing_date_count))
    }

    /// raw feed bytes を取得します。
    ///
    /// ここでは以下を実施します。
    ///
    /// - TTL cache
    /// - timeout
    /// - redirect limit
    /// - response status check
    /// - response size limit
    async fn fetch_source_bytes(
        &self,
        source: &SourceConfig,
        config: &AppConfig,
    ) -> Result<Arc<Vec<u8>>> {
        if let Some(cached) = self.cache.get(&source.id, config.cache_ttl_seconds).await {
            return Ok(cached);
        }

        let response = self
            .client
            .get(&source.url)
            .send()
            .await
            .with_context(|| format!("request に失敗しました: {}", source.id))?
            .error_for_status()
            .with_context(|| format!("HTTP status が成功ではありません: {}", source.id))?;
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("response body の読み込みに失敗しました: {}", source.id))?;

        if bytes.len() > config.max_feed_bytes {
            bail!(
                "feed が大きすぎます: {} ({} bytes > {} bytes)",
                source.id,
                bytes.len(),
                config.max_feed_bytes
            );
        }

        let bytes = Arc::new(bytes.to_vec());
        self.cache.put(source.id.clone(), Arc::clone(&bytes)).await;

        Ok(bytes)
    }
}

fn normalize_topics(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
        .collect()
}

fn matches_topics(item: &NewsItem, requested_topics: &[String]) -> bool {
    item.topics
        .iter()
        .map(|topic| topic.to_lowercase())
        .any(|topic| {
            requested_topics
                .iter()
                .any(|requested| topic == *requested || topic.contains(requested))
        })
}

/// URL があれば URL, なければ title を重複判定 key にします。
fn dedupe_key(item: &NewsItem) -> String {
    item.url
        .as_ref()
        .and_then(|url_str| url::Url::parse(url_str).ok())
        .map(|mut url| {
            // 記事の同一性に影響のないパラメータなどを落とします。
            url.set_query(None); // `?ref=twitter` のようなトラッキングパラメータを無視します。
            url.set_fragment(None); // `#...` を無視します。
            url.to_string().to_lowercase()
        })
        .unwrap_or_else(|| item.title.trim().to_lowercase())
}

/// title / summary から簡易的に topic を推定します。
fn extend_topics_from_text(topics: &mut Vec<String>, text: &str, rules: &[TopicRule]) {
    let lower = text.to_lowercase();

    for rule in rules {
        if rule
            .keywords
            .iter()
            .any(|keyword| lower.contains(keyword.as_str()))
        {
            topics.push(rule.topic.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_filter_matches_partial_topic() {
        let item = NewsItem {
            id: "1".into(),
            title: "t".into(),
            source_id: "s".into(),
            source_name: "s".into(),
            source_type: SourceType::Community,
            url: None,
            published_at: None,
            age_hours: None,
            topics: vec!["typescript".into()],
            summary: None,
            score: 0,
        };

        assert!(matches_topics(&item, &["type".into()]));
        assert!(matches_topics(&item, &["typescript".into()]));
        assert!(!matches_topics(&item, &["rust".into()]));
    }
}
