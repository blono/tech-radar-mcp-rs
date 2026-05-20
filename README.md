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

## Transport

本 MCP は 2 種類の transport をサポートします。 CLI 引数 `--transport` で切り替え、デフォルトは `stdio` です。

| transport | 用途 |
|---|---|
| `stdio` | MCP host から子プロセスとして起動される従来の動作（デフォルト） |
| `http` | Streamable HTTP transport で `0.0.0.0:{port}` を listen。Cloud Run などへの公開を想定 |

### stdio (default)

```bash
cargo run -- --config config/sources.toml
```

### HTTP

HTTP transport は外部公開を想定するため、認証必須です。  
HTTP Basic 認証を採用しており、環境変数 `MCP_AUTH_USER` / `MCP_AUTH_PASSWORD` の両方を設定してから起動してください（どちらか未設定または空文字なら起動時に異常終了します）。

Claude も ChatGPT もカスタムコネクタは `Authorization: Bearer` などのヘッダの設定ができず、半面、URL 形式 `https://user:pass@host/mcp` で Basic 認証を送ってくれる仕様のため、本 MCP も Basic 認証で受けます。

```bash
# 開発用に適当な credential を発行（本番では十分長いランダム文字列を使ってください）
export MCP_AUTH_USER="claude"
export MCP_AUTH_PASSWORD="$(openssl rand -base64 64)"

# 起動（デフォルト port 8080）
cargo run -- --transport=http --config config/sources.toml

# 動作確認
curl -fsS http://127.0.0.1:8080/health
# → 200 OK

# 認証なしは 401
curl -i http://127.0.0.1:8080/mcp
# → HTTP/1.1 401 Unauthorized
# → WWW-Authenticate: Basic realm="MCP"

# 正しい credential なら通過
curl -i -u "${MCP_AUTH_USER}:${MCP_AUTH_PASSWORD}" http://127.0.0.1:8080/mcp
```

### Port の優先順位

HTTP transport で listen する port は次の優先順位で決定します。

1. 環境変数 `PORT`（Cloud Run などほとんどのクラウドはこれで指示してきます）
2. CLI 引数 `--port`
3. デフォルト値 `8080`

## MCP Inspector

```bash
cargo build
npx -y @modelcontextprotocol/inspector target/debug/tech-radar-mcp-rs -- --config config/sources.toml
```

## Docker

Cloud Run などへの deploy 用に Dockerfile を同梱しています。  
multi-stage build で `rust:1.83-slim-bookworm` → `gcr.io/distroless/cc-debian13:nonroot` に絞り込んでいます。

```bash
# build
docker build -t tech-radar-mcp-rs:latest .

# 起動（HTTP transport）
docker run --rm -p 8080:8080 \
    -e MCP_AUTH_USER="claude" \
    -e MCP_AUTH_PASSWORD="$(openssl rand -base64 64)" \
    tech-radar-mcp-rs:latest
```
