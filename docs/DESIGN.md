# Design

## OPML ではなく TOML を主設定にする理由

OPML は RSS reader の購読リスト交換には便利ですが、本 MCP としては以下が足りません。

- source type
- priority
- topics
- freshness policy
- require published date
- cache TTL
- source-specific limits

そのため、この MCP は `sources.toml` を使用します。

## Tool 粒度

低レベル tool は公開しません。

公開しないもの:

- `fetch_feed`
- `parse_feed`
- `filter_items`
- `score_items`

公開するもの:

- `get_latest_technology_news`
- `get_trending_technology_topics`
- `get_source_health_report`

LLM が使う tool は、ユーザーの目的に近い名前にします。

## URL 本文 fetch を入れない理由

`fetch_url(url)` は便利ですが、SSRF, robots, paywall, copyright, prompt injection などの懸念点があります。

この MCP では、RSS / Atom が提供する metadata と要約だけを返します。
