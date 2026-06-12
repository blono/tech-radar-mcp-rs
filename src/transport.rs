//! MCP サーバのトランスポート層。
//!
//! 設計方針:
//! - stdio（Claude Desktop 等のローカル MCP クライアント用）
//! - Streamable HTTP（claude.ai / ChatGPT 等のリモート MCP クライアント、および Cloud Run 用）
//! - 両方のトランスポートで同じ DiaryServer 実装を使い回す。
//!   トランスポートの違いを上位層に漏らさないのがポイント。

pub mod http;
pub mod stdio;
