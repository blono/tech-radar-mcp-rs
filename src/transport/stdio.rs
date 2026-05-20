//! stdio トランスポートでサーバを起動する。
//!
//! 用途: Claude Desktop 等、子プロセスとして起動する MCP クライアント向け。
//!
//! 注意: stdio transport の場合、stdout は MCP の JSON-RPC で占有されるため、
//! ログは絶対に stderr に出すこと（println! でうっかり stdout に書くと、クライアントが「不正な JSON が来た」とエラーで切断する）。

use crate::server::TechRadarServer;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing::info;

/// stdio transport でサーバを起動し、クライアント切断まで待機する。
pub async fn run(server_impl: TechRadarServer) -> anyhow::Result<()> {
    info!("MCP stdio transport を開始します");

    let service = server_impl.serve(stdio()).await.inspect_err(|e| {
        tracing::error!(error = %e, "stdio transport の起動に失敗しました");
    })?;

    // クライアント切断まで待機
    let quit_reason = service.waiting().await?;
    info!(?quit_reason, "stdio transport を終了します");

    Ok(())
}
