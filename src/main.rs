mod cache;
mod config;
mod feed;
mod scoring;
mod server;
mod transport;
mod util;

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, ValueEnum};
use config::AppConfig;
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use server::TechRadarServer;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// 使用する transport の種類です。
///
/// - `stdio`: MCP host から子プロセスとして起動される場合に使います（デフォルト）。
/// - `http`: Streamable HTTP transport で listen します。Cloud Run など、
///           リモートから利用させる場合に使います。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Transport {
    Stdio,
    Http,
}

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

    /// 使用する transport を選びます。
    /// デフォルトは stdio で、MCP host から子プロセス起動される挙動です。
    #[arg(long, value_enum, default_value_t = Transport::Stdio)]
    transport: Transport,

    /// HTTP transport で待ち受ける port 番号です。
    ///
    /// 優先順位: 環境変数 `PORT` > このフラグ > デフォルト値 8080
    /// Cloud Run は環境変数 `PORT` で待ち受け port を指示するため、
    /// それを最優先する設計です。
    #[arg(long, default_value_t = 8080)]
    port: u16,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("tech_radar_mcp_rs=info,warn"));

    // stderr に出すのが重要です。
    // stdout に出すと MCP stdio transport の JSON-RPC stream を壊します。
    // HTTP transport の場合は stdout でも問題ありませんが、
    // 両 transport で共通化するため stderr に統一しています。
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// 必須の環境変数を読み込みます。
///
/// 未設定または空文字なら、丁寧なメッセージ付きで `Err` を返します。
fn require_env(key: &str) -> Result<String> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{key} 環境変数が未設定または空です。\
                 HTTP transport は Basic 認証必須のため、設定してから起動してください。"
            )
        })
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
    // stdio / HTTP のどちらの transport でもホットリロードを有効にしたいので、
    // transport 分岐の前にここで起動しておきます。
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

    // CLI 引数で transport を切り替えます。
    match args.transport {
        Transport::Stdio => {
            transport::stdio::run(server).await?;
        }
        Transport::Http => {
            // HTTP transport は外部公開を想定するため、認証必須です。
            // Claude も ChatGPT もカスタムコネクタは Authorization: Bearer などのヘッダの設定ができず、
            // 半面、URL 形式 `https://user:pass@host/mcp` で Basic 認証を送ってくれる仕様のため、
            // 本 MCP も Basic 認証で受けます。
            let credentials = transport::http::Credentials {
                user: require_env("MCP_AUTH_USER")?,
                password: require_env("MCP_AUTH_PASSWORD")?,
            };

            // DNS rebinding 攻撃対策で、rmcp は Host header 検証を行います
            // （CVE-2026-42559 の対策、 v1.4.0 以降デフォルト有効）。
            // デフォルト allowlist は loopback only（localhost / 127.0.0.1 / ::1）のため、
            // Cloud Run など外部にデプロイする場合は、サービスのホスト名を
            // `MCP_ALLOWED_HOSTS` 環境変数（カンマ区切り）に設定してください。
            //
            // 例: MCP_ALLOWED_HOSTS=xxx.run.app,custom.example.com
            //
            // 未設定なら rmcp のデフォルト (loopback only) を維持します。
            // 設定すると指定値で完全に上書きするため、 ローカル開発も併用するなら
            // `localhost,127.0.0.1` も明示的に含めてください。
            let allowed_hosts = std::env::var("MCP_ALLOWED_HOSTS")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                });

            // port の優先順位: 環境変数 PORT > CLI --port > デフォルト 8080
            //
            // Cloud Run などほとんどのクラウドは環境変数 PORT で待ち受け port を指示するため、
            // PORT があればそれを最優先します。
            let port = std::env::var("PORT")
                .or_else(|_| std::env::var("CONTAINER_APP_PORT"))
                .or_else(|_| std::env::var("AWS_LWA_PORT"))
                .ok()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(args.port);

            info!("HTTP transport で起動します (0.0.0.0:{port})");
            transport::http::run(server, credentials, allowed_hosts, port).await?;
        }
    }

    Ok(())
}
