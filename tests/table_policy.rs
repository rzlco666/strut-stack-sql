//! Table allow/deny, masking and row-filter enforcement (R2-04).

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::CallToolResult;
use serde_json::{json, Value};
use strut_sql_driver_api::{ConnectOptions, EngineDriver, EngineKind};
use strut_sql_driver_sqlite::SqliteDriver;
use strut_sql_guard::{ColumnMask, MaskMode, RowFilter, TablePolicy, TableRule};
use strut_stack_sql::config::{AppConfig, ResolvedSource, WriteMode};
use strut_stack_sql::core::auth::{Grant, GrantSource, Policy, Tier};
use strut_stack_sql::tools::core::{
    handle_execute_sql, handle_search_objects, ExecuteSqlParams, SearchObjectsParams,
};
use strut_stack_sql::tools::exec::{execute_query, ExecOptions};
use strut_stack_sql::tools::full::{
    handle_describe_table, handle_list_tables, DescribeTableParams, ListTablesParams,
};

fn policy_with(tables: TablePolicy) -> Policy {
    let mut grant = Grant::new("local", GrantSource::Any, Tier::Ddl);
    grant.tables = tables;
    let mut policy = Policy::new(Tier::Ddl, true, false);
    policy.add_grant(grant);
    policy.with_local_default()
}

async fn config_with(tables: TablePolicy) -> (Arc<AppConfig>, Arc<dyn EngineDriver>) {
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
        policy: policy_with(tables.clone()),
        require_approval: false,
        approvals: std::sync::Arc::new(strut_stack_sql::core::approvals::ApprovalQueue::new()),
        plans: std::sync::Arc::new(strut_sql_core::migrations::PlanStore::new()),
        trace_sql: false,
        sqlite_allowlist: Vec::new(),
        limiter: std::sync::Arc::new(strut_sql_core::limits::PrincipalLimiter::new(0)),
        tables,
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
        principal: String::new(),
        source: String::new(),
        trace_sql: false,
        limiter: None,
    }
}

async fn seed(driver: &Arc<dyn EngineDriver>) {
    for sql in [
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, tenant INTEGER, email TEXT)",
        "CREATE TABLE payroll (id INTEGER PRIMARY KEY, salary INTEGER)",
        "INSERT INTO orders VALUES (1, 7, 'alice@example.com')",
        "INSERT INTO orders VALUES (2, 7, 'bob@example.com')",
        "INSERT INTO orders VALUES (3, 9, 'carol@example.com')",
        "INSERT INTO payroll VALUES (1, 5000)",
    ] {
        execute_query(driver, sql, &[], &opts())
            .await
            .expect("seed");
    }
}

fn exec_params(sql: &str) -> ExecuteSqlParams {
    ExecuteSqlParams {
        sql: Some(sql.into()),
        params: None,
        queries: None,
        page_offset: None,
        page_size: None,
        cursor: None,
        source: None,
        format: None,
        summary: None,
        approval_id: None,
        dry_run: None,
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

fn rule(table: &str) -> TableRule {
    TableRule {
        schema: None,
        table: table.into(),
    }
}

#[tokio::test]
async fn denied_table_is_unreachable_and_undiscoverable() {
    let tables = TablePolicy {
        deny: vec![rule("payroll")],
        ..Default::default()
    };
    let (config, driver) = config_with(tables).await;
    seed(&driver).await;

    let err = handle_execute_sql(&config, exec_params("SELECT * FROM payroll"))
        .await
        .expect_err("denied table");
    assert!(err.message.contains("not accessible"), "{err}");
    assert_eq!(
        err.data.expect("data")["code"],
        json!("guard_denied"),
        "denial must be a guard error"
    );

    let listed = body(
        handle_list_tables(
            &config,
            ListTablesParams {
                schema: None,
                keyword: None,
                offset: None,
                limit: None,
                source: None,
            },
        )
        .await
        .expect("list"),
    );
    let listed = listed.to_string();
    assert!(listed.contains("orders"), "{listed}");
    assert!(!listed.contains("payroll"), "denied table leaked: {listed}");

    let err = handle_describe_table(
        &config,
        DescribeTableParams {
            table: "payroll".into(),
            schema: None,
            source: None,
        },
    )
    .await
    .expect_err("describe denied");
    assert!(err.message.contains("not accessible"), "{err}");

    let found = body(
        handle_search_objects(
            &config,
            SearchObjectsParams {
                object_type: strut_stack_sql::tools::introspect::ObjectType::Table,
                keyword: Some("pay".into()),
                schema: None,
                source: None,
                offset: None,
                limit: None,
                include_score: None,
            },
        )
        .await
        .expect("search"),
    );
    assert!(
        !found.to_string().contains("payroll"),
        "search leaked: {found}"
    );
}

#[tokio::test]
async fn allow_list_limits_reachable_tables() {
    let tables = TablePolicy {
        allow: vec![rule("orders")],
        ..Default::default()
    };
    let (config, driver) = config_with(tables).await;
    seed(&driver).await;
    assert!(
        handle_execute_sql(&config, exec_params("SELECT count(*) AS n FROM orders"))
            .await
            .is_ok()
    );
    let err = handle_execute_sql(&config, exec_params("SELECT * FROM payroll"))
        .await
        .expect_err("not allow-listed");
    assert!(err.message.contains("not accessible"), "{err}");
}

#[tokio::test]
async fn deny_wins_over_allow() {
    let tables = TablePolicy {
        allow: vec![rule("orders"), rule("payroll")],
        deny: vec![rule("payroll")],
        ..Default::default()
    };
    assert!(!tables.allows_table(None, "payroll"));
    assert!(tables.allows_table(None, "orders"));
}

#[tokio::test]
async fn row_filter_is_applied_and_unsupported_statements_are_rejected() {
    let tables = TablePolicy {
        row_filters: vec![RowFilter {
            schema: None,
            table: "orders".into(),
            predicate: "tenant = 7".into(),
        }],
        ..Default::default()
    };
    let (config, driver) = config_with(tables).await;
    seed(&driver).await;

    let payload = body(
        handle_execute_sql(&config, exec_params("SELECT id FROM orders ORDER BY id"))
            .await
            .expect("filtered select"),
    );
    assert_eq!(payload["data"]["rows"], json!([[1], [2]]), "{payload}");
    assert_eq!(payload["data"]["meta"]["n"], json!(2));

    // The filter is injected in the AST, so an alias-qualified query and a
    // WHERE clause both still get it (AND semantics).
    let payload = body(
        handle_execute_sql(
            &config,
            exec_params("SELECT o.id FROM orders o WHERE o.id > 0 ORDER BY o.id"),
        )
        .await
        .expect("aliased"),
    );
    assert_eq!(payload["data"]["rows"], json!([[1], [2]]), "{payload}");

    // A statement family the filter cannot be applied to is rejected instead
    // of running unfiltered.
    let err = handle_execute_sql(&config, exec_params("DELETE FROM orders"))
        .await
        .expect_err("filtered DML rejected");
    assert!(err.message.contains("row filter"), "{err}");
}

#[tokio::test]
async fn masks_partial_hash_and_null_apply_to_results() {
    for (mode, check) in [
        (MaskMode::Partial, "a***@example.com"),
        (MaskMode::Null, "null"),
    ] {
        let tables = TablePolicy {
            masks: vec![ColumnMask {
                table: None,
                column: "email".into(),
                mode,
            }],
            ..Default::default()
        };
        let (config, driver) = config_with(tables).await;
        seed(&driver).await;
        let payload = body(
            handle_execute_sql(
                &config,
                exec_params("SELECT email FROM orders WHERE id = 1"),
            )
            .await
            .expect("masked"),
        );
        let rendered = payload.to_string();
        assert!(!rendered.contains("alice@example.com"), "{rendered}");
        assert!(rendered.contains(check), "{mode:?}: {rendered}");
    }

    let tables = TablePolicy {
        masks: vec![ColumnMask {
            table: None,
            column: "email".into(),
            mode: MaskMode::Hash,
        }],
        ..Default::default()
    };
    let (config, driver) = config_with(tables).await;
    seed(&driver).await;
    let payload = body(
        handle_execute_sql(
            &config,
            exec_params("SELECT email FROM orders WHERE id = 1"),
        )
        .await
        .expect("hashed"),
    );
    let value = payload["data"]["rows"][0][0]
        .as_str()
        .expect("hash string")
        .to_string();
    assert_eq!(value.len(), 64, "sha256 hex expected: {value}");
    assert_eq!(
        payload["data"]["meta"]["redacted_columns"],
        json!(["email"]),
        "{payload}"
    );
}

#[tokio::test]
async fn advisor_refuses_a_denied_table() {
    let tables = TablePolicy {
        deny: vec![rule("payroll")],
        ..Default::default()
    };
    let (config, driver) = config_with(tables).await;
    seed(&driver).await;
    let params: strut_stack_sql::tools::core::AnalyzeQueryParams =
        serde_json::from_value(json!({ "sql": "SELECT * FROM payroll" })).expect("params");
    let err = strut_stack_sql::tools::core::handle_analyze_query(&config, params)
        .await
        .expect_err("advisor denial");
    assert!(err.message.contains("not accessible"), "{err}");
}

#[tokio::test]
async fn database_allowlist_filters_list_schemas() {
    use strut_stack_sql::tools::full::{handle_list_schemas, SourceParams};

    let (allowed, _) = config_with(TablePolicy {
        databases: vec!["main".into()],
        ..Default::default()
    })
    .await;
    let listed = body(
        handle_list_schemas(&allowed, SourceParams { source: None })
            .await
            .expect("list schemas"),
    );
    let names: Vec<&str> = listed
        .as_array()
        .expect("schema array")
        .iter()
        .filter_map(|object| object.get("name").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&"main"), "{names:?}");

    let (denied, _) = config_with(TablePolicy {
        databases: vec!["elsewhere".into()],
        ..Default::default()
    })
    .await;
    let listed = body(
        handle_list_schemas(&denied, SourceParams { source: None })
            .await
            .expect("list schemas"),
    );
    assert_eq!(listed.as_array().map(Vec::len), Some(0), "{listed}");
}

#[tokio::test]
async fn catalog_tools_reject_table_arguments_qualified_by_a_denied_database() {
    use strut_stack_sql::tools::full::{
        handle_describe_table, handle_list_foreign_keys, handle_list_indexes, DescribeTableParams,
        ListForeignKeysParams, ListIndexesParams,
    };
    use strut_stack_sql::tools::intel::{handle_suggest_joins, SuggestJoinsParams};

    let (config, driver) = config_with(TablePolicy {
        databases: vec!["main".into()],
        ..Default::default()
    })
    .await;
    seed(&driver).await;

    let err = handle_describe_table(
        &config,
        DescribeTableParams {
            table: "other.users".into(),
            schema: None,
            source: None,
        },
    )
    .await
    .expect_err("describe_table must not split a denied qualifier");
    assert!(
        err.message.contains("database 'other' is not accessible"),
        "{err}"
    );

    let err = handle_list_indexes(
        &config,
        ListIndexesParams {
            schema: None,
            table: Some("other.users".into()),
            keyword: None,
            source: None,
        },
    )
    .await
    .expect_err("list_indexes must not accept a denied qualifier");
    assert!(
        err.message.contains("database 'other' is not accessible"),
        "{err}"
    );

    let err = handle_list_foreign_keys(
        &config,
        ListForeignKeysParams {
            schema: None,
            table: Some("other.users".into()),
            source: None,
        },
    )
    .await
    .expect_err("list_foreign_keys must not accept a denied qualifier");
    assert!(
        err.message.contains("database 'other' is not accessible"),
        "{err}"
    );

    let err = handle_suggest_joins(
        &config,
        SuggestJoinsParams {
            tables: vec!["main.orders".into(), "other.users".into()],
            schema: None,
            source: None,
        },
    )
    .await
    .expect_err("suggest_joins must not accept a denied qualifier");
    assert!(
        err.message.contains("database 'other' is not accessible"),
        "{err}"
    );

    // An allowed qualifier keeps working; the guard must not reject every dot.
    let described = handle_describe_table(
        &config,
        DescribeTableParams {
            table: "main.orders".into(),
            schema: None,
            source: None,
        },
    )
    .await
    .expect("allowed database qualifier");
    let payload = body(described);
    assert_eq!(payload["name"], "orders", "{payload}");
    assert_eq!(payload["schema"], "main", "{payload}");
}
