//! Transports: stdio (default) and streamable HTTP.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::routing::{any_service, get};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, ConfigError};
use crate::mcp::SlackMcp;
use crate::{auth, host_guard};

const USAGE: &str = "\
usage: slack-mcp [stdio|http|--help|--version]

  stdio    serve MCP over stdin/stdout (default)
  http     serve MCP over streamable HTTP at /mcp (requires MCP_API_KEYS)

Configuration comes from environment variables only; see README.md.";

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("slack_mcp=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::from(1)
}

/// Dispatches on the first CLI argument and returns the process exit code.
pub async fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["stdio"] => run_stdio().await,
        ["http"] => run_http().await,
        ["-h" | "--help"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        ["-V" | "--version"] => {
            println!("slack-mcp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown arguments: {}\n{USAGE}", other.join(" "));
            ExitCode::from(2)
        }
    }
}

fn load_config() -> Result<Config, ConfigError> {
    Config::from_env()
}

async fn run_stdio() -> ExitCode {
    init_tracing();
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let mcp = match SlackMcp::new(&cfg) {
        Ok(m) => m,
        Err(e) => return fail(e),
    };
    tracing::info!("serving Slack MCP over stdio");
    let running = match rmcp::serve_server(mcp, (tokio::io::stdin(), tokio::io::stdout())).await {
        Ok(r) => r,
        Err(e) => return fail(format!("stdio initialization failed: {e}")),
    };
    if let Err(e) = running.waiting().await {
        tracing::error!("stdio session ended with error: {e}");
    }
    ExitCode::SUCCESS
}

/// Router for HTTP mode: Host guard, then API key on `/mcp`; `/health` needs no key.
pub fn build_router(cfg: &Config) -> Result<Router, crate::slack::SlackError> {
    let mcp = SlackMcp::new(cfg)?;
    let mut mcp_config = StreamableHttpServerConfig::default();
    mcp_config.legacy_session_mode = false;
    mcp_config.json_response = true;
    let mcp_config = mcp_config.disable_allowed_hosts();
    let service = StreamableHttpService::new(
        move || Ok(mcp.clone()),
        Arc::new(NeverSessionManager::default()),
        mcp_config,
    );
    let allowed = Arc::new(host_guard::build_allowlist(&cfg.host, &cfg.allowed_hosts));
    let mcp_route = Router::new()
        .route("/mcp", any_service(service))
        .route_layer(from_fn_with_state(
            Arc::new(cfg.api_keys.clone()),
            auth::require_api_key,
        ));
    Ok(Router::new()
        .route("/health", get(|| async { (StatusCode::OK, "ok") }))
        .merge(mcp_route)
        .layer(from_fn_with_state(
            allowed,
            host_guard::require_allowed_host,
        ))
        .layer(TraceLayer::new_for_http()))
}

async fn run_http() -> ExitCode {
    init_tracing();
    let cfg = match load_config().and_then(|c| c.require_api_keys().map(|()| c)) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let app = match build_router(&cfg) {
        Ok(a) => a,
        Err(e) => return fail(e),
    };
    let addr: SocketAddr = match format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .or_else(|_| format!("[{}]:{}", cfg.host, cfg.port).parse())
    {
        Ok(a) => a,
        Err(_) => return fail("MCP_HOST must be an IP address"),
    };
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return fail(format!("cannot bind {addr}: {e}")),
    };
    if !addr.ip().is_loopback() {
        tracing::warn!(
            "binding {addr}, which is not loopback: keep API keys secret and use TLS in front"
        );
    }
    tracing::info!(
        "serving Slack MCP over HTTP on {addr} ({} API key(s))",
        cfg.api_keys.len()
    );
    let shutdown = async {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("shutting down");
    };
    match axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}
