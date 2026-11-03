//! Approval gate for destructive DDL (R2-05).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use strut_sql_driver_api::{ConnectOptions, EngineDriver, EngineKind};
use strut_sql_driver_sqlite::SqliteDriver;
use strut_stack_sql::config::{AppConfig, ResolvedSource, WriteMode};
use strut_stack_sql::core::auth::{Policy, Tier};
use strut_stack_sql::server::build_http_router;
use strut_stack_sql::tools::core::{handle_execute_sql, ExecuteSqlParams};
use strut_stack_sql::tools::ddl::{handle_drop_table, DropTableParams};
use strut_stack_sql::tools::exec::{execute_query, ExecOptions};

async fn config(require_approval: bool) -> (Arc<AppConfig>, Arc<dyn EngineDriver>) {
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
    .expect("sqlite driver");
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
        full_tools: true,
        max_rows: 100,
        max_bytes: 64 * 1024,
        query_timeout: Duration::from_secs(10),
        batch_concurrency: 4,
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
        policy: Policy::new(Tier::Ddl, true, false).with_local_default(),
        require_approval,
        approvals: std::sync::Arc::new(strut_stack_sql::core::approvals::ApprovalQueue::new()),
        plans: std::sync::Arc::new(strut_sql_core::migrations::PlanStore::new()),
        tables: Default::default(),
        ephemeral: false,
        hide_endpoints: None,
    };
    (Arc::new(config), driver)
}

fn opts() -> ExecOptions {
    ExecOptions {
        write_mode: WriteMode::AllowDdl,
        max_rows: 100,
        max_bytes: 64 * 1024,
        timeout: Duration::from_secs(10),
        limit_injected: false,
        page_offset: 0,
        page_size: None,
        deny_columns: vec![],
        masks: vec![],
        table_policy: None,
    }
}

async fn seed_table(driver: &Arc<dyn EngineDriver>) {
    execute_query(
        driver,
        "CREATE TABLE doomed (id INTEGER PRIMARY KEY)",
        &[],
        &opts(),
    )
    .await
    .expect("create");
    execute_query(driver, "INSERT INTO doomed VALUES (1)", &[], &opts())
        .await
        .expect("insert");
}

async fn table_exists(driver: &Arc<dyn EngineDriver>) -> bool {
    execute_query(driver, "SELECT count(*) AS n FROM doomed", &[], &opts())
        .await
        .is_ok()
}

fn drop_params(confirm: bool, approval_id: Option<String>) -> DropTableParams {
    DropTableParams {
        table: "doomed".into(),
        schema: None,
        confirm: Some(confirm),
        approval_id,
        statements: None,
        plan_id: None,
        dry_run: None,
        source: None,
    }
}

#[tokio::test]
async fn destructive_ddl_needs_an_accepted_approval() {
    let (config, driver) = config(true).await;
    seed_table(&driver).await;

    // confirm:true is a legacy flag, not an approval.
    let err = handle_drop_table(&config, drop_params(true, None))
        .await
        .expect_err("approval required");
    assert_eq!(
        err.data.as_ref().expect("data")["code"],
        json!("approval_required")
    );
    let id = err.data.as_ref().expect("data")["approval_id"]
        .as_str()
        .expect("approval id")
        .to_string();
    assert!(
        table_exists(&driver).await,
        "table must not be dropped without approval"
    );

    // Pending approval is not enough.
    let err = handle_drop_table(&config, drop_params(true, Some(id.clone())))
        .await
        .expect_err("still pending");
    assert_eq!(
        err.data.as_ref().expect("data")["code"],
        json!("approval_pending")
    );
    assert!(table_exists(&driver).await);

    // Accept, then retry with the id: exactly one execution.
    assert!(config.approvals.decide(&id, "local", true).is_some());
    handle_drop_table(&config, drop_params(true, Some(id.clone())))
        .await
        .expect("accepted approval executes");
    assert!(!table_exists(&driver).await);

    // The approval is single-use.
    let err = handle_drop_table(&config, drop_params(true, Some(id)))
        .await
        .expect_err("replay");
    assert_eq!(
        err.data.as_ref().expect("data")["code"],
        json!("approval_rejected")
    );
}

#[tokio::test]
async fn rejected_and_expired_approvals_fail_closed() {
    let (config, driver) = config(true).await;
    seed_table(&driver).await;

    let err = handle_drop_table(&config, drop_params(true, None))
        .await
        .expect_err("request");
    let id = err.data.as_ref().expect("data")["approval_id"]
        .as_str()
        .expect("id")
        .to_string();
    assert!(config.approvals.decide(&id, "local", false).is_some());
    let err = handle_drop_table(&config, drop_params(true, Some(id)))
        .await
        .expect_err("rejected");
    assert_eq!(
        err.data.as_ref().expect("data")["code"],
        json!("approval_rejected")
    );

    // Another principal cannot accept the pending request.
    let err = handle_drop_table(&config, drop_params(true, None))
        .await
        .expect_err("request");
    let id = err.data.as_ref().expect("data")["approval_id"]
        .as_str()
        .expect("id")
        .to_string();
    assert!(config.approvals.decide(&id, "someone-else", true).is_none());
    assert!(table_exists(&driver).await);
}

#[tokio::test]
async fn raw_drop_via_execute_sql_is_gated_too() {
    let (config, driver) = config(true).await;
    seed_table(&driver).await;
    let params: ExecuteSqlParams =
        serde_json::from_value(json!({ "sql": "DROP TABLE doomed" })).expect("params");
    let err = handle_execute_sql(&config, params)
        .await
        .expect_err("approval required");
    assert_eq!(
        err.data.as_ref().expect("data")["code"],
        json!("approval_required")
    );
    assert!(table_exists(&driver).await);
}

#[tokio::test]
async fn approvals_disabled_keeps_confirm_behaviour() {
    let (config, driver) = config(false).await;
    seed_table(&driver).await;
    handle_drop_table(&config, drop_params(true, None))
        .await
        .expect("confirm path unchanged when approvals are off");
    assert!(!table_exists(&driver).await);
}

#[tokio::test]
async fn http_queue_lists_and_accepts_only_for_the_owner() {
    let (config, _driver) = config(true).await;
    let mut plain = AppConfig::clone(&config);
    plain.http_tokens = vec![strut_stack_sql::config::HttpToken {
        name: "alice".into(),
        token: "alice-secret".into(),
        mode: WriteMode::ReadOnly,
        sources: vec![],
    }];
    plain.policy = {
        let mut policy = Policy::new(Tier::Ddl, true, false);
        policy.add_grant(strut_stack_sql::core::auth::Grant::new(
            "alice",
            strut_stack_sql::core::auth::GrantSource::Any,
            Tier::Ddl,
        ));
        policy.with_local_default()
    };
    let (app, _ct) = build_http_router(plain.clone());
    let config = Arc::new(plain);

    // Queue an approval directly (the MCP call path is covered above).
    let entry = config
        .approvals
        .request("alice", "default", "drop_table", "drop table doomed");

    let get = |path: &'static str| {
        let app = app.clone();
        async move {
            let response = tower::ServiceExt::oneshot(
                app,
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .header("authorization", "Bearer alice-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&bytes).to_string())
        }
    };
    let (status, body) = get("/approvals").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains(&entry.id), "{body}");

    // Accept requires auth; an unknown id is 404.
    let post = |path: String| {
        let app = app.clone();
        async move {
            let response = tower::ServiceExt::oneshot(
                app,
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("authorization", "Bearer alice-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
            response.status()
        }
    };
    let status = post(format!("/approvals/{}/accept", entry.id)).await;
    assert_eq!(status, StatusCode::OK);
    let status = post("/approvals/nope/accept".into()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let unauthenticated = tower::ServiceExt::oneshot(
        app,
        Request::builder()
            .method("GET")
            .uri("/approvals")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
    .status();
    assert_eq!(unauthenticated, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn approval_summary_never_contains_parameter_values() {
    let (config, driver) = config(true).await;
    seed_table(&driver).await;
    let err = handle_drop_table(&config, drop_params(true, None))
        .await
        .expect_err("request");
    let body: Value = serde_json::to_value(err.data.expect("data")).unwrap();
    assert!(!body.to_string().contains("INSERT"), "{body}");
    let pending = config.approvals.pending_for("local");
    assert_eq!(pending.len(), 1);
    assert!(pending[0].summary.contains("doomed"));
    assert!(!pending[0].summary.contains("INSERT"));
}
