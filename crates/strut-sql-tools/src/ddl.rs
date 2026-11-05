use std::sync::Arc;

use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use strut_sql_core::config::{AppConfig, WriteMode};
use strut_sql_driver_api::EngineKind;
use strut_sql_guard::{classify_sql, validate_and_prepare, SqlClass};

use crate::core::{
    exec_error_kind, exec_options, json_result, tool_error, tool_error_code, ToolErrorKind,
};
use crate::exec::{execute_query, ExecOptions};

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaMutateAction {
    CreateTable,
    DropTable,
    AddColumn,
    AlterColumn,
    DropColumn,
    TruncateTable,
    /// Migration planning: validate statements and store a plan (never executes).
    PlanMigration,
    /// Execute a stored plan; requires plan_id and an accepted approval.
    ApplyMigration,
    /// Execute a plan's inverse; requires an applied plan with a defined inverse.
    RollbackMigration,
}

impl SchemaMutateAction {
    /// Stable label used for approval summaries and tool names.
    pub fn label(self) -> &'static str {
        match self {
            Self::CreateTable => "create table",
            Self::DropTable => "drop table",
            Self::AddColumn => "add column",
            Self::AlterColumn => "alter column",
            Self::DropColumn => "drop column",
            Self::TruncateTable => "truncate table",
            Self::PlanMigration => "plan migration",
            Self::ApplyMigration => "apply migration",
            Self::RollbackMigration => "rollback migration",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ColumnDef {
    pub name: String,
    #[serde(rename = "type")]
    pub data_type: String,
    #[serde(default)]
    pub nullable: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SchemaMutateParams {
    pub action: SchemaMutateAction,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub ddl: Option<String>,
    #[serde(default)]
    pub column: Option<ColumnDef>,
    #[serde(default)]
    pub confirm: Option<bool>,
    #[serde(default)]
    pub approval_id: Option<String>,
    #[serde(default)]
    pub statements: Option<Vec<String>>,
    #[serde(default)]
    pub plan_id: Option<String>,
    /// Validate and report without executing anything (R2-06).
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Alias-tool params (action implied by tool name).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTableParams {
    pub ddl: String,
    #[serde(default)]
    pub approval_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DropTableParams {
    pub table: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub confirm: Option<bool>,
    #[serde(default)]
    pub approval_id: Option<String>,
    /// Validate and report without executing anything (R2-06).
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TruncateTableParams {
    pub table: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub confirm: Option<bool>,
    #[serde(default)]
    pub approval_id: Option<String>,
    /// Validate and report without executing anything (R2-06).
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddColumnParams {
    pub table: String,
    pub column: ColumnDef,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub approval_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AlterColumnParams {
    pub table: String,
    pub column: ColumnDef,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub approval_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DropColumnParams {
    pub table: String,
    pub column: ColumnDef,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub confirm: Option<bool>,
    #[serde(default)]
    pub approval_id: Option<String>,
    /// Validate and report without executing anything (R2-06).
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Serialize)]
struct SchemaMutateResult {
    ok: bool,
    action: SchemaMutateAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows_affected: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn handle_create_table(
    config: &Arc<AppConfig>,
    params: CreateTableParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    handle_schema_mutate(
        config,
        SchemaMutateParams {
            action: SchemaMutateAction::CreateTable,
            schema: None,
            table: None,
            ddl: Some(params.ddl),
            column: None,
            confirm: None,
            approval_id: params.approval_id,
            statements: None,
            plan_id: None,
            dry_run: params.dry_run,
            source: params.source,
        },
    )
    .await
}

pub async fn handle_drop_table(
    config: &Arc<AppConfig>,
    params: DropTableParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    handle_schema_mutate(
        config,
        SchemaMutateParams {
            action: SchemaMutateAction::DropTable,
            schema: params.schema,
            table: Some(params.table),
            ddl: None,
            column: None,
            confirm: params.confirm,
            approval_id: params.approval_id,
            statements: None,
            plan_id: None,
            dry_run: params.dry_run,
            source: params.source,
        },
    )
    .await
}

pub async fn handle_truncate_table(
    config: &Arc<AppConfig>,
    params: TruncateTableParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    handle_schema_mutate(
        config,
        SchemaMutateParams {
            action: SchemaMutateAction::TruncateTable,
            schema: params.schema,
            table: Some(params.table),
            ddl: None,
            column: None,
            confirm: params.confirm,
            approval_id: params.approval_id,
            statements: None,
            plan_id: None,
            dry_run: params.dry_run,
            source: params.source,
        },
    )
    .await
}

pub async fn handle_add_column(
    config: &Arc<AppConfig>,
    params: AddColumnParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    handle_schema_mutate(
        config,
        SchemaMutateParams {
            action: SchemaMutateAction::AddColumn,
            schema: params.schema,
            table: Some(params.table),
            ddl: None,
            column: Some(params.column),
            confirm: None,
            approval_id: params.approval_id,
            statements: None,
            plan_id: None,
            dry_run: params.dry_run,
            source: params.source,
        },
    )
    .await
}

pub async fn handle_alter_column(
    config: &Arc<AppConfig>,
    params: AlterColumnParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    handle_schema_mutate(
        config,
        SchemaMutateParams {
            action: SchemaMutateAction::AlterColumn,
            schema: params.schema,
            table: Some(params.table),
            ddl: None,
            column: Some(params.column),
            confirm: None,
            approval_id: params.approval_id,
            statements: None,
            plan_id: None,
            dry_run: params.dry_run,
            source: params.source,
        },
    )
    .await
}

pub async fn handle_drop_column(
    config: &Arc<AppConfig>,
    params: DropColumnParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    handle_schema_mutate(
        config,
        SchemaMutateParams {
            action: SchemaMutateAction::DropColumn,
            schema: params.schema,
            table: Some(params.table),
            ddl: None,
            column: Some(params.column),
            confirm: params.confirm,
            approval_id: params.approval_id,
            statements: None,
            plan_id: None,
            dry_run: params.dry_run,
            source: params.source,
        },
    )
    .await
}

/// Plan a migration: validate every statement as DDL, derive an inverse where
/// one is exact, store the plan. Nothing is executed.
async fn plan_migration(
    config: &Arc<AppConfig>,
    params: SchemaMutateParams,
) -> Result<CallToolResult, McpError> {
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Execution))?;
    let engine = driver.kind();
    let mut statements = params.statements.clone().unwrap_or_default();
    if let Some(ddl) = &params.ddl {
        statements.push(ddl.clone());
    }
    if statements.is_empty() {
        return Err(tool_error(
            "plan_migration requires `statements` (or `ddl`)",
        ));
    }
    for statement in &statements {
        let prepared =
            validate_and_prepare(statement, &[], engine, WriteMode::AllowDdl, config.max_rows)
                .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Guard))?;
        if !matches!(prepared.class.sql_class(), SqlClass::Ddl) {
            return Err(tool_error_code(
                format!(
                    "migration statement is classified {} ({:?}); only DDL is allowed",
                    prepared.class.label(),
                    prepared.class.sql_class()
                ),
                ToolErrorKind::Guard,
            ));
        }
    }
    let plan = config
        .plans
        .plan(&config.principal_id, &source.name, statements);
    json_result(&serde_json::json!({
        "ok": true,
        "dry_run": true,
        "plan_id": plan.id,
        "statements": plan.statements,
        "inverse_defined": plan.inverse.is_some(),
        "inverse": plan.inverse,
        "expires_unix": plan.expires_unix,
        "source": source.name,
    }))
}

/// Apply or roll back a stored migration plan.
async fn apply_migration(
    config: &Arc<AppConfig>,
    params: SchemaMutateParams,
    rollback: bool,
) -> Result<CallToolResult, McpError> {
    let plan_id = params
        .plan_id
        .as_deref()
        .ok_or_else(|| tool_error("plan_id is required for apply_migration/rollback_migration"))?;
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Execution))?;
    let engine = driver.kind();
    config.plans.gc();
    let plan = config
        .plans
        .get(plan_id, &config.principal_id, &source.name)
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Validation))?;

    let statements = if rollback {
        if !plan.applied {
            return Err(tool_error(
                "migration plan has not been applied; there is nothing to roll back",
            ));
        }
        plan.inverse
            .clone()
            .ok_or_else(|| tool_error("no inverse is defined for this plan; rollback is refused"))?
    } else {
        plan.statements.clone()
    };

    // Applying and rolling back a migration always needs an accepted approval.
    let tool = if rollback {
        "rollback_migration"
    } else {
        "apply_migration"
    };
    crate::approval::require_forced(
        config,
        tool,
        &source.name,
        &format!("{tool} plan {plan_id}"),
        params.approval_id.as_deref(),
        true,
    )?;

    let mut opts = exec_options(config);
    opts.write_mode = WriteMode::AllowDdl;
    let mut executed = Vec::new();
    for statement in &statements {
        validate_and_prepare(statement, &[], engine, WriteMode::AllowDdl, config.max_rows)
            .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Guard))?;
        execute_query(&driver, statement, &[], &opts)
            .await
            .map_err(|e| {
                let message = format!(
                    "migration stopped after {} of {} statements: {}",
                    executed.len(),
                    statements.len(),
                    e
                );
                tool_error_code(message, exec_error_kind(&e))
            })?;
        executed.push(statement.clone());
    }
    if rollback {
        config
            .plans
            .mark_rolled_back(plan_id)
            .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Validation))?;
    } else {
        config
            .plans
            .mark_applied(plan_id)
            .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Validation))?;
    }
    json_result(&serde_json::json!({
        "ok": true,
        "dry_run": false,
        "plan_id": plan_id,
        "direction": if rollback { "rollback" } else { "apply" },
        "executed": executed,
        "source": source.name,
    }))
}

pub async fn handle_schema_mutate(
    config: &Arc<AppConfig>,
    params: SchemaMutateParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    if !config.write_mode.allows_ddl() {
        return Err(tool_error_code(
            "schema_mutate requires --allow-ddl on the server",
            ToolErrorKind::Guard,
        ));
    }

    match params.action {
        SchemaMutateAction::PlanMigration => {
            return plan_migration(config, params).await;
        }
        SchemaMutateAction::ApplyMigration => {
            return apply_migration(config, params, false).await;
        }
        SchemaMutateAction::RollbackMigration => {
            return apply_migration(config, params, true).await;
        }
        _ => {}
    }

    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Execution))?;
    let engine = driver.kind();

    let sql = build_ddl_sql(&params, engine)?;

    // R2-06: universal dry run — validate and report, execute nothing.
    if params.dry_run.unwrap_or(false) {
        let payload = serde_json::json!({
            "ok": true,
            "dry_run": true,
            "sql": sql,
            "action": params.action.label(),
            "source": source.name,
        });
        return json_result(&payload);
    }

    // R2-05: destructive DDL needs an accepted approval when the process
    // requires one. `confirm: true` is checked by build_ddl_sql and remains a
    // separate legacy flag — it is not an approval.
    if crate::approval::destructive_label(params.action.label()) {
        let tool = params.action.label().to_ascii_lowercase().replace(' ', "_");
        let summary = format!(
            "{tool} on {}",
            params
                .table
                .clone()
                .or_else(|| params.ddl.clone())
                .unwrap_or_else(|| "<unknown>".into())
        );
        crate::approval::require(
            config,
            &tool,
            &source.name,
            &summary,
            params.approval_id.as_deref(),
        )?;
    }

    validate_and_prepare(&sql, &[], engine, WriteMode::AllowDdl, config.max_rows)
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Guard))?;

    let mut opts: ExecOptions = exec_options(config);
    opts.write_mode = WriteMode::AllowDdl;

    let result = execute_query(&driver, &sql, &[], &opts)
        .await
        .map_err(|e| tool_error_code(e.to_string(), exec_error_kind(&e)))?;

    let rows_affected = result.data.as_ref().and_then(|d| d.meta.rows_affected);

    // PRD-F2-04: a successful DDL through this server invalidates every
    // cached introspection entry for the source.
    crate::introspect::invalidate_source(&source.name);

    json_result(&SchemaMutateResult {
        ok: true,
        action: params.action,
        table: params.table,
        rows_affected,
        error: None,
    })
}

fn build_ddl_sql(
    params: &SchemaMutateParams,
    engine: EngineKind,
) -> Result<String, rmcp::ErrorData> {
    let sql = build_ddl_sql_unchecked(params, engine)?;
    // SQL-SEC-14: the tool's name must guarantee the statement's class.
    // Classify the FINAL assembled SQL with the guard itself and refuse
    // anything that is not DDL — and for create_table, anything that is not
    // literally a CREATE TABLE (previously `ddl: "GRANT …"` sailed through
    // schema_mutate because GRANT classified as plain Ddl).
    let class = classify_sql(&sql, engine).map_err(|e| tool_error(e.to_string()))?;
    if class.sql_class() != SqlClass::Ddl {
        return Err(tool_error(format!(
            "assembled SQL classified as {} ({:?}), expected DDL — refused",
            class.label(),
            class.sql_class()
        )));
    }
    if matches!(params.action, SchemaMutateAction::CreateTable) && class.label() != "CREATE TABLE" {
        return Err(tool_error("create_table requires a CREATE TABLE statement"));
    }
    Ok(sql)
}

fn build_ddl_sql_unchecked(
    params: &SchemaMutateParams,
    engine: EngineKind,
) -> Result<String, rmcp::ErrorData> {
    match params.action {
        SchemaMutateAction::CreateTable => {
            let ddl = params
                .ddl
                .as_deref()
                .ok_or_else(|| tool_error("create_table requires ddl"))?;
            Ok(ddl.to_string())
        }
        SchemaMutateAction::DropTable => {
            require_confirm(params)?;
            let table = require_table(params)?;
            let qualified = qualify_table(params.schema.as_deref(), table, engine);
            Ok(format!("DROP TABLE {qualified}"))
        }
        SchemaMutateAction::PlanMigration
        | SchemaMutateAction::ApplyMigration
        | SchemaMutateAction::RollbackMigration => Err(tool_error(
            "migration actions are handled before SQL assembly",
        )),
        SchemaMutateAction::TruncateTable => {
            require_confirm(params)?;
            let table = require_table(params)?;
            let qualified = qualify_table(params.schema.as_deref(), table, engine);
            Ok(format!("TRUNCATE TABLE {qualified}"))
        }
        SchemaMutateAction::AddColumn => {
            let table = require_table(params)?;
            let column = params
                .column
                .as_ref()
                .ok_or_else(|| tool_error("add_column requires column"))?;
            let qualified = qualify_table(params.schema.as_deref(), table, engine);
            let data_type = validate_column_type(&column.data_type, engine)?;
            let null_sql = match column.nullable {
                Some(true) if engine == EngineKind::SqlServer => " NULL",
                Some(true) | None => "",
                Some(false) => " NOT NULL",
            };
            let add = if engine == EngineKind::SqlServer {
                "ADD"
            } else {
                "ADD COLUMN"
            };
            Ok(format!(
                "ALTER TABLE {qualified} {add} {} {}{}",
                quote_ident(&column.name, engine),
                data_type,
                null_sql
            ))
        }
        SchemaMutateAction::AlterColumn => {
            let table = require_table(params)?;
            let column = params
                .column
                .as_ref()
                .ok_or_else(|| tool_error("alter_column requires column"))?;
            let qualified = qualify_table(params.schema.as_deref(), table, engine);
            let col = quote_ident(&column.name, engine);
            let data_type = validate_column_type(&column.data_type, engine)?;
            match engine {
                EngineKind::MySql => Ok(format!(
                    "ALTER TABLE {qualified} MODIFY COLUMN {col} {data_type}"
                )),
                EngineKind::Postgres => {
                    if column.nullable.is_some() {
                        return Err(tool_error(
                            "alter_column on Postgres supports type only; omit nullable and run SET/DROP NOT NULL via execute_sql",
                        ));
                    }
                    Ok(format!(
                        "ALTER TABLE {qualified} ALTER COLUMN {col} TYPE {data_type}"
                    ))
                }
                EngineKind::DuckDb => {
                    if column.nullable.is_some() {
                        return Err(tool_error(
                            "alter_column on DuckDB supports type only; omit nullable and run SET/DROP NOT NULL via execute_sql",
                        ));
                    }
                    Ok(format!(
                        "ALTER TABLE {qualified} ALTER COLUMN {col} TYPE {data_type}"
                    ))
                }
                EngineKind::SqlServer => {
                    let null_sql = match column.nullable {
                        Some(true) => " NULL",
                        Some(false) => " NOT NULL",
                        None => return Err(tool_error(
                            "alter_column on SQL Server requires explicit nullable:true or nullable:false to avoid changing nullability implicitly",
                        )),
                    };
                    Ok(format!(
                        "ALTER TABLE {qualified} ALTER COLUMN {col} {data_type}{null_sql}"
                    ))
                }
                EngineKind::Sqlite => Err(tool_error(
                    "alter_column unsupported on SQLite; recreate the table",
                )),
                EngineKind::ClickHouse => Ok(format!(
                    "ALTER TABLE {qualified} MODIFY COLUMN {col} {data_type}"
                )),
            }
        }
        SchemaMutateAction::DropColumn => {
            require_confirm(params)?;
            let table = require_table(params)?;
            let column = params
                .column
                .as_ref()
                .ok_or_else(|| tool_error("drop_column requires column"))?;
            let qualified = qualify_table(params.schema.as_deref(), table, engine);
            Ok(format!(
                "ALTER TABLE {qualified} DROP COLUMN {}",
                quote_ident(&column.name, engine)
            ))
        }
    }
}

/// Words that may appear in a column type declaration (upper-case).
/// Deliberately a superset across PostgreSQL / MySQL / SQLite — a word that
/// is harmless-but-unknown on the target engine fails at the database, not
/// silently through this tool. Constraint/keyword smuggling (`NOT NULL`,
/// `DEFAULT`, `REFERENCES`, `SELECT`, …) is impossible because none of those
/// words are in this set (nullability has its own structured field).
const TYPE_WORDS: &[&str] = &[
    "INT",
    "INTEGER",
    "BIGINT",
    "SMALLINT",
    "TINYINT",
    "MEDIUMINT",
    "SERIAL",
    "BIGSERIAL",
    "SMALLSERIAL",
    "INT2",
    "INT4",
    "INT8",
    "FLOAT",
    "FLOAT4",
    "FLOAT8",
    "DOUBLE",
    "PRECISION",
    "REAL",
    "DECIMAL",
    "NUMERIC",
    "MONEY",
    "SMALLMONEY",
    "CHAR",
    "VARCHAR",
    "CHARACTER",
    "VARYING",
    "TEXT",
    "TINYTEXT",
    "MEDIUMTEXT",
    "LONGTEXT",
    "NCHAR",
    "NVARCHAR",
    "STRING",
    "BLOB",
    "TINYBLOB",
    "MEDIUMBLOB",
    "LONGBLOB",
    "BYTEA",
    "BINARY",
    "VARBINARY",
    "BIT",
    "BOOLEAN",
    "BOOL",
    "DATE",
    "TIME",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "DATETIME",
    "DATETIME2",
    "DATETIMEOFFSET",
    "SMALLDATETIME",
    "YEAR",
    "JSON",
    "JSONB",
    "UUID",
    "UNIQUEIDENTIFIER",
    "INET",
    "CIDR",
    "MACADDR",
    "MACADDR8",
    "XML",
    "INTERVAL",
    "UNSIGNED",
    "WITH",
    "WITHOUT",
    "ZONE",
    "ARRAY",
    "MAX",
    "IMAGE",
    "NTEXT",
    "ROWVERSION",
    "SQL_VARIANT",
];

/// SQL-SEC-14: validate a user-supplied column `type` before it is spliced
/// into assembled DDL. Previously the raw string was interpolated verbatim,
/// so `type: "INT); DROP TABLE x; --"` was an injection fragment waiting for
/// a path that skipped the guard. Rules: only letters/digits/spaces and
/// `( ) , . _ [ ]` characters; every word token must be a known type word
/// or a pure number (precision/scale). Returns the trimmed type unchanged.
fn validate_column_type(raw: &str, engine: EngineKind) -> Result<String, rmcp::ErrorData> {
    if engine == EngineKind::ClickHouse {
        return validate_clickhouse_column_type(raw);
    }
    let ty = raw.trim();
    if ty.is_empty() {
        return Err(tool_error("column type must not be empty"));
    }
    if !ty.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, ' ' | '\t' | '(' | ')' | ',' | '.' | '_' | '[' | ']')
    }) {
        return Err(tool_error(format!(
            "column type contains forbidden characters: {raw:?}"
        )));
    }
    let mut saw_word = false;
    for token in ty.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if token.is_empty() {
            continue;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            continue; // precision / scale, e.g. VARCHAR(255), DECIMAL(10,2)
        }
        saw_word = true;
        let upper = token.to_ascii_uppercase();
        if !TYPE_WORDS.contains(&upper.as_str()) {
            return Err(tool_error(format!(
                "column type token not in allowlist: {token}"
            )));
        }
    }
    if !saw_word {
        return Err(tool_error(format!("column type has no type name: {raw:?}")));
    }
    Ok(ty.to_string())
}

/// ClickHouse types are parameterised and nested (`Nullable(String)`,
/// `Decimal(18, 2)`, `DateTime64(3, 'UTC')`, `Array(LowCardinality(String))`),
/// so the allowlist admits quotes and `=` for enum values, but still refuses
/// statement separators, comments and every unknown word.
fn validate_clickhouse_column_type(raw: &str) -> Result<String, rmcp::ErrorData> {
    const CH_TYPE_WORDS: &[&str] = &[
        "INT",
        "INTEGER",
        "BIGINT",
        "SMALLINT",
        "TINYINT",
        "INT8",
        "INT16",
        "INT32",
        "INT64",
        "INT128",
        "INT256",
        "UINT8",
        "UINT16",
        "UINT32",
        "UINT64",
        "UINT128",
        "UINT256",
        "FLOAT",
        "FLOAT32",
        "FLOAT64",
        "DOUBLE",
        "REAL",
        "DECIMAL",
        "DECIMAL32",
        "DECIMAL64",
        "DECIMAL128",
        "DECIMAL256",
        "NUMERIC",
        "STRING",
        "FIXEDSTRING",
        "TEXT",
        "VARCHAR",
        "CHAR",
        "DATE",
        "DATE32",
        "DATETIME",
        "DATETIME64",
        "TIMESTAMP",
        "ENUM",
        "ENUM8",
        "ENUM16",
        "UUID",
        "IPV4",
        "IPV6",
        "BOOL",
        "BOOLEAN",
        "ARRAY",
        "NULLABLE",
        "LOWCARDINALITY",
        "MAP",
        "TUPLE",
        "NESTED",
        "JSON",
        "OBJECT",
        "VARIANT",
        "DYNAMIC",
        "NOTHING",
        "AGGREGATEFUNCTION",
        "SIMPLEAGGREGATEFUNCTION",
        "INTERVAL",
    ];
    let ty = raw.trim();
    if ty.is_empty() {
        return Err(tool_error("column type must not be empty"));
    }
    if !ty.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                ' ' | '\t' | '(' | ')' | ',' | '.' | '_' | '[' | ']' | '\'' | '='
            )
    }) {
        return Err(tool_error(format!(
            "column type contains forbidden characters: {raw:?}"
        )));
    }
    let mut depth = 0i32;
    let mut in_quote = false;
    let mut unquoted = String::with_capacity(ty.len());
    for c in ty.chars() {
        match c {
            '\'' => {
                in_quote = !in_quote;
                unquoted.push(' ');
            }
            _ if in_quote => unquoted.push(' '),
            '(' => {
                depth += 1;
                unquoted.push(c);
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(tool_error("column type has unbalanced parentheses"));
                }
                unquoted.push(c);
            }
            _ => unquoted.push(c),
        }
    }
    if in_quote {
        return Err(tool_error("column type has an unterminated quoted literal"));
    }
    if depth != 0 {
        return Err(tool_error("column type has unbalanced parentheses"));
    }
    let tokens: Vec<&str> = unquoted
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|token| !token.is_empty())
        .collect();
    let mut saw_word = false;
    for (index, token) in tokens.iter().enumerate() {
        if token.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        // Named tuple elements (`Tuple(a Int64, b String)`) are identifiers,
        // not types: skip a token that directly precedes a known type word.
        let followed_by_type = index > 0
            && tokens
                .get(index + 1)
                .is_some_and(|next| CH_TYPE_WORDS.contains(&next.to_ascii_uppercase().as_str()));
        if followed_by_type {
            continue;
        }
        saw_word = true;
        let upper = token.to_ascii_uppercase();
        if !CH_TYPE_WORDS.contains(&upper.as_str()) {
            return Err(tool_error(format!(
                "column type token not in allowlist: {token}"
            )));
        }
    }
    if !saw_word {
        return Err(tool_error(format!("column type has no type name: {raw:?}")));
    }
    Ok(ty.to_string())
}

fn require_confirm(params: &SchemaMutateParams) -> Result<(), rmcp::ErrorData> {
    if params.confirm != Some(true) {
        return Err(tool_error("destructive action requires confirm: true"));
    }
    Ok(())
}

fn require_table(params: &SchemaMutateParams) -> Result<&str, rmcp::ErrorData> {
    params
        .table
        .as_deref()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| tool_error("table is required"))
}

fn qualify_table(schema: Option<&str>, table: &str, engine: EngineKind) -> String {
    if let Some(schema) = schema.filter(|s| !s.is_empty()) {
        format!(
            "{}.{}",
            quote_ident(schema, engine),
            quote_ident(table, engine)
        )
    } else {
        quote_ident(table, engine)
    }
}

fn quote_ident(name: &str, engine: EngineKind) -> String {
    match engine {
        EngineKind::MySql | EngineKind::ClickHouse => {
            let trimmed = name.trim().trim_matches('`');
            format!("`{}`", trimmed.replace('`', "``"))
        }
        EngineKind::SqlServer => format!("[{}]", name.replace(']', "]]")),
        EngineKind::Postgres | EngineKind::Sqlite | EngineKind::DuckDb => {
            let trimmed = name.trim().trim_matches('"');
            format!("\"{}\"", trimmed.replace('"', "\"\""))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alter_column_mysql_uses_modify() {
        let params = SchemaMutateParams {
            action: SchemaMutateAction::AlterColumn,
            schema: None,
            table: Some("t".into()),
            ddl: None,
            column: Some(ColumnDef {
                name: "c".into(),
                data_type: "VARCHAR(64)".into(),
                nullable: None,
            }),
            confirm: None,
            approval_id: None,
            statements: None,
            plan_id: None,
            dry_run: None,
            source: None,
        };
        let sql = build_ddl_sql(&params, EngineKind::MySql).unwrap();
        assert_eq!(sql, "ALTER TABLE `t` MODIFY COLUMN `c` VARCHAR(64)");
    }

    #[test]
    fn alter_column_postgres_uses_type() {
        let params = SchemaMutateParams {
            action: SchemaMutateAction::AlterColumn,
            schema: Some("public".into()),
            table: Some("t".into()),
            ddl: None,
            column: Some(ColumnDef {
                name: "c".into(),
                data_type: "text".into(),
                nullable: None,
            }),
            confirm: None,
            approval_id: None,
            statements: None,
            plan_id: None,
            dry_run: None,
            source: None,
        };
        let sql = build_ddl_sql(&params, EngineKind::Postgres).unwrap();
        assert_eq!(
            sql,
            "ALTER TABLE \"public\".\"t\" ALTER COLUMN \"c\" TYPE text"
        );
    }

    #[test]
    fn alter_column_sqlite_errors() {
        let params = SchemaMutateParams {
            action: SchemaMutateAction::AlterColumn,
            schema: None,
            table: Some("t".into()),
            ddl: None,
            column: Some(ColumnDef {
                name: "c".into(),
                data_type: "TEXT".into(),
                nullable: None,
            }),
            confirm: None,
            approval_id: None,
            statements: None,
            plan_id: None,
            dry_run: None,
            source: None,
        };
        assert!(build_ddl_sql(&params, EngineKind::Sqlite).is_err());
    }

    #[test]
    fn alter_column_duckdb_is_type_only() {
        let mut params = mutate_params(SchemaMutateAction::AlterColumn, None, Some("BIGINT"));
        let sql = build_ddl_sql(&params, EngineKind::DuckDb).unwrap();
        let class = strut_sql_guard::classify_sql(&sql, EngineKind::DuckDb).unwrap();
        assert_eq!(class.sql_class(), strut_sql_guard::SqlClass::Ddl);
        params.column.as_mut().unwrap().nullable = Some(false);
        assert!(build_ddl_sql(&params, EngineKind::DuckDb).is_err());
        params.column.as_mut().unwrap().nullable = Some(true);
        assert!(build_ddl_sql(&params, EngineKind::DuckDb).is_err());
    }

    fn mutate_params(
        action: SchemaMutateAction,
        ddl: Option<&str>,
        data_type: Option<&str>,
    ) -> SchemaMutateParams {
        SchemaMutateParams {
            action,
            schema: None,
            table: Some("t".into()),
            ddl: ddl.map(str::to_string),
            column: data_type.map(|ty| ColumnDef {
                name: "c".into(),
                data_type: ty.into(),
                nullable: None,
            }),
            confirm: Some(true),
            approval_id: None,
            statements: None,
            plan_id: None,
            dry_run: None,
            source: None,
        }
    }

    #[test]
    fn sqlserver_structured_columns_use_tsql_and_escape_brackets() {
        let mut params = mutate_params(SchemaMutateAction::AddColumn, None, Some("NVARCHAR(MAX)"));
        params.schema = Some("s]x".into());
        params.table = Some("t]x".into());
        let column = params.column.as_mut().unwrap();
        column.name = "c]x".into();
        column.nullable = Some(false);
        assert_eq!(
            build_ddl_sql(&params, EngineKind::SqlServer).unwrap(),
            "ALTER TABLE [s]]x].[t]]x] ADD [c]]x] NVARCHAR(MAX) NOT NULL"
        );
        params.action = SchemaMutateAction::AlterColumn;
        params.column.as_mut().unwrap().nullable = Some(true);
        assert_eq!(
            build_ddl_sql(&params, EngineKind::SqlServer).unwrap(),
            "ALTER TABLE [s]]x].[t]]x] ALTER COLUMN [c]]x] NVARCHAR(MAX) NULL"
        );
        params.column.as_mut().unwrap().nullable = Some(false);
        assert!(build_ddl_sql(&params, EngineKind::SqlServer)
            .unwrap()
            .ends_with("NVARCHAR(MAX) NOT NULL"));
        params.action = SchemaMutateAction::DropColumn;
        assert_eq!(
            build_ddl_sql(&params, EngineKind::SqlServer).unwrap(),
            "ALTER TABLE [s]]x].[t]]x] DROP COLUMN [c]]x]"
        );
    }

    #[test]
    fn sqlserver_alter_column_requires_explicit_nullability() {
        let mut params = mutate_params(SchemaMutateAction::AlterColumn, None, Some("BIGINT"));
        let error = build_ddl_sql(&params, EngineKind::SqlServer).unwrap_err();
        assert!(error.message.contains("requires explicit nullable"));
        params.column.as_mut().unwrap().nullable = Some(false);
        assert_eq!(
            build_ddl_sql(&params, EngineKind::SqlServer).unwrap(),
            "ALTER TABLE [t] ALTER COLUMN [c] BIGINT NOT NULL"
        );
        params.column.as_mut().unwrap().nullable = Some(true);
        assert_eq!(
            build_ddl_sql(&params, EngineKind::SqlServer).unwrap(),
            "ALTER TABLE [t] ALTER COLUMN [c] BIGINT NULL"
        );
    }

    #[test]
    fn sqlserver_alter_column_prepared_sql_stays_tsql() {
        // The guard parses TSQL `ALTER COLUMN <type>` through a canonical form
        // because sqlparser 0.62 has no grammar for it. The SQL the tool then
        // sends to SQL Server must remain the original TSQL: SQL Server rejects
        // the MySQL `MODIFY COLUMN` form.
        let mut params =
            mutate_params(SchemaMutateAction::AlterColumn, None, Some("NVARCHAR(MAX)"));
        params.column.as_mut().unwrap().nullable = Some(false);
        let sql = build_ddl_sql(&params, EngineKind::SqlServer).unwrap();
        let prepared =
            validate_and_prepare(&sql, &[], EngineKind::SqlServer, WriteMode::AllowDdl, 100)
                .unwrap();
        assert_eq!(prepared.class.sql_class(), SqlClass::Ddl);
        assert!(prepared.sql.contains("ALTER COLUMN"), "{}", prepared.sql);
        assert!(
            prepared.sql.contains("NVARCHAR(MAX) NOT NULL"),
            "{}",
            prepared.sql
        );
        assert!(!prepared.sql.contains("MODIFY"), "{}", prepared.sql);
    }

    #[test]
    fn sqlserver_ddl_fragments_cannot_escape_structured_identifiers() {
        let mut params = mutate_params(SchemaMutateAction::AddColumn, None, Some("INT"));
        params.column.as_mut().unwrap().name = "id]; DROP TABLE secrets; --".into();
        let sql = build_ddl_sql(&params, EngineKind::SqlServer).unwrap();
        assert_eq!(
            sql,
            "ALTER TABLE [t] ADD [id]]; DROP TABLE secrets; --] INT"
        );
        params.column.as_mut().unwrap().data_type = "INT; DROP TABLE secrets".into();
        assert!(build_ddl_sql(&params, EngineKind::SqlServer).is_err());
    }

    // --- SQL-SEC-14: data_type allowlist ---

    #[test]
    fn valid_column_types_pass() {
        for ty in [
            "INT",
            "BIGINT UNSIGNED",
            "VARCHAR(255)",
            "DECIMAL(10,2)",
            "DOUBLE PRECISION",
            "TIMESTAMP WITH TIME ZONE",
            "TIMESTAMP WITHOUT TIME ZONE",
            "TEXT",
            "text",
            "JSONB",
            "CHARACTER VARYING(64)",
            "INT[]",
        ] {
            assert_eq!(
                validate_column_type(ty, EngineKind::Postgres).unwrap(),
                ty,
                "type={ty}"
            );
        }
    }

    #[test]
    fn clickhouse_column_types_accepted() {
        for ty in [
            "UInt64",
            "Nullable(String)",
            "LowCardinality(String)",
            "DateTime64(3, 'UTC')",
            "Decimal(18, 2)",
            "Array(Nullable(Int64))",
            "Map(String, UInt8)",
            "Tuple(a Int64, b String)",
            "Enum8('ok' = 1, 'bad' = 2)",
            "FixedString(16)",
        ] {
            assert_eq!(
                validate_column_type(ty, EngineKind::ClickHouse).unwrap(),
                ty,
                "type={ty}"
            );
        }
    }

    #[test]
    fn clickhouse_injected_column_types_rejected() {
        for ty in [
            "Int64); DROP TABLE x; --",
            "Int64) ENGINE = Log",
            "Nullable(String) NOT NULL",
            "Array(Int64",
            "Enum8('a' = 1",
            "SOME_RANDOM_TYPE",
            "",
            "123",
        ] {
            assert!(
                validate_column_type(ty, EngineKind::ClickHouse).is_err(),
                "type should be rejected: {ty:?}"
            );
        }
    }

    #[test]
    fn injected_column_types_rejected() {
        for ty in [
            "INT); DROP TABLE x; --",
            "INT, (SELECT password FROM users)",
            "VARCHAR(10) DEFAULT 'x'",
            "INT NOT NULL",
            "INT REFERENCES other(id)",
            "INT); GRANT ALL ON t TO bob; --",
            "SOME_RANDOM_TYPE",
            "",
            "123",
        ] {
            assert!(
                validate_column_type(ty, EngineKind::Postgres).is_err(),
                "type should be rejected: {ty:?}"
            );
        }
    }

    #[test]
    fn add_column_with_injected_type_rejected_end_to_end() {
        let params = mutate_params(
            SchemaMutateAction::AddColumn,
            None,
            Some("INT); DROP TABLE x; --"),
        );
        assert!(build_ddl_sql(&params, EngineKind::Postgres).is_err());
        let params = mutate_params(
            SchemaMutateAction::AlterColumn,
            None,
            Some("INT, (SELECT 1)"),
        );
        assert!(build_ddl_sql(&params, EngineKind::MySql).is_err());
    }

    #[test]
    fn add_column_with_valid_type_builds() {
        let params = mutate_params(SchemaMutateAction::AddColumn, None, Some("DECIMAL(10,2)"));
        let sql = build_ddl_sql(&params, EngineKind::Postgres).unwrap();
        assert_eq!(sql, "ALTER TABLE \"t\" ADD COLUMN \"c\" DECIMAL(10,2)");
    }

    // --- SQL-SEC-14: create_table must really be CREATE TABLE ---

    #[test]
    fn create_table_accepts_create_table() {
        let params = mutate_params(
            SchemaMutateAction::CreateTable,
            Some("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(64))"),
            None,
        );
        let sql = build_ddl_sql(&params, EngineKind::Postgres).unwrap();
        assert!(sql.starts_with("CREATE TABLE"));
    }

    #[test]
    fn create_table_rejects_non_create_statements() {
        for ddl in [
            "GRANT ALL ON t TO bob",
            "REVOKE ALL ON t FROM bob",
            "DROP TABLE t",
            "INSERT INTO t VALUES (1)",
            "CREATE INDEX idx ON t (id)",
            "CREATE TABLE t (id INT); DROP TABLE t",
        ] {
            let params = mutate_params(SchemaMutateAction::CreateTable, Some(ddl), None);
            assert!(
                build_ddl_sql(&params, EngineKind::Postgres).is_err(),
                "ddl should be rejected: {ddl}"
            );
        }
    }
}
