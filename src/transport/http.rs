//! Streamable HTTP transport の実装です。
//!
//! `rmcp` の `transport-streamable-http-server` feature が提供する
//! `StreamableHttpService` を axum 上にマウントし、`/mcp` に MCP の
//! Streamable HTTP プロトコルを公開します。
//!
//! セキュリティモデル:
//! - bind は `0.0.0.0` 固定（LAN / Cloud Run など外部からの利用を想定）。
//! - そのぶん、認証は HTTP Basic 認証必須です。資格情報は環境変数
//!   `MCP_AUTH_USER` / `MCP_AUTH_PASSWORD` から読み、 `Authorization: Basic <base64>`
//!   ヘッダで判定します。
//! - 本来であれば OAuth 2.1 / Bearer など、より新しい仕組みを使うべきですが、
//!   Claude も ChatGPT もカスタムコネクタ画面では自前のリクエストヘッダーを設定できず、
//!   URL に `https://user:pass@host/mcp` 形式で credential を入れた場合に
//!   Basic 認証を送ってくる動きをしてくれるため、それに合わせています。
//! - `/health` だけは認証不要にして、Cloud Run 等の startup / liveness probe や
//!   外形監視からの利用に備えています。

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::transport::{StreamableHttpServerConfig, streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
}};
use subtle::ConstantTimeEq;
use tokio::signal;
use tracing::{info, warn};

use crate::server::TechRadarServer;

/// HTTP Basic 認証で受け付ける資格情報です。
///
/// `MCP_AUTH_USER` / `MCP_AUTH_PASSWORD` 環境変数から構築されます。
/// 構築側 (main.rs) で空文字チェック済みである前提です。
#[derive(Debug, Clone)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

/// HTTP transport を起動します。
///
/// `0.0.0.0:{port}` で listen し、Ctrl+C / SIGTERM を受けると
/// graceful shutdown します（Cloud Run の SIGTERM に対応するため）。
///
/// # Arguments
/// - `server`: MCP server の本体です。`StreamableHttpService` の service_factory が
///   セッション確立のたびに `Clone` を要求するため、内部で `clone` して使います。
///   `TechRadarServer` は内部状態を `Arc` で持っているので、clone は高速です。
/// - `credentials`: Basic 認証の user / password です。
/// - `port`: listen する TCP port です。
pub async fn run(server: TechRadarServer, credentials: Credentials, allowed_hosts: Option<Vec<String>>, port: u16) -> Result<()> {
    // rmcp の Streamable HTTP server の config を組み立てます。
    // allowed_hosts が指定されていればそれで上書きし、 未指定ならデフォルトを保ちます。
    // デフォルトは loopback only（["localhost", "127.0.0.1", "::1"]）で、
    // これはローカル開発を想定した安全側の設定です。
    let mut config = StreamableHttpServerConfig::default();
    if let Some(hosts) = allowed_hosts {
        info!("Host header の allowlist を上書きします: {hosts:?}");
        config.allowed_hosts = hosts;
    }

    // service_factory はセッション確立のたびに呼ばれます。
    // 重い初期化（reqwest client 構築など）を毎回行わないよう、
    // 事前に構築済みの `server` を clone する形にしています。
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    // axum の middleware から参照するため、Arc にラップして state 化します。
    let credentials_state: Arc<Credentials> = Arc::new(credentials);

    // ルーティング:
    // - `/health`: 認証なし。常に 200 OK を返します。
    // - `/mcp/*`: Basic 認証必須。MCP の Streamable HTTP transport を載せます。
    //
    // `nest_service` は Router 全体に layer を伝播させると Service 側の型と
    // 噛み合わないことがあるため、認証必須ルートだけを Router にまとめて
    // そこへ layer を当ててから、認証不要ルートと `merge` するパターンにしています。
    let public_routes = Router::new().route("/health", get(health_handler));
    let private_routes = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            credentials_state.clone(),
            basic_auth_middleware,
        ));
    let app = public_routes.merge(private_routes);

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("HTTP server の bind に失敗しました: {addr}"))?;

    info!("HTTP transport ready at http://{addr}/mcp");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server が異常終了しました")?;
    info!("HTTP transport を停止しました");

    Ok(())
}

/// `Authorization: Basic <base64(user:password)>` ヘッダで認証する middleware です。
///
/// 認証に失敗した場合は 401 Unauthorized を返し、リクエストは下流ハンドラに渡しません。
/// 失敗時は path / method を warn ログに残しますが、credential 本体はログに出しません。
async fn basic_auth_middleware(
    State(expected): State<Arc<Credentials>>,
    req: Request,
    next: Next,
) -> Response {
    // 借用スコープを限定するため、判定だけ先に済ませてから req を next.run に渡します。
    // （req.headers() の参照が生きている間は req を move できないためです）
    let auth_ok = check_basic_auth(req.headers(), &expected);

    if auth_ok {
        next.run(req).await
    } else {
        // ログ用に method と path を所有権付きでコピーします。
        let method = req.method().clone();
        let path = req.uri().path().to_owned();
        warn!("認証に失敗しました: {method} {path}");

        // RFC 7617 に従い、401 には WWW-Authenticate ヘッダを含めます。
        // realm はクライアントへの表示用なので、識別できる名前なら何でも構いません。
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, r#"Basic realm="MCP""#)],
        ).into_response()
    }
}

/// `Authorization` ヘッダを parse し、Basic 認証の credential が一致するかを判定します。
///
/// 返り値が true なら認証成功です。 ヘッダ未設定、形式不正、credential 不一致など、
/// 失敗する全パターンで false を返します。
fn check_basic_auth(headers: &HeaderMap, expected: &Credentials) -> bool {
    // 1. Authorization ヘッダから "Basic " プレフィックスを剥がす
    let encoded = match headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            let (scheme, encoded) = s.split_once(' ')?;
            scheme.eq_ignore_ascii_case("basic").then(|| encoded.trim())
        })
    {
        Some(s) => s,
        None => return false,
    };

    // 2. base64 デコード
    let decoded = match BASE64.decode(encoded) {
        Ok(b) => b,
        Err(_) => return false,
    };

    // 3. UTF-8 として解釈
    //    RFC 7617 では charset の指定がない場合 ISO-8859-1 ですが、
    //    現代のクライアントはほぼ UTF-8 を送ると思われるため、ここでは UTF-8 のみ受け入れます。
    let decoded_str = match std::str::from_utf8(&decoded) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // 4. "user:password" 形式を分解
    //    password 側にコロンが含まれる可能性があるため、 最初の `:` でだけ分割します。
    let (user, password) = match decoded_str.split_once(':') {
        Some(t) => t,
        None => return false,
    };

    // 5. constant-time comparison で照合
    //    通常の == や単純なバイト列比較は、長さの違い・内容の違いの両方が
    //    処理時間に漏れ、timing 攻撃の足がかりになります。
    //    ここでは両辺を SHA-256 でハッシュして固定長（32 byte）にしてから
    //    subtle::ConstantTimeEq で比較することで、長さも内容も漏らしません。
    let user_match = hash_eq(user, &expected.user);
    let pass_match = hash_eq(password, &expected.password);

    // & にすることで、user_match が false でも pass_match の評価を省略しません。
    // hash_eq は常に 32 byte の ct_eq を実行するため、
    // user_match の真偽で処理時間が変わりません。
    user_match & pass_match
}

/// 2 つの文字列を constant-time で比較します。
///
/// 両辺を SHA-256 でハッシュして固定長（32 byte）に揃えてから
/// `subtle::ConstantTimeEq` で比較します。
/// これにより、長さの違いも内容の違いも処理時間に漏れません。
fn hash_eq(lhs: &str, rhs: &str) -> bool {
    use sha2::{Digest, Sha256};

    // ハッシュ化して固定長にしてから比較します。
    // 元の長さの違いはハッシュ後には見えなくなるため、
    // 長さ起因の timing leak がなくなります。
    let lhs_hash = Sha256::digest(lhs.as_bytes());
    let rhs_hash = Sha256::digest(rhs.as_bytes());

    lhs_hash.ct_eq(&rhs_hash).into()
}

/// ヘルスチェック handler です。
///
/// Cloud Run の startup / liveness probe から叩かれることを想定し、認証不要で常に 200 OK を返します。
async fn health_handler() -> impl IntoResponse {
    StatusCode::OK
}

/// SIGINT (Ctrl+C) / SIGTERM を待つ future です。
///
/// Cloud Run はコンテナ停止時に SIGTERM を送るため、これを拾って graceful shutdown します。
/// Windows では SIGTERM が無いので Ctrl+C のみを待ちます。
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Ctrl+C handler の installation に失敗しました");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler の installation に失敗しました")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Ctrl+C を受信しました。shutdown を開始します。");
        }
        _ = terminate => {
            info!("SIGTERM を受信しました。shutdown を開始します。");
        }
    }
}
