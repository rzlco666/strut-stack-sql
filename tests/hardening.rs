//! Hardening: SQLite allowlist, per-source circuit breaker, per-principal
//! concurrency limit (R2-08).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::json;
use strut_sql_core::config::{load_config, Cli};
use strut_sql_core::registry::LazyDriver;
use strut_sql_driver_api::{ConnectOptions, EngineDriver, EngineKind};
use strut_sql_driver_sqlite::SqliteDriver;
use strut_stack_sql::config::{AppConfig, ResolvedSource, WriteMode};
use strut_stack_sql::core::auth::{Policy, Tier};
use strut_stack_sql::tools::core::{handle_execute_sql, principal_tag, ExecuteSqlParams};

fn cli(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("cli")
}

#[tokio::test]
async fn sqlite_sources_outside_the_allowlist_are_rejected_at_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let allowed = dir.path().join("allowed");
    std::fs::create_dir_all(&allowed).expect("mkdir");
    let inside = allowed.join("app.db");
    let outside = dir.path().join("outside.db");

    let ok = cli(&[
        "strut-stack-sql",
        "--url",
        &format!("sqlite://{}", inside.display()),
        "--sqlite-allowlist",
        &allowed.display().to_string(),
    ]);
    load_config(&ok).await.expect("inside allowlist");

    let denied = cli(&[
        "strut-stack-sql",
        "--url",
        &format!("sqlite://{}", outside.display()),
        "--sqlite-allowlist",
        &allowed.display().to_string(),
    ]);
    let err = load_config(&denied).await.expect_err("outside allowlist");
    assert!(err.to_string().contains("outside the allowlist"), "{err}");

    // In-memory databases are always allowed.
    let memory = cli(&[
        "strut-stack-sql",
        "--url",
        "sqlite::memory:",
        "--sqlite-allowlist",
        &allowed.display().to_string(),
    ]);
    load_config(&memory).await.expect("memory allowed");
}

#[tokio::test]
async fn circuit_breaker_fails_fast_after_five_connect_failures() {
    let lazy = LazyDriver::new(
        "postgresql://user:secret@127.0.0.1:1/db".into(),
        EngineKind::Postgres,
        1,
        true,
        Duration::from_millis(100),
        Duration::from_secs(1),
    );
    for _ in 0..5 {
        let _ = lazy.get().await;
    }
    let started = Instant::now();
    let err = match lazy.get().await {
        Ok(_) => panic!("breaker should be open"),
        Err(err) => err,
    };
    let elapsed = started.elapsed();
    assert!(err.to_string().contains("circuit breaker"), "{err}");
    assert!(
        elapsed < Duration::from_millis(250),
        "breaker must fail fast, took {elapsed:?}"
    );

    // A healthy driver never trips the breaker.
    let ok = LazyDriver::new(
        "sqlite::memory:".into(),
        EngineKind::Sqlite,
        1,
        false,
        Duration::from_secs(2),
        Duration::from_secs(10),
    );
    for _ in 0..10 {
        ok.get().await.expect("sqlite connects");
    }
}

async fn config_with_limiter(per_principal: usize) -> (Arc<AppConfig>, Arc<dyn EngineDriver>) {
    std::env::set_var("MCP_SQL_FORMAT", "columnar");
    let driver = SqliteDriver::from_connection_string_with_options(
        "sqlite::memory:",
        &ConnectOptions {
            pool_size: 1,
            read_only: false,
            ..ConnectOptions::default()
        },
    )
    .await
    .expect("sqlite");
    let driver: Arc<dyn EngineDriver> = Arc::new(*driver);
    let mut sources = std::collections::HashMap::new();
    sources.insert(
        "default".into(),
        ResolvedSource::with_driver(
            "default",
            "sqlite::memory:".into(),
            EngineKind::Sqlite,
            driver.clone(),
            false,
        ),
    );
    let config = AppConfig {
        write_mode: WriteMode::AllowDdl,
        full_tools: false,
        max_rows: 100,
        max_bytes: 64 * 1024,
        query_timeout: Duration::from_secs(5),
        batch_concurrency: 1,
        batch_max_statements: 50,
        batch_timeout: Duration::from_secs(60),
        batch_max_bytes: 1024 * 1024,
        fail_fast: false,
        deny_columns: vec![],
        allow_explain_analyze: false,
        principal_id: "local".into(),
        query_history: false,
        default_source: "default".into(),
        sources,
        searched_paths: vec![],
        workspace: None,
        http_tokens: vec![],
        http_rate_limit_per_minute: 600,
        allow_unauthenticated_http: false,
        allow_grants: false,
        oauth: None,
        policy: Policy::new(Tier::Ddl, false, false).with_local_default(),
        require_approval: false,
        approvals: Arc::new(strut_stack_sql::core::approvals::ApprovalQueue::new()),
        plans: Arc::new(strut_stack_sql::core::migrations::PlanStore::new()),
        trace_sql: false,
        sqlite_allowlist: Vec::new(),
        limiter: Arc::new(strut_stack_sql::core::limits::PrincipalLimiter::new(
            per_principal,
        )),
        tables: Default::default(),
        ephemeral: false,
        hide_endpoints: None,
    };
    (Arc::new(config), driver)
}

fn params(sql: &str) -> ExecuteSqlParams {
    serde_json::from_value(json!({ "sql": sql })).expect("params")
}

#[tokio::test]
async fn one_principal_cannot_exhaust_the_query_slots() {
    let (config, _driver) = config_with_limiter(1).await;
    let tag = principal_tag(&config.principal_id);
    let held = config.limiter.try_acquire(&tag).expect("slot");

    let err = handle_execute_sql(&config, params("SELECT 1"))
        .await
        .expect_err("no slot left");
    assert!(err.message.contains("maximum concurrent queries"), "{err}");

    drop(held);
    handle_execute_sql(&config, params("SELECT 1"))
        .await
        .expect("slot released");
}

#[test]
fn allowlist_paths_are_extracted_from_sqlite_urls() {
    use strut_sql_core::config::sqlite_path;
    assert_eq!(sqlite_path("sqlite::memory:"), None);
    assert_eq!(
        sqlite_path("sqlite:///tmp/a.db"),
        Some(PathBuf::from("/tmp/a.db"))
    );
    assert_eq!(
        sqlite_path("sqlite:a.db?mode=ro"),
        Some(PathBuf::from("a.db"))
    );
}
