use anyhow::Result;
use clap::Parser;
use strut_stack_sql::config::{load_config, Cli};
use strut_stack_sql::server::{run_http, run_stdio};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let config = load_config(&cli).await?;
    if cli.eager_connect {
        config.connect_all().await?;
    }

    if let Some(addr) = &cli.http {
        run_http(config, addr).await
    } else {
        run_stdio(config).await
    }
}

fn init_tracing() {
    // Audit events (target "audit", one per tool call — see server.rs) are
    // info-level: visible in the default filter, isolatable with
    // MCP_SQL_LOG=audit=info.
    let filter = EnvFilter::try_from_env("MCP_SQL_LOG")
        .or_else(|_| EnvFilter::try_from_env("STRUT_STACK_SQL_LOG"))
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,audit=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}
