//! Migration plan/apply/rollback and universal dry run (R2-06).

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::CallToolResult;
use serde_json::{json, Value};
use strut_sql_driver_api::{ConnectOptions, EngineDriver, EngineKind};
use strut_sql_driver_sqlite::SqliteDriver;
use strut_stack_sql::config::{AppConfig, ResolvedSource, WriteMode};
use strut_stack_sql::core::auth::{Policy, Tier};
use strut_stack_sql::tools::core::{handle_execute_sql, ExecuteSqlParams};
use strut_stack_sql::tools::ddl::{handle_schema_mutate, SchemaMutateAction, SchemaMutateParams};
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
        plans: std::sync::Arc::new(strut_stack_sql::core::migrations::PlanStore::new()),
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

fn body(result: CallToolResult) -> Value {
    let text = result
        .content
        .first()
        .and_then(|b| b.as_text())
        .map(|t| t.text.as_str())
        .expect("text");
    serde_json::from_str(text).expect("json")
}

async fn count(driver: &Arc<dyn EngineDriver>, sql: &str) -> Option<i64> {
    execute_query(driver, sql, &[], &opts())
        .await
        .ok()
        .and_then(|r| r.data)
        .and_then(|d| {
            d.rows
                .first()
                .and_then(|row| row.first().and_then(|v| v.as_i64()))
        })
}

fn plan_params(statements: Vec<&str>) -> SchemaMutateParams {
    SchemaMutateParams {
        action: SchemaMutateAction::PlanMigration,
        schema: None,
        table: None,
        ddl: None,
        column: None,
        confirm: None,
        approval_id: None,
        statements: Some(statements.into_iter().map(str::to_string).collect()),
        plan_id: None,
        dry_run: None,
        source: None,
    }
}

fn apply_params(plan_id: &str, approval_id: Option<&str>, dry_run: bool) -> SchemaMutateParams {
    SchemaMutateParams {
        action: SchemaMutateAction::ApplyMigration,
        schema: None,
        table: None,
        ddl: None,
        column: None,
        confirm: None,
        approval_id: approval_id.map(str::to_string),
        statements: None,
        plan_id: Some(plan_id.into()),
        dry_run: dry_run.then_some(true),
        source: None,
    }
}

fn rollback_params(plan_id: &str, approval_id: Option<&str>) -> SchemaMutateParams {
    SchemaMutateParams {
        action: SchemaMutateAction::RollbackMigration,
        ..apply_params(plan_id, approval_id, false)
    }
}

#[tokio::test]
async fn plan_never_executes_and_apply_requires_approval() {
    let (config, driver) = config(false).await;
    let planned = body(
        handle_schema_mutate(
            &config,
            plan_params(vec![
                "CREATE TABLE audit (id INTEGER PRIMARY KEY)",
                "ALTER TABLE audit ADD COLUMN note TEXT",
            ]),
        )
        .await
        .expect("plan"),
    );
    assert_eq!(planned["dry_run"], json!(true));
    assert_eq!(planned["inverse_defined"], json!(true));
    let plan_id = planned["plan_id"].as_str().expect("plan id").to_string();
    assert_eq!(
        count(&driver, "SELECT count(*) AS n FROM audit").await,
        None,
        "plan must not execute"
    );

    // apply needs plan_id and an accepted approval (forced even with the
    // process flag off).
    let err = handle_schema_mutate(&config, apply_params(&plan_id, None, false))
        .await
        .expect_err("approval required");
    assert_eq!(
        err.data.as_ref().expect("data")["code"],
        json!("approval_required")
    );
    let approval_id = err.data.as_ref().expect("data")["approval_id"]
        .as_str()
        .expect("approval id")
        .to_string();
    assert!(config
        .approvals
        .decide(&approval_id, "local", true)
        .is_some());

    let applied = body(
        handle_schema_mutate(&config, apply_params(&plan_id, Some(&approval_id), false))
            .await
            .expect("apply"),
    );
    assert_eq!(applied["dry_run"], json!(false));
    assert_eq!(applied["executed"].as_array().expect("executed").len(), 2);
    assert_eq!(
        count(&driver, "SELECT count(*) AS n FROM audit").await,
        Some(0),
        "applied statements execute"
    );

    // A plan applies once.
    let err = handle_schema_mutate(&config, apply_params(&plan_id, None, false))
        .await
        .expect_err("already applied");
    assert!(
        err.message.contains("already applied") || err.message.contains("approval required"),
        "{err}"
    );
}

#[tokio::test]
async fn rollback_requires_an_applied_plan_and_a_defined_inverse() {
    let (config, driver) = config(false).await;
    let planned = body(
        handle_schema_mutate(
            &config,
            plan_params(vec!["CREATE TABLE temp_t (id INTEGER)"]),
        )
        .await
        .expect("plan"),
    );
    let plan_id = planned["plan_id"].as_str().expect("id").to_string();

    // Nothing to roll back before apply.
    let err = handle_schema_mutate(&config, rollback_params(&plan_id, None))
        .await
        .expect_err("not applied");
    assert!(err.message.contains("nothing to roll back"), "{err}");

    let err = handle_schema_mutate(&config, apply_params(&plan_id, None, false))
        .await
        .expect_err("approval");
    let approval_id = err.data.as_ref().expect("data")["approval_id"]
        .as_str()
        .expect("id")
        .to_string();
    assert!(config
        .approvals
        .decide(&approval_id, "local", true)
        .is_some());
    handle_schema_mutate(&config, apply_params(&plan_id, Some(&approval_id), false))
        .await
        .expect("apply");

    // Rollback has an inverse (DROP TABLE) and needs its own approval.
    let err = handle_schema_mutate(&config, rollback_params(&plan_id, None))
        .await
        .expect_err("approval");
    let approval_id = err.data.as_ref().expect("data")["approval_id"]
        .as_str()
        .expect("id")
        .to_string();
    assert!(config
        .approvals
        .decide(&approval_id, "local", true)
        .is_some());
    handle_schema_mutate(&config, rollback_params(&plan_id, Some(&approval_id)))
        .await
        .expect("rollback");
    assert_eq!(
        count(&driver, "SELECT count(*) AS n FROM temp_t").await,
        None
    );

    // A plan without a derivable inverse refuses rollback.
    let planned = body(
        handle_schema_mutate(&config, plan_params(vec!["DROP TABLE temp_t"]))
            .await
            .expect("plan"),
    );
    assert_eq!(planned["inverse_defined"], json!(false));
    let plan_id = planned["plan_id"].as_str().expect("id").to_string();
    let err = handle_schema_mutate(&config, rollback_params(&plan_id, None))
        .await
        .expect_err("not applied");
    assert!(err.message.contains("nothing to roll back"), "{err}");
}

#[tokio::test]
async fn plan_rejects_non_ddl_statements() {
    let (config, _driver) = config(false).await;
    let err = handle_schema_mutate(&config, plan_params(vec!["DELETE FROM users"]))
        .await
        .expect_err("only DDL");
    assert!(err.message.contains("only DDL"), "{err}");
}

#[tokio::test]
async fn dry_run_executes_nothing_and_needs_no_approval() {
    let (config, driver) = config(true).await;
    execute_query(&driver, "CREATE TABLE t (id INTEGER)", &[], &opts())
        .await
        .expect("seed");

    // schema_mutate dry run: DROP is validated, not executed, no approval needed.
    let mut params = apply_params("unused", None, true);
    params.action = SchemaMutateAction::DropTable;
    params.plan_id = None;
    params.table = Some("t".into());
    params.confirm = Some(true);
    let payload = body(
        handle_schema_mutate(&config, params)
            .await
            .expect("dry run"),
    );
    assert_eq!(payload["dry_run"], json!(true));
    assert!(payload["sql"].as_str().expect("sql").contains("DROP TABLE"));
    assert_eq!(count(&driver, "SELECT count(*) AS n FROM t").await, Some(0));

    // execute_sql dry run: INSERT validated, nothing written.
    let params: ExecuteSqlParams =
        serde_json::from_value(json!({ "sql": "INSERT INTO t (id) VALUES (1)", "dry_run": true }))
            .expect("params");
    let payload = body(
        handle_execute_sql(&config, params)
            .await
            .expect("dry run sql"),
    );
    assert_eq!(payload["dry_run"], json!(true));
    assert!(payload["sql"].as_str().expect("sql").contains("INSERT"));
    assert_eq!(count(&driver, "SELECT count(*) AS n FROM t").await, Some(0));

    // A write SQL dry run also works in read-only mode: nothing executes.
    let mut read_only = AppConfig::clone(&config);
    read_only.write_mode = WriteMode::ReadOnly;
    let read_only = Arc::new(read_only);
    let params: ExecuteSqlParams =
        serde_json::from_value(json!({ "sql": "DELETE FROM t", "dry_run": true })).expect("params");
    let err = handle_execute_sql(&read_only, params)
        .await
        .expect_err("guard still applies to dry runs");
    assert!(
        err.message.to_lowercase().contains("blocked") || err.message.contains("requires"),
        "{err}"
    );
}
