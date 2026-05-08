# tech-radar-mcp-rs

学習目的で作成した MCP です。  
Rust + `rmcp`( https://github.com/modelcontextprotocol/rust-sdk ) で実装しています。  
最近の技術ニュースを LLM に検索させる際の補助として利用する、という想定です。  
（それなりの結果を得られますが、別途 API 利用料を払えば Perplexity MCP が利用できるので、実用上はそちらのほうがよいと思われます）

`config/sources.toml` に定義した RSS / Atom を読み、指定時間以内の技術ニュースやトピックなどの傾向を返却します。

## Tool

| Tool | 内容 |
|---|---|
| `get_latest_technology_news` | 指定時間以内の技術ニュースを重複除去・スコアリングして返す |
| `get_trending_technology_topics` | 新着記事からトピックの出現傾向を返す |
| `list_sources` | 設定済みソース一覧を返す |
| `get_source_health_report` | 各 feed の取得 / parse 結果を返す |
| `explain_ranking_policy` | filter, drop, scoring の方針を返す |

## Build

```bash
cargo build
cargo test
```

## MCP Inspector

```bash
cargo build
npx -y @modelcontextprotocol/inspector target/debug/tech-radar-mcp-rs -- --config config/sources.toml
```
