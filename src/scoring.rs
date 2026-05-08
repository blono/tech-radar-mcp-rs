use crate::config::{SourceConfig, SourceType, TopicRule};
use crate::feed::NewsItem;

/// LLM に渡す前のランキングです。
///
/// 技術ニュースの「重要度」を完全に機械判定することはできません。
/// ただし、少なくとも以下は LLM 任せにせず MCP 側で安定して反映できます。
///
/// - source の信頼度
/// - source の優先度
/// - 鮮度
/// - ユーザーが関心を持ちやすい topic
///
/// ここで score を付けてから LLM に渡すことで、LLM は「候補選別」ではなく「意味づけ / 要約」に集中できます。
pub fn score_item(
    source: &SourceConfig,
    item: &NewsItem,
    topic_rules: &[TopicRule],
) -> i32 {
    let mut score = source.priority;

    score += match source.source_type {
        SourceType::Official   => 20,
        SourceType::Changelog  => 15,
        SourceType::Security   => 25,
        SourceType::Community  => 5,
        SourceType::Media      => 0,
    };

    for topic in &item.topics {
        if let Some(rule) = topic_rules.iter().find(|r| r.topic == *topic) {
            score += rule.boost;
        }
    }

    if let Some(age_hours) = item.age_hours {
        if age_hours <= 6.0 {
            score += 25;
        } else if age_hours <= 12.0 {
            score += 20;
        } else if age_hours <= 24.0 {
            score += 15;
        } else if age_hours <= 72.0 {
            score += 8;
        }
    }

    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SourceKind, SourceType};

    #[test]
    fn official_rust_item_scores_high() {
        let topic_rules = vec![TopicRule {
            topic: "rust".to_string(),
            keywords: vec!["rust".to_string()],
            boost: 50,
        }];
        let source = SourceConfig {
            id: "rust-blog".into(),
            name: "Rust Blog".into(),
            kind: SourceKind::Rss,
            url: "https://example.com/feed.xml".into(),
            homepage: None,
            source_type: SourceType::Official,
            priority: 90,
            topics: vec!["rust".into()],
            enabled: true,
            require_published_at: true,
        };

        let item = NewsItem {
            id: "1".into(),
            title: "Rust release".into(),
            source_id: source.id.clone(),
            source_name: source.name.clone(),
            source_type: source.source_type.clone(),
            url: Some("https://example.com".into()),
            published_at: None,
            age_hours: Some(3.0),
            topics: vec!["rust".into()],
            summary: None,
            score: 0,
        };

        assert!(score_item(&source, &item, &topic_rules) >= 150);
    }
}
