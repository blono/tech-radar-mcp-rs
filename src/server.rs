use crate::config::AppConfig;
use crate::feed::{FeedService, FetchOptions, Freshness};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// MCP server の本体です。
///
/// `rmcp` の `#[tool_router]` macro により、
/// `#[tool]` が付いた method から tool router が生成されます。
///
/// feed 取得、parse, ランキングは `FeedService` に閉じ込め、このファイルでは MCP tool としての入出力に集中します。
#[derive(Clone)]
pub struct TechRadarServer {
    feed_service: FeedService,
}

/// `since`（絶対時刻）指定時に threshold を巻き戻す分数です。
///
/// RSS / Atom は feed への反映ラグがあり、`published_at` が前回 `generated_at` より
/// わずかに前なのに、feed に載るのはその後、という item が発生します。
/// `since` をちょうど前回 `generated_at` にすると、こういう item を静かに取りこぼすため、
/// 少しだけ過去に巻き戻して取りこぼしを防ぎます。重複は URL / title の dedupe で吸収されます。
///
/// なお、この巻き戻しは `since` パスのみに効かせ、`since_hours`（24h / 48h などの固定窓）には
/// 効かせません。固定窓は「ちょうどその時間」が欲しく、連続 fetch の継ぎ目問題も無いためです。
const SINCE_OVERLAP_MINUTES: i64 = 10;

impl TechRadarServer {
    pub fn new(config: Arc<RwLock<AppConfig>>) -> Result<Self> {
        Ok(Self {
            feed_service: FeedService::new(config)?,
        })
    }
}

/// `get_latest_technology_news` の引数 schema 用 struct です。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LatestNewsArgs {
    #[schemars(
        description = "絶対時刻の下限です（RFC3339 / ISO8601、例: 2026-06-11T10:35:00Z）。\
        前回この MCP を呼んだときのレスポンスの generated_at をそのまま渡すと、\
        その時刻以降の新着 item だけを返します。指定した場合は since_hours より優先され、\
        since_hours は無視されます。「前回の続き」を取りたいときはこちらを使ってください。"
    )]
    pub since: Option<DateTime<Utc>>,

    #[schemars(description = "何時間以内の記事だけを含めるか。")]
    pub since_hours: Option<i64>,

    #[schemars(description = "返す記事数の上限。")]
    pub max_items: Option<usize>,

    #[schemars(description = "topic filter です。例: rust, typescript, aws, mcp, ai, security")]
    #[serde(default)]
    pub topics: Vec<String>,

    #[schemars(description = "feed が提供する要約を含めるか。")]
    pub include_summary: Option<bool>,
}

/// `get_trending_technology_topics` の引数 schema 用 struct です。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TrendingTopicsArgs {
    #[schemars(
        description = "絶対時刻の下限です（RFC3339 / ISO8601）。前回レスポンスの generated_at を渡すと、\
        その時刻以降の記事だけを集計します。指定した場合は since_hours より優先されます。"
    )]
    pub since: Option<DateTime<Utc>>,

    #[schemars(description = "何時間以内の記事だけを対象にするか。")]
    pub since_hours: Option<i64>,

    #[schemars(description = "返すトピック数の上限。")]
    pub max_topics: Option<usize>,
}

/// source health report の引数 schema 用 struct です。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct SourceHealthArgs {
    #[schemars(
        description = "将来、disabled source も含める option を追加するための予約 field です。"
    )]
    pub include_disabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TrendingTopic {
    pub topic: String,
    pub count: usize,
    pub top_titles: Vec<String>,
}

/// rmcp の macro で MCP tools を登録します。
///
/// tool 名は function 名になります。
/// そのため、ユーザーが理解しやすく、かつ LLM が選びやすい名前にします。
#[tool_router]
impl TechRadarServer {
    #[tool(
        description = "最新の技術ニュースやエンジニア向けトレンドを知りたいときに使います。設定済みの RSS / Atom source を取得し、公開日時で絞り込み、重複除去とスコアリングを行って、source URL 付きで新しい記事だけを返します。"
    )]
    pub async fn get_latest_technology_news(
        &self,
        Parameters(args): Parameters<LatestNewsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let config = self.feed_service.config_snapshot();

        // since / since_hours の排他・優先順位・デフォルト・clamp を resolve_freshness に集約します。
        let freshness =
            resolve_freshness(args.since, args.since_hours, config.default_freshness_hours);
        let max_items = args.max_items.unwrap_or(100).clamp(1, 300);
        let include_summary = args.include_summary.unwrap_or(true);

        let result = self
            .feed_service
            .latest_items(FetchOptions {
                freshness,
                overlap_minutes: SINCE_OVERLAP_MINUTES,
                max_items,
                topics: args.topics,
                include_summary,
            })
            .await;

        to_call_tool_result(&result)
    }

    #[tool(
        description = "設定済みの技術系 feed 全体で、今何のトピックが多く出ているかを知りたいときに使います。指定時間以内の記事をトピック別に集計し、代表タイトルと件数を返します。"
    )]
    pub async fn get_trending_technology_topics(
        &self,
        Parameters(args): Parameters<TrendingTopicsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let config = self.feed_service.config_snapshot();
        let freshness =
            resolve_freshness(args.since, args.since_hours, config.default_freshness_hours);
        let max_topics = args.max_topics.unwrap_or(20).clamp(1, 50);
        let result = self
            .feed_service
            .latest_items(FetchOptions {
                freshness,
                overlap_minutes: SINCE_OVERLAP_MINUTES,
                max_items: 200,
                topics: vec![],
                include_summary: false,
            })
            .await;
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for item in &result.items {
            for topic in &item.topics {
                map.entry(topic.clone())
                    .or_default()
                    .push(item.title.clone());
            }
        }

        let mut topics: Vec<TrendingTopic> = map
            .into_iter()
            .map(|(topic, mut titles)| {
                titles.sort();
                titles.dedup();

                let count = titles.len();
                titles.truncate(10);

                TrendingTopic {
                    topic,
                    count,
                    top_titles: titles,
                }
            })
            .collect();

        topics.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.topic.cmp(&b.topic)));
        topics.truncate(max_topics);

        to_call_tool_result(&json!({
            "generated_at": result.generated_at,
            "effective_since": result.effective_since,
            "topics": topics,
            "dropped": result.dropped,
        }))
    }

    #[tool(
        description = "この MCP server が参照する RSS / Atom 一覧を確認したいときに使います。source id, URL, topic, priority, source type などを返します。"
    )]
    pub async fn list_sources(&self) -> Result<CallToolResult, McpError> {
        to_call_tool_result(&json!({
            "sources": self.feed_service.config_snapshot().sources,
        }))
    }

    #[tool(
        description = "feed 設定のデバッグに使います。enabled な source を 1 回ずつ取得し、到達可能か、RSS / Atom として parse できるか、何件 item が取れたかを返します。"
    )]
    pub async fn get_source_health_report(
        &self,
        // Parameters(args): Parameters<SourceHealthArgs>,
    ) -> Result<CallToolResult, McpError> {
        let reports = self.feed_service.source_health().await;
        let ok_count = reports.iter().filter(|report| report.ok).count();
        let failed_count = reports.len().saturating_sub(ok_count);

        to_call_tool_result(&json!({
            "generated_at": chrono::Utc::now(),
            "ok_count": ok_count,
            "failed_count": failed_count,
            "sources": reports,
        }))
    }

    #[tool(
        description = "この MCP server が記事をどのように絞り込み、drop し、スコア順に並べているかを説明します。なぜ特定の記事が含まれないのかを確認したいときに使います。"
    )]
    pub async fn explain_ranking_policy(&self) -> String {
        [
            "ランキング方針:",
            "- sources.toml で enabled=true の RSS / Atom source を取得します。",
            "- source ごとに max_items_per_source 件まで item を読みます。",
            "- published / updated が取れない item は、require_published_at=true の source では drop します。",
            "- since_hours より古い item は drop します。",
            "- URL があれば URL、なければ title で重複除去します。",
            "- source priority, source type, 鮮度、topic によって score を付けます。",
            "- official / changelog / security source は community source より高めに評価します。",
            "- items だけでなく dropped stats も返し、除外理由を説明できるようにします。",
            "- 任意 URL の本文 fetch は行わず、RSS / Atom の metadata と要約のみ扱います。",
        ]
        .join("\n")
    }
}

/// `#[tool_handler]` が `tools/list` と `tools/call` を実装します。
///
/// tools-only なら `#[tool_router(server_handler)]` でも書けます。
/// ただし server name / version / instructions を明示したいため、
/// 明示的に `impl ServerHandler` を置いています。
#[tool_handler(
    name = "tech-radar-mcp-rs",
    version = "0.1.0",
    instructions = "この MCP server は、設定済み RSS / Atom source から最新の技術ニュース、技術トレンド、source health を返します。回答前に公開日時で絞り込み、重複除去とスコアリングを行います。任意 URL の本文取得は行いません。"
)]
impl ServerHandler for TechRadarServer {}

/// `since` / `since_hours` から内部用の `Freshness` を解決します。
///
/// 排他ルール・優先順位・デフォルト・clamp をこの 1 箇所に閉じ込めるための関数です。
/// - `since`（絶対時刻）があれば最優先。`since_hours` は無視します。
/// - 無ければ `since_hours`（無ければ default）を 1〜720 時間（= 24 * 30）に clamp した相対窓にします。
fn resolve_freshness(
    since: Option<DateTime<Utc>>,
    since_hours: Option<i64>,
    default_hours: i64,
) -> Freshness {
    match since {
        Some(at) => Freshness::Since(at),
        None => {
            let hours = since_hours.unwrap_or(default_hours).clamp(1, 24 * 30);
            Freshness::Hours(hours)
        }
    }
}

fn to_call_tool_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json_str = serde_json::to_string(value).map_err(|e| {
        McpError::internal_error(
            "tool result の JSON serialization に失敗しました",
            Some(json!({ "error": e.to_string() })),
        )
    })?;
    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}
