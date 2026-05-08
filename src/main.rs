mod cache;
mod config;
mod feed;
mod scoring;
mod server;
mod util;

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use clap::Parser;
use config::AppConfig;
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use rmcp::{ServiceExt, transport::stdio};
use server::TechRadarServer;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Rust + rmcp 製 Tech Radar MCP Server の起動引数です。
///
/// MCP server は MCP Host から子プロセスとして起動されるため、
/// 設定ファイルの path は CLI 引数で渡せるようにしています。
#[derive(Debug, Parser)]
#[command(name = "tech-radar-mcp-rs")]
#[command(about = "RSS / Atom から技術トレンドを返す Rust 製 MCP server です。")]
struct Args {
    /// sources.toml の path です。
    #[arg(long, default_value = "config/sources.toml")]
    config: String,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("tech_radar_mcp_rs=info,warn"));

    // stderr に出すのが重要です。
    // stdout に出すと MCP stdio transport の JSON-RPC stream を壊します。
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let args = Args::parse();
    let initial_config = AppConfig::load(&args.config)
        .with_context(|| format!("設定ファイルの読み込みに失敗しました: {}", args.config))?;
    // ホットリローディング用に Arc<RwLock> 化します。
    let config_arc: Arc<RwLock<AppConfig>> = Arc::new(RwLock::new(initial_config));
    // notify watcher を起動します。
    // watcher を drop するとファイル監視が止まるため、変数に束縛して保持します。
    let _watcher = {
        let config_arc = Arc::clone(&config_arc);
        let config_path = args.config.clone();

        // notify のコールバックは同期なので、tokio channel で async 側に渡します。
        let (tx, mut rx) = mpsc::channel::<Event>(4);

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            match res {
                Ok(event) => {
                    let _ = tx.blocking_send(event);
                }
                Err(e) => {
                    // watcher 自体のエラー（対象ファイルが削除されたなど）
                    warn!("ファイル監視でエラーが発生しました: {e:#}");
                }
            }
        })?;

        watcher.watch(config_path.as_ref(), RecursiveMode::NonRecursive)
            .with_context(|| format!("ファイルの監視開始に失敗しました: {config_path}"))?;

        // 変更イベントを受け取るタスクです。
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                // Write / Create イベントのみリロードします（Access, Remove などは無視します）。
                let should_reload = matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_)
                );

                if !should_reload {
                    continue;
                }

                match AppConfig::load(&config_path) {
                    Ok(new_config) => {
                        *config_arc.write().unwrap() = new_config;
                        info!("sources.toml をリロードしました");
                    }
                    Err(e) => {
                        // parse エラー時は直前の設定を使い続けます。
                        warn!("sources.toml のリロードに失敗しました（直前の設定を継続します）: {e:#}");
                    }
                }
            }
        });

        watcher // drop 防止のため返す
    };

    let server = TechRadarServer::new(config_arc)?;

    // rmcp の stdio transport を使います。
    //
    // stdio transport では stdout が MCP の JSON-RPC message 専用になります。
    // そのため、println! や通常ログを stdout に出すと protocol が壊れます。
    // ログは init_tracing() で stderr に出力しています。
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
