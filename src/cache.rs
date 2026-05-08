use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// RSS / Atom の raw bytes 用 TTL cache です。
///
/// MCP client は同じ会話内で、以下のように複数 tool を連続して呼ぶ可能性があります。
///
/// - `get_latest_technology_news`
/// - `get_trending_technology_topics`
/// - `get_source_health_report`
///
/// 毎回 feed を取り直すと source 側にも自分側にも無駄が出るため、
/// process-local な短時間 cache を入れています。
///
/// 本格運用では SQLite / redb / sled などの persistent cache に差し替えてもよいです。
#[derive(Debug, Clone, Default)]
pub struct FeedCache {
    inner: Arc<RwLock<HashMap<String, CachedFeed>>>,
}

#[derive(Debug, Clone)]
struct CachedFeed {
    fetched_at: DateTime<Utc>,
    bytes: Arc<Vec<u8>>,
}

impl FeedCache {
    /// cache hit した場合だけ raw bytes を返します。
    pub async fn get(&self, key: &str, ttl_seconds: i64) -> Option<Arc<Vec<u8>>> {
        {
            let guard = self.inner.read().await;
            if let Some(entry) = guard.get(key) {
                if Utc::now().signed_duration_since(entry.fetched_at) <= Duration::seconds(ttl_seconds) {
                    return Some(Arc::clone(&entry.bytes));
                }
                // TTL 切れの場合のみ write lock を取りに行く（下の処理）
            } else {
                // key 自体が存在しない → write lock 不要
                return None;
            }
        }

        self.inner.write().await.remove(key); // TTL 切れを即削除

        None
    }

    /// feed の raw bytes を cache に保存します。
    /// key は source id を想定しています。
    pub async fn put(&self, key: String, bytes: Arc<Vec<u8>>) {
        let mut guard = self.inner.write().await;

        guard.insert(
            key,
            CachedFeed {
                fetched_at: Utc::now(),
                bytes,
            },
        );
    }
}
