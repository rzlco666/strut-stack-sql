mod classify;
mod params;

use serde_json::Value;
use sqlparser::ast::{
    Expr, Fetch, Function, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    JoinConstraint, JoinOperator, LimitClause, ObjectName, Offset, OffsetRows, Query, Select,
    SelectItem, SetExpr, Statement, TableFactor, Value as SqlValue, Values,
};
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;

use crate::config::WriteMode;
use crate::db::EngineKind;

pub use classify::{SqlClass, StmtClass};
pub use params::{
    count_pg_numbered_placeholders, count_question_mark_placeholders, placeholder_count,
    rewrite_placeholders_for_postgres, validate_param_count,
};

#[derive(Debug, Clone, Default)]
pub struct PrepareOptions {
    pub page_offset: usize,
    pub page_size: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct PreparedSql {
    pub sql: String,
    pub params: Vec<Value>,
    pub limit_injected: bool,
    pub limit_clamped: bool,
    /// SQL-SEC-12: true when the statement carries a row limit whose size
    /// could NOT be proven `<= max_rows` from a literal (parameterised
    /// `LIMIT ?` / `LIMIT $1`, expression limits, `FETCH FIRST … PERCENT`,
    /// non-literal FETCH). The SQL is left untouched, so the execution layer
    /// MUST cap the number of rows it fetches at `max_rows + 1` itself.
    pub limit_unverified: bool,
    pub class: StmtClass,
    pub server_pagination: bool,
    pub page_size: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("SQL guard: {0}")]
    Denied(String),
}

/// SQL-SEC-02 (continued): MySQL `INTO OUTFILE` / `INTO DUMPFILE` writes
/// server-side files. sqlparser 0.62 cannot represent the syntax at all, so
/// without this pre-parse check the denial would surface as a confusing
/// parse error; reject it explicitly, in every mode, before parsing.
fn reject_file_export_syntax(sql: &str) -> Result<(), GuardError> {
    let lower = sql.to_ascii_lowercase();
    for needle in ["into outfile", "into dumpfile"] {
        if lower.contains(needle) {
            return Err(GuardError::Denied(format!(
                "{} is not allowed: exporting query results to server files is forbidden",
                needle.to_ascii_uppercase()
            )));
        }
    }
    Ok(())
}

/// Parse a single statement with the engine's dialect and classify it.
/// Used by the structured DDL tools (SQL-SEC-14) to verify that SQL they
/// assembled really is the statement class they claim to run.
pub fn classify_sql(sql: &str, engine: EngineKind) -> Result<StmtClass, GuardError> {
    reject_file_export_syntax(sql)?;
    let dialect: Box<dyn Dialect> = dialect_for(engine);
    let statements = Parser::parse_sql(dialect.as_ref(), sql)
        .map_err(|e| GuardError::Denied(format!("parse error: {e}")))?;
    if statements.len() != 1 {
        return Err(GuardError::Denied(format!(
            "expected exactly one statement, got {}",
            statements.len()
        )));
    }
    classify::classify(&statements[0])
}

fn dialect_for(engine: EngineKind) -> Box<dyn Dialect> {
    match engine {
        EngineKind::Postgres => Box::new(PostgreSqlDialect {}),
        EngineKind::Mysql => Box::new(MySqlDialect {}),
        EngineKind::Sqlite => Box::new(SQLiteDialect {}),
    }
}

pub fn validate_and_prepare(
    sql: &str,
    query_params: &[Value],
    engine: EngineKind,
    write_mode: WriteMode,
    default_limit: u32,
) -> Result<PreparedSql, GuardError> {
    validate_and_prepare_with_options(
        sql,
        query_params,
        engine,
        write_mode,
        default_limit,
        PrepareOptions::default(),
    )
}

pub fn validate_and_prepare_with_options(
    sql: &str,
    query_params: &[Value],
    engine: EngineKind,
    write_mode: WriteMode,
    default_limit: u32,
    prepare_opts: PrepareOptions,
) -> Result<PreparedSql, GuardError> {
    validate_param_count(sql, query_params, engine)?;
    reject_file_export_syntax(sql)?;

    let normalized = normalize_sql_for_engine(sql, engine);
    let dialect = dialect_for(engine);

    let statements = Parser::parse_sql(dialect.as_ref(), &normalized)
        .map_err(|e| GuardError::Denied(format!("parse error: {e}")))?;

    if statements.is_empty() {
        return Err(GuardError::Denied("empty query".into()));
    }
    if statements.len() > 1 {
        return Err(GuardError::Denied(
            "multiple statements are not allowed in a single query string".into(),
        ));
    }

    let stmt = statements.into_iter().next().expect("checked len");
    let class = classify::classify(&stmt)?;
    check_query_complexity(&stmt)?;
    // SQL-SEC-03/04: dangerous functions are denied in EVERY mode — this is
    // not a write-mode question, these functions must never run at all.
    check_denied_functions(&stmt, engine)?;

    if class.requires_writes_for_explain() && !write_mode.allows_dml() {
        return Err(GuardError::Denied(
            "EXPLAIN ANALYZE executes the query and requires --allow-writes".into(),
        ));
    }

    if let Statement::Explain {
        statement: inner, ..
    } = &stmt
    {
        let inner_class = classify::classify(inner)?;
        enforce_write_mode(&inner_class, write_mode)?;
    }

    enforce_write_mode(&class, write_mode)?;

    let wants_pagination = prepare_opts.page_offset > 0 || prepare_opts.page_size.is_some();
    let page_size = prepare_opts.page_size.unwrap_or(default_limit as usize) as u32;

    let needs_limit = class.is_select_like()
        && !wants_pagination
        && matches!(&stmt, Statement::Query(q) if q.limit_clause.is_none() && q.fetch.is_none());

    let trimmed = strip_trailing_semicolons(&normalized);
    let mut stmt = stmt;
    let (limit_clamped, limit_unverified) = if wants_pagination {
        (false, false)
    } else {
        clamp_query_limit(&mut stmt, default_limit)
    };

    let (final_sql, server_pagination) = if wants_pagination {
        if let Statement::Query(q) = &mut stmt {
            if !class.is_select_like() {
                return Err(GuardError::Denied(
                    "page_offset/page_size only supported on SELECT queries".into(),
                ));
            }
            let offset = prepare_opts.page_offset as u64;
            let fetch_limit = offset + page_size as u64 + 1;
            set_query_offset_limit(q, offset, fetch_limit);
            (stmt.to_string(), true)
        } else {
            return Err(GuardError::Denied(
                "page_offset/page_size only supported on SELECT queries".into(),
            ));
        }
    } else if needs_limit {
        (format!("{trimmed} LIMIT {default_limit}"), false)
    } else if limit_clamped {
        (stmt.to_string(), false)
    } else {
        (trimmed.to_string(), false)
    };

    validate_param_count(&final_sql, query_params, engine)?;

    Ok(PreparedSql {
        sql: final_sql,
        params: query_params.to_vec(),
        limit_injected: needs_limit,
        limit_clamped,
        limit_unverified,
        class,
        server_pagination,
        page_size,
    })
}

fn normalize_sql_for_engine(sql: &str, engine: EngineKind) -> String {
    match engine {
        EngineKind::Postgres if count_question_mark_placeholders(sql) > 0 => {
            rewrite_placeholders_for_postgres(sql)
        }
        _ => sql.to_string(),
    }
}

fn enforce_write_mode(class: &StmtClass, mode: WriteMode) -> Result<(), GuardError> {
    match class.sql_class() {
        SqlClass::Read => Ok(()),
        SqlClass::Dml if mode.allows_dml() => Ok(()),
        SqlClass::Ddl if mode.allows_ddl() => Ok(()),
        // SQL-SEC-13: GRANT/REVOKE (SqlClass::Admin) are NOT part of DDL.
        // Guard-level rule: Admin passes only in AllowDdl mode. A second,
        // stricter gate — an explicit --allow-grants opt-in — is enforced at
        // the config/tools/server layer (WriteMode lives in config and is
        // not extended from here); until that gate lands, AllowDdl is the
        // minimum bar and ReadOnly/AllowWrites always reject Admin.
        SqlClass::Admin if mode.allows_ddl() => Ok(()),
        SqlClass::Admin => Err(GuardError::Denied(
            "GRANT/REVOKE require AllowDdl mode and --allow-grants".into(),
        )),
        SqlClass::Txn => Err(GuardError::Denied(
            "transaction control statements are not allowed".into(),
        )),
        SqlClass::Dml => Err(GuardError::Denied(
            "DML blocked in read-only mode; restart with --allow-writes".into(),
        )),
        SqlClass::Ddl => Err(GuardError::Denied(
            "DDL blocked; restart with --allow-ddl".into(),
        )),
        SqlClass::Other => Err(GuardError::Denied(format!(
            "statement type not allowed: {} (blocked for safety)",
            class.label()
        ))),
    }
}

// ---------------------------------------------------------------------------
// SQL-SEC-03 / SQL-SEC-04: denied functions, per engine.
//
// Statement type alone does not bound what SQL can do — file access, remote
// execution and state mutation live in *functions* (`pg_read_file`,
// `dblink_exec`, `nextval`, `pg_sleep`, …). Every function call in the
// statement is walked (projection, WHERE, HAVING, GROUP BY, JOIN … ON,
// subqueries, CTEs, function arguments, DML sources/targets) and matched
// against the engine's denylist. Denied means denied in every write mode.
// ---------------------------------------------------------------------------

struct DeniedFunction {
    name: &'static str,
    reason: &'static str,
}

const DENIED_FUNCTIONS_PG: &[DeniedFunction] = &[
    // Server file access / remote execution.
    DeniedFunction {
        name: "pg_read_file",
        reason: "reads arbitrary files from the database server",
    },
    DeniedFunction {
        name: "pg_read_binary_file",
        reason: "reads arbitrary files from the database server",
    },
    DeniedFunction {
        name: "pg_ls_dir",
        reason: "lists directories on the database server",
    },
    DeniedFunction {
        name: "pg_stat_file",
        reason: "stats files on the database server",
    },
    DeniedFunction {
        name: "pg_logdir_ls",
        reason: "lists log files on the database server",
    },
    DeniedFunction {
        name: "lo_export",
        reason: "writes a large object to a file on the database server",
    },
    DeniedFunction {
        name: "lo_import",
        reason: "reads a file from the database server into a large object",
    },
    DeniedFunction {
        name: "dblink",
        reason: "executes SQL on a remote database server",
    },
    DeniedFunction {
        name: "dblink_exec",
        reason: "executes SQL on a remote database server",
    },
    // State mutation that is legal even inside a read-only transaction.
    DeniedFunction {
        name: "nextval",
        reason: "mutates sequence state",
    },
    DeniedFunction {
        name: "setval",
        reason: "mutates sequence state",
    },
    DeniedFunction {
        name: "set_config",
        reason: "mutates session configuration",
    },
    DeniedFunction {
        name: "pg_advisory_lock",
        reason: "acquires a session-level advisory lock",
    },
    DeniedFunction {
        name: "pg_advisory_xact_lock",
        reason: "acquires a transaction-level advisory lock",
    },
    DeniedFunction {
        name: "pg_try_advisory_lock",
        reason: "acquires a session-level advisory lock",
    },
    DeniedFunction {
        name: "pg_try_advisory_xact_lock",
        reason: "acquires a transaction-level advisory lock",
    },
    // Cheap denial of service.
    DeniedFunction {
        name: "pg_sleep",
        reason: "sleeps the database session (denial of service)",
    },
];

const DENIED_FUNCTIONS_MYSQL: &[DeniedFunction] = &[
    DeniedFunction {
        name: "load_file",
        reason: "reads arbitrary files from the database server",
    },
    // Cheap denial of service / connection pinning.
    DeniedFunction {
        name: "sleep",
        reason: "sleeps the database session (denial of service)",
    },
    DeniedFunction {
        name: "benchmark",
        reason: "burns database CPU in a tight loop (denial of service)",
    },
    DeniedFunction {
        name: "get_lock",
        reason: "acquires a named lock and pins the session",
    },
    DeniedFunction {
        name: "release_lock",
        reason: "manipulates named locks held by other sessions",
    },
];

const DENIED_FUNCTIONS_SQLITE: &[DeniedFunction] = &[
    DeniedFunction {
        name: "readfile",
        reason: "reads arbitrary files from the host filesystem",
    },
    DeniedFunction {
        name: "writefile",
        reason: "writes arbitrary files to the host filesystem",
    },
];

fn denied_functions_for(engine: EngineKind) -> &'static [DeniedFunction] {
    match engine {
        EngineKind::Postgres => DENIED_FUNCTIONS_PG,
        EngineKind::Mysql => DENIED_FUNCTIONS_MYSQL,
        EngineKind::Sqlite => DENIED_FUNCTIONS_SQLITE,
    }
}

fn check_denied_functions(stmt: &Statement, engine: EngineKind) -> Result<(), GuardError> {
    let denied = denied_functions_for(engine);
    scan_statement_functions(stmt, &mut |name| {
        if let Some(hit) = denied.iter().find(|d| d.name == name) {
            return Err(GuardError::Denied(format!(
                "function '{name}' is not allowed: {}",
                hit.reason
            )));
        }
        Ok(())
    })
}

/// Last component of a possibly schema-qualified function name, lower-cased
/// and unquoted: `pg_catalog.PG_SLEEP` → `pg_sleep`.
fn function_base_name(func: &Function) -> String {
    object_name_base_name(&func.name)
}

fn object_name_base_name(name: &ObjectName) -> String {
    let full = name.to_string();
    let last = full.rsplit('.').next().unwrap_or(&full);
    last.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
        .to_ascii_lowercase()
}

/// The scanner reports each function *name* it encounters: expression
/// function calls (`Expr::Function`) and table functions in FROM
/// (`FROM dblink(...)`, `FROM generate_series(...)`).
type FnScan<'a> = dyn FnMut(&str) -> Result<(), GuardError> + 'a;

fn scan_statement_functions(stmt: &Statement, on_fn: &mut FnScan<'_>) -> Result<(), GuardError> {
    match stmt {
        Statement::Query(query) => scan_query_functions(query, on_fn),
        Statement::Explain { statement, .. } => scan_statement_functions(statement, on_fn),
        Statement::Insert(insert) => {
            if let Some(source) = &insert.source {
                scan_query_functions(source, on_fn)?;
            }
            if let Some(returning) = &insert.returning {
                scan_select_items_functions(returning, on_fn)?;
            }
            Ok(())
        }
        Statement::Update(update) => {
            for assignment in &update.assignments {
                scan_expr_functions(&assignment.value, on_fn)?;
            }
            if let Some(selection) = &update.selection {
                scan_expr_functions(selection, on_fn)?;
            }
            if let Some(returning) = &update.returning {
                scan_select_items_functions(returning, on_fn)?;
            }
            Ok(())
        }
        Statement::Delete(delete) => {
            if let Some(selection) = &delete.selection {
                scan_expr_functions(selection, on_fn)?;
            }
            if let Some(returning) = &delete.returning {
                scan_select_items_functions(returning, on_fn)?;
            }
            Ok(())
        }
        Statement::Merge(merge) => scan_expr_functions(&merge.on, on_fn),
        _ => Ok(()),
    }
}

fn scan_query_functions(query: &Query, on_fn: &mut FnScan<'_>) -> Result<(), GuardError> {
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            scan_query_functions(&cte.query, on_fn)?;
        }
    }
    scan_set_expr_functions(query.body.as_ref(), on_fn)?;
    if let Some(fetch) = &query.fetch {
        if let Some(quantity) = &fetch.quantity {
            scan_expr_functions(quantity, on_fn)?;
        }
    }
    Ok(())
}

fn scan_set_expr_functions(body: &SetExpr, on_fn: &mut FnScan<'_>) -> Result<(), GuardError> {
    match body {
        SetExpr::Select(select) => scan_select_functions(select, on_fn),
        SetExpr::Query(query) => scan_query_functions(query, on_fn),
        SetExpr::SetOperation { left, right, .. } => {
            scan_set_expr_functions(left, on_fn)?;
            scan_set_expr_functions(right, on_fn)
        }
        SetExpr::Values(values) => scan_values_functions(values, on_fn),
        SetExpr::Insert(stmt)
        | SetExpr::Update(stmt)
        | SetExpr::Delete(stmt)
        | SetExpr::Merge(stmt) => scan_statement_functions(stmt, on_fn),
        SetExpr::Table(_) => Ok(()),
    }
}

fn scan_values_functions(values: &Values, on_fn: &mut FnScan<'_>) -> Result<(), GuardError> {
    for row in &values.rows {
        for expr in row.iter() {
            scan_expr_functions(expr, on_fn)?;
        }
    }
    Ok(())
}

fn scan_select_functions(select: &Select, on_fn: &mut FnScan<'_>) -> Result<(), GuardError> {
    scan_select_items_functions(&select.projection, on_fn)?;
    for twj in &select.from {
        scan_table_factor_functions(&twj.relation, on_fn)?;
        for join in &twj.joins {
            scan_table_factor_functions(&join.relation, on_fn)?;
            if let Some(on) = join_on_expr(&join.join_operator) {
                scan_expr_functions(on, on_fn)?;
            }
        }
    }
    if let Some(selection) = &select.selection {
        scan_expr_functions(selection, on_fn)?;
    }
    if let GroupByExpr::Expressions(exprs, _) = &select.group_by {
        for expr in exprs {
            scan_expr_functions(expr, on_fn)?;
        }
    }
    if let Some(having) = &select.having {
        scan_expr_functions(having, on_fn)?;
    }
    if let Some(qualify) = &select.qualify {
        scan_expr_functions(qualify, on_fn)?;
    }
    Ok(())
}

fn scan_select_items_functions(
    items: &[SelectItem],
    on_fn: &mut FnScan<'_>,
) -> Result<(), GuardError> {
    for item in items {
        match item {
            SelectItem::UnnamedExpr(expr) => scan_expr_functions(expr, on_fn)?,
            SelectItem::ExprWithAlias { expr, .. } => scan_expr_functions(expr, on_fn)?,
            _ => {}
        }
    }
    Ok(())
}

fn scan_table_factor_functions(
    factor: &TableFactor,
    on_fn: &mut FnScan<'_>,
) -> Result<(), GuardError> {
    match factor {
        TableFactor::Derived { subquery, .. } => scan_query_functions(subquery, on_fn),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            scan_table_factor_functions(&table_with_joins.relation, on_fn)?;
            for join in &table_with_joins.joins {
                scan_table_factor_functions(&join.relation, on_fn)?;
                if let Some(on) = join_on_expr(&join.join_operator) {
                    scan_expr_functions(on, on_fn)?;
                }
            }
            Ok(())
        }
        TableFactor::Table { name, args, .. } => {
            if let Some(args) = args {
                // A table factor with arguments is a function call.
                on_fn(&object_name_base_name(name))?;
                for arg in &args.args {
                    scan_function_arg_functions(arg, on_fn)?;
                }
            }
            Ok(())
        }
        TableFactor::Function { name, args, .. } => {
            on_fn(&object_name_base_name(name))?;
            for arg in args {
                scan_function_arg_functions(arg, on_fn)?;
            }
            Ok(())
        }
        TableFactor::UNNEST { array_exprs, .. } => {
            for expr in array_exprs {
                scan_expr_functions(expr, on_fn)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn join_on_expr(op: &JoinOperator) -> Option<&Expr> {
    let constraint = match op {
        JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c) => c,
        _ => return None,
    };
    match constraint {
        JoinConstraint::On(expr) => Some(expr),
        _ => None,
    }
}

fn scan_function_arg_functions(
    arg: &FunctionArg,
    on_fn: &mut FnScan<'_>,
) -> Result<(), GuardError> {
    let fae = match arg {
        FunctionArg::Unnamed(fae) => fae,
        FunctionArg::Named { arg, .. } | FunctionArg::ExprNamed { arg, .. } => arg,
    };
    if let FunctionArgExpr::Expr(expr) = fae {
        scan_expr_functions(expr, on_fn)?;
    }
    Ok(())
}

fn scan_function_arguments_functions(
    args: &FunctionArguments,
    on_fn: &mut FnScan<'_>,
) -> Result<(), GuardError> {
    match args {
        FunctionArguments::None => Ok(()),
        FunctionArguments::Subquery(query) => scan_query_functions(query, on_fn),
        FunctionArguments::List(list) => {
            for arg in &list.args {
                scan_function_arg_functions(arg, on_fn)?;
            }
            Ok(())
        }
    }
}

fn scan_expr_functions(expr: &Expr, on_fn: &mut FnScan<'_>) -> Result<(), GuardError> {
    match expr {
        Expr::Function(func) => {
            on_fn(&function_base_name(func))?;
            scan_function_arguments_functions(&func.parameters, on_fn)?;
            scan_function_arguments_functions(&func.args, on_fn)?;
            if let Some(filter) = &func.filter {
                scan_expr_functions(filter, on_fn)?;
            }
            Ok(())
        }
        Expr::Subquery(query) => scan_query_functions(query, on_fn),
        Expr::InSubquery { expr, subquery, .. } => {
            scan_expr_functions(expr, on_fn)?;
            scan_query_functions(subquery, on_fn)
        }
        Expr::Exists { subquery, .. } => scan_query_functions(subquery, on_fn),
        Expr::BinaryOp { left, right, .. } => {
            scan_expr_functions(left, on_fn)?;
            scan_expr_functions(right, on_fn)
        }
        Expr::UnaryOp { expr, .. } => scan_expr_functions(expr, on_fn),
        Expr::Nested(inner) => scan_expr_functions(inner, on_fn),
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                scan_expr_functions(op, on_fn)?;
            }
            for when in conditions {
                scan_expr_functions(&when.condition, on_fn)?;
                scan_expr_functions(&when.result, on_fn)?;
            }
            if let Some(el) = else_result {
                scan_expr_functions(el, on_fn)?;
            }
            Ok(())
        }
        Expr::InList { expr, list, .. } => {
            scan_expr_functions(expr, on_fn)?;
            for item in list {
                scan_expr_functions(item, on_fn)?;
            }
            Ok(())
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            scan_expr_functions(expr, on_fn)?;
            scan_expr_functions(low, on_fn)?;
            scan_expr_functions(high, on_fn)
        }
        Expr::Tuple(items) => {
            for item in items {
                scan_expr_functions(item, on_fn)?;
            }
            Ok(())
        }
        Expr::Array(array) => {
            for item in &array.elem {
                scan_expr_functions(item, on_fn)?;
            }
            Ok(())
        }
        Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Extract { expr, .. }
        | Expr::Ceil { expr, .. }
        | Expr::Floor { expr, .. } => scan_expr_functions(expr, on_fn),
        Expr::Like { expr, pattern, .. }
        | Expr::ILike { expr, pattern, .. }
        | Expr::SimilarTo { expr, pattern, .. } => {
            scan_expr_functions(expr, on_fn)?;
            scan_expr_functions(pattern, on_fn)
        }
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => scan_expr_functions(inner, on_fn),
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            scan_expr_functions(left, on_fn)?;
            scan_expr_functions(right, on_fn)
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            scan_expr_functions(expr, on_fn)?;
            if let Some(from) = substring_from {
                scan_expr_functions(from, on_fn)?;
            }
            if let Some(for_) = substring_for {
                scan_expr_functions(for_, on_fn)?;
            }
            Ok(())
        }
        Expr::Trim {
            expr, trim_what, ..
        } => {
            scan_expr_functions(expr, on_fn)?;
            if let Some(what) = trim_what {
                scan_expr_functions(what, on_fn)?;
            }
            Ok(())
        }
        Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
            ..
        } => {
            scan_expr_functions(expr, on_fn)?;
            scan_expr_functions(overlay_what, on_fn)?;
            scan_expr_functions(overlay_from, on_fn)?;
            if let Some(f) = overlay_for {
                scan_expr_functions(f, on_fn)?;
            }
            Ok(())
        }
        Expr::Interval(interval) => scan_expr_functions(&interval.value, on_fn),
        Expr::JsonAccess { value, path, .. } => {
            scan_expr_functions(value, on_fn)?;
            for elem in &path.path {
                if let sqlparser::ast::JsonPathElem::Bracket { key } = elem {
                    scan_expr_functions(key, on_fn)?;
                }
            }
            Ok(())
        }
        Expr::Rollup(groups) | Expr::Cube(groups) | Expr::GroupingSets(groups) => {
            for group in groups {
                for expr in group {
                    scan_expr_functions(expr, on_fn)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Complexity limits (SQL-BUG-12 fix).
//
// The counters now walk the WHOLE query: CTEs, comma joins (multiple FROM
// items are implicit joins), subqueries in every expression position
// (projection, CASE, function arguments, HAVING, GROUP BY, IN lists, JOIN ON)
// — not just the WHERE clause of the outer SELECT.
// ---------------------------------------------------------------------------

/// Soft limits to stop agent-generated Cartesian explosions before they hit the DB.
const MAX_JOINS: u32 = 8;
const MAX_SUBQUERY_DEPTH: u32 = 5;

fn check_query_complexity(stmt: &Statement) -> Result<(), GuardError> {
    let (joins, depth) = match stmt {
        Statement::Query(q) => complexity_of_query(q, 0),
        Statement::Explain { statement, .. } => match statement.as_ref() {
            Statement::Query(q) => complexity_of_query(q, 0),
            _ => return Ok(()),
        },
        Statement::Insert(insert) => match &insert.source {
            Some(source) => complexity_of_query(source, 0),
            None => return Ok(()),
        },
        _ => return Ok(()),
    };
    if joins > MAX_JOINS {
        return Err(GuardError::Denied(format!(
            "query too complex: {joins} joins (max {MAX_JOINS})"
        )));
    }
    if depth > MAX_SUBQUERY_DEPTH {
        return Err(GuardError::Denied(format!(
            "query too complex: subquery depth {depth} (max {MAX_SUBQUERY_DEPTH})"
        )));
    }
    Ok(())
}

fn complexity_of_query(query: &Query, depth: u32) -> (u32, u32) {
    let mut joins = 0u32;
    let mut max_depth = depth;
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            let (j, d) = complexity_of_query(&cte.query, depth.saturating_add(1));
            joins = joins.saturating_add(j);
            max_depth = max_depth.max(d);
        }
    }
    let (j, d) = complexity_of_set_expr(query.body.as_ref(), depth);
    joins = joins.saturating_add(j);
    max_depth = max_depth.max(d);
    (joins, max_depth)
}

fn complexity_of_set_expr(body: &SetExpr, depth: u32) -> (u32, u32) {
    match body {
        SetExpr::Select(select) => complexity_of_select(select, depth),
        SetExpr::Query(q) => complexity_of_query(q, depth.saturating_add(1)),
        SetExpr::SetOperation { left, right, .. } => {
            let (jl, dl) = complexity_of_set_expr(left.as_ref(), depth);
            let (jr, dr) = complexity_of_set_expr(right.as_ref(), depth);
            (jl.saturating_add(jr), dl.max(dr))
        }
        SetExpr::Values(values) => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            for row in &values.rows {
                for expr in row.iter() {
                    let (j, d) = complexity_of_expr(expr, depth);
                    joins = joins.saturating_add(j);
                    max_depth = max_depth.max(d);
                }
            }
            (joins, max_depth)
        }
        SetExpr::Insert(stmt)
        | SetExpr::Update(stmt)
        | SetExpr::Delete(stmt)
        | SetExpr::Merge(stmt) => match stmt {
            Statement::Insert(insert) => match &insert.source {
                Some(source) => complexity_of_query(source, depth.saturating_add(1)),
                None => (0, depth),
            },
            _ => (0, depth),
        },
        SetExpr::Table(_) => (0, depth),
    }
}

fn complexity_of_select(select: &Select, depth: u32) -> (u32, u32) {
    let mut joins = 0u32;
    let mut max_depth = depth;

    // FROM items: each table factor beyond the first in the whole FROM list
    // is a join — explicit (JOIN) or implicit (comma). Count tables, not
    // just JOIN keywords (SQL-BUG-12).
    let mut table_count = 0u32;
    for twj in &select.from {
        table_count = table_count.saturating_add(1 + twj.joins.len() as u32);
        let (j, d) = complexity_of_table_factor(&twj.relation, depth);
        joins = joins.saturating_add(j);
        max_depth = max_depth.max(d);
        for join in &twj.joins {
            let (j2, d2) = complexity_of_table_factor(&join.relation, depth);
            joins = joins.saturating_add(j2);
            max_depth = max_depth.max(d2);
            if let Some(on) = join_on_expr(&join.join_operator) {
                let (j3, d3) = complexity_of_expr(on, depth);
                joins = joins.saturating_add(j3);
                max_depth = max_depth.max(d3);
            }
        }
    }
    joins = joins.saturating_add(table_count.saturating_sub(1));

    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(expr) => {
                let (j, d) = complexity_of_expr(expr, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            SelectItem::ExprWithAlias { expr, .. } => {
                let (j, d) = complexity_of_expr(expr, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            _ => {}
        }
    }
    if let Some(selection) = &select.selection {
        let (j, d) = complexity_of_expr(selection, depth);
        joins = joins.saturating_add(j);
        max_depth = max_depth.max(d);
    }
    if let GroupByExpr::Expressions(exprs, _) = &select.group_by {
        for expr in exprs {
            let (j, d) = complexity_of_expr(expr, depth);
            joins = joins.saturating_add(j);
            max_depth = max_depth.max(d);
        }
    }
    if let Some(having) = &select.having {
        let (j, d) = complexity_of_expr(having, depth);
        joins = joins.saturating_add(j);
        max_depth = max_depth.max(d);
    }
    if let Some(qualify) = &select.qualify {
        let (j, d) = complexity_of_expr(qualify, depth);
        joins = joins.saturating_add(j);
        max_depth = max_depth.max(d);
    }
    (joins, max_depth)
}

fn complexity_of_table_factor(factor: &TableFactor, depth: u32) -> (u32, u32) {
    match factor {
        TableFactor::Derived { subquery, .. } => {
            complexity_of_query(subquery, depth.saturating_add(1))
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            let mut joins = table_with_joins.joins.len() as u32;
            let (j, d) = complexity_of_table_factor(&table_with_joins.relation, depth);
            joins = joins.saturating_add(j);
            let mut max_depth = d;
            for join in &table_with_joins.joins {
                let (j2, d2) = complexity_of_table_factor(&join.relation, depth);
                joins = joins.saturating_add(j2);
                max_depth = max_depth.max(d2);
            }
            (joins, max_depth)
        }
        TableFactor::Table { args, .. } => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            if let Some(args) = args {
                for arg in &args.args {
                    let (j, d) = complexity_of_function_arg(arg, depth);
                    joins = joins.saturating_add(j);
                    max_depth = max_depth.max(d);
                }
            }
            (joins, max_depth)
        }
        TableFactor::Function { args, .. } => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            for arg in args {
                let (j, d) = complexity_of_function_arg(arg, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
        TableFactor::UNNEST { array_exprs, .. } => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            for expr in array_exprs {
                let (j, d) = complexity_of_expr(expr, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
        _ => (0, depth),
    }
}

fn complexity_of_function_arg(arg: &FunctionArg, depth: u32) -> (u32, u32) {
    let fae = match arg {
        FunctionArg::Unnamed(fae) => fae,
        FunctionArg::Named { arg, .. } | FunctionArg::ExprNamed { arg, .. } => arg,
    };
    match fae {
        FunctionArgExpr::Expr(expr) => complexity_of_expr(expr, depth),
        _ => (0, depth),
    }
}

fn complexity_of_function_arguments(args: &FunctionArguments, depth: u32) -> (u32, u32) {
    match args {
        FunctionArguments::None => (0, depth),
        FunctionArguments::Subquery(query) => complexity_of_query(query, depth.saturating_add(1)),
        FunctionArguments::List(list) => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            for arg in &list.args {
                let (j, d) = complexity_of_function_arg(arg, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
    }
}

fn complexity_of_expr(expr: &Expr, depth: u32) -> (u32, u32) {
    match expr {
        Expr::Subquery(q) => complexity_of_query(q, depth.saturating_add(1)),
        Expr::InSubquery { expr, subquery, .. } => {
            let (j1, d1) = complexity_of_expr(expr, depth);
            let (j2, d2) = complexity_of_query(subquery, depth.saturating_add(1));
            (j1.saturating_add(j2), d1.max(d2))
        }
        Expr::Exists { subquery, .. } => complexity_of_query(subquery, depth.saturating_add(1)),
        Expr::Function(func) => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            for args in [&func.parameters, &func.args] {
                let (j, d) = complexity_of_function_arguments(args, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            if let Some(filter) = &func.filter {
                let (j, d) = complexity_of_expr(filter, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            if let Some(op) = operand {
                let (j, d) = complexity_of_expr(op, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            for when in conditions {
                for e in [&when.condition, &when.result] {
                    let (j, d) = complexity_of_expr(e, depth);
                    joins = joins.saturating_add(j);
                    max_depth = max_depth.max(d);
                }
            }
            if let Some(el) = else_result {
                let (j, d) = complexity_of_expr(el, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
        Expr::BinaryOp { left, right, .. } => {
            let (jl, dl) = complexity_of_expr(left, depth);
            let (jr, dr) = complexity_of_expr(right, depth);
            (jl.saturating_add(jr), dl.max(dr))
        }
        Expr::UnaryOp { expr, .. } => complexity_of_expr(expr, depth),
        Expr::Nested(inner) => complexity_of_expr(inner, depth),
        Expr::InList { expr, list, .. } => {
            let (mut joins, mut max_depth) = complexity_of_expr(expr, depth);
            for item in list {
                let (j, d) = complexity_of_expr(item, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            let (j1, d1) = complexity_of_expr(expr, depth);
            let (j2, d2) = complexity_of_expr(low, depth);
            let (j3, d3) = complexity_of_expr(high, depth);
            (j1.saturating_add(j2).saturating_add(j3), d1.max(d2).max(d3))
        }
        Expr::Tuple(items) => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            for item in items {
                let (j, d) = complexity_of_expr(item, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
        Expr::Array(array) => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            for item in &array.elem {
                let (j, d) = complexity_of_expr(item, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
        Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Extract { expr, .. }
        | Expr::Ceil { expr, .. }
        | Expr::Floor { expr, .. } => complexity_of_expr(expr, depth),
        Expr::Like { expr, pattern, .. }
        | Expr::ILike { expr, pattern, .. }
        | Expr::SimilarTo { expr, pattern, .. } => {
            let (j1, d1) = complexity_of_expr(expr, depth);
            let (j2, d2) = complexity_of_expr(pattern, depth);
            (j1.saturating_add(j2), d1.max(d2))
        }
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => complexity_of_expr(inner, depth),
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            let (j1, d1) = complexity_of_expr(left, depth);
            let (j2, d2) = complexity_of_expr(right, depth);
            (j1.saturating_add(j2), d1.max(d2))
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            let (mut joins, mut max_depth) = complexity_of_expr(expr, depth);
            if let Some(from) = substring_from {
                let (j, d) = complexity_of_expr(from, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            if let Some(for_) = substring_for {
                let (j, d) = complexity_of_expr(for_, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
        Expr::Trim {
            expr, trim_what, ..
        } => {
            let (mut joins, mut max_depth) = complexity_of_expr(expr, depth);
            if let Some(what) = trim_what {
                let (j, d) = complexity_of_expr(what, depth);
                joins = joins.saturating_add(j);
                max_depth = max_depth.max(d);
            }
            (joins, max_depth)
        }
        Expr::Interval(interval) => complexity_of_expr(&interval.value, depth),
        Expr::JsonAccess { value, path, .. } => {
            let (mut joins, mut max_depth) = complexity_of_expr(value, depth);
            for elem in &path.path {
                if let sqlparser::ast::JsonPathElem::Bracket { key } = elem {
                    let (j, d) = complexity_of_expr(key, depth);
                    joins = joins.saturating_add(j);
                    max_depth = max_depth.max(d);
                }
            }
            (joins, max_depth)
        }
        Expr::Rollup(groups) | Expr::Cube(groups) | Expr::GroupingSets(groups) => {
            let mut joins = 0u32;
            let mut max_depth = depth;
            for group in groups {
                for expr in group {
                    let (j, d) = complexity_of_expr(expr, depth);
                    joins = joins.saturating_add(j);
                    max_depth = max_depth.max(d);
                }
            }
            (joins, max_depth)
        }
        _ => (0, depth),
    }
}

fn strip_trailing_semicolons(sql: &str) -> &str {
    let mut s = sql.trim();
    while let Some(rest) = s.strip_suffix(';') {
        s = rest.trim_end();
    }
    s
}

fn expr_as_limit(expr: &Expr) -> Option<u64> {
    match expr {
        Expr::Value(v) => match &v.value {
            SqlValue::Number(n, _) => n.parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

/// The raw limit expression of a query, whatever its shape (SQL-SEC-12).
fn query_limit_expr(q: &Query) -> Option<&Expr> {
    match &q.limit_clause {
        Some(LimitClause::LimitOffset { limit, .. }) => limit.as_ref(),
        Some(LimitClause::OffsetCommaLimit { limit, .. }) => Some(limit),
        None => None,
    }
}

fn set_query_limit(q: &mut Query, max_rows: u32) {
    let limit_expr = Expr::value(SqlValue::Number(max_rows.to_string(), false));
    match &mut q.limit_clause {
        Some(LimitClause::LimitOffset { limit, .. }) => {
            *limit = Some(limit_expr);
        }
        Some(LimitClause::OffsetCommaLimit { limit, .. }) => {
            *limit = limit_expr;
        }
        None => {
            q.limit_clause = Some(LimitClause::LimitOffset {
                limit: Some(limit_expr),
                offset: None,
                limit_by: vec![],
            });
        }
    }
}

fn set_query_offset_limit(q: &mut Query, offset: u64, limit: u64) {
    let limit_expr = Expr::value(SqlValue::Number(limit.to_string(), false));
    let offset_expr = Offset {
        value: Expr::value(SqlValue::Number(offset.to_string(), false)),
        rows: OffsetRows::None,
    };
    q.limit_clause = Some(LimitClause::LimitOffset {
        limit: Some(limit_expr),
        offset: Some(offset_expr),
        limit_by: vec![],
    });
}

/// Clamp literal row limits to `max_rows` and report unverifiable ones.
///
/// Returns `(clamped, unverified)`:
/// * A literal `LIMIT n` / `FETCH FIRST n ROWS` with `n > max_rows` is
///   rewritten to `max_rows` in the AST (`clamped = true`).
/// * A limit that is not a plain literal (placeholder, expression) — or a
///   `FETCH … PERCENT`, whose row count depends on the result size — cannot
///   be proven `<= max_rows`, so the SQL is left alone and
///   `unverified = true`: the execution layer must cap fetched rows itself
///   (SQL-SEC-12; previously such limits escaped clamping entirely).
fn clamp_query_limit(stmt: &mut Statement, max_rows: u32) -> (bool, bool) {
    let Statement::Query(q) = stmt else {
        return (false, false);
    };
    let mut clamped = false;
    let mut unverified = false;

    if let Some(expr) = query_limit_expr(q) {
        match expr_as_limit(expr) {
            Some(current) if current > max_rows as u64 => {
                set_query_limit(q, max_rows);
                clamped = true;
            }
            Some(_) => {}
            None => unverified = true,
        }
    }

    if let Some(fetch) = &mut q.fetch {
        clamp_fetch(fetch, max_rows, &mut clamped, &mut unverified);
    }

    (clamped, unverified)
}

fn clamp_fetch(fetch: &mut Fetch, max_rows: u32, clamped: &mut bool, unverified: &mut bool) {
    if fetch.percent {
        // FETCH FIRST n PERCENT — the absolute row count is unknowable here.
        *unverified = true;
        return;
    }
    match &fetch.quantity {
        None => {} // FETCH FIRST ROW ONLY — exactly one row.
        Some(expr) => match expr_as_limit(expr) {
            Some(current) if current > max_rows as u64 => {
                fetch.quantity = Some(Expr::value(SqlValue::Number(max_rows.to_string(), false)));
                *clamped = true;
            }
            Some(_) => {}
            None => *unverified = true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_drop_in_readonly() {
        let err = validate_and_prepare(
            "DROP TABLE users",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap_err();
        assert!(err.to_string().contains("DDL blocked"));
    }

    #[test]
    fn allows_select_with_limit_injection() {
        let p = validate_and_prepare(
            "SELECT 1",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap();
        assert!(p.limit_injected);
        assert!(!p.limit_unverified);
        assert!(p.sql.ends_with("LIMIT 100"));
    }

    #[test]
    fn allows_insert_with_writes() {
        validate_and_prepare(
            "INSERT INTO t VALUES (1)",
            &[],
            EngineKind::Postgres,
            WriteMode::AllowWrites,
            100,
        )
        .unwrap();
    }

    #[test]
    fn allows_show_processlist_mysql_readonly() {
        validate_and_prepare(
            "SHOW PROCESSLIST",
            &[],
            EngineKind::Mysql,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap();
    }

    #[test]
    fn blocks_batch_multi_statement_in_single_string() {
        let err = validate_and_prepare(
            "SELECT 1; DROP TABLE users",
            &[],
            EngineKind::Mysql,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap_err();
        assert!(err.to_string().contains("multiple statements"));
    }

    #[test]
    fn clamps_explicit_limit_above_max_rows() {
        let p = validate_and_prepare(
            "SELECT * FROM users LIMIT 200",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            50,
        )
        .unwrap();
        assert!(!p.limit_injected);
        assert!(p.limit_clamped);
        assert!(!p.limit_unverified);
        assert!(p.sql.contains("LIMIT 50"));
        assert!(!p.sql.contains("LIMIT 200"));
    }

    #[test]
    fn allows_parameterized_select_mysql() {
        let p = validate_and_prepare(
            "SELECT * FROM t WHERE id = ?",
            &[Value::from(1)],
            EngineKind::Mysql,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap();
        assert_eq!(p.params.len(), 1);
        assert!(p.limit_injected);
    }

    #[test]
    fn allows_parameterized_select_postgres() {
        validate_and_prepare(
            "SELECT * FROM t WHERE id = ?",
            &[Value::from(1)],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap();
    }

    #[test]
    fn server_pagination_injects_offset_limit() {
        let p = validate_and_prepare_with_options(
            "SELECT id FROM pages ORDER BY id",
            &[],
            EngineKind::Sqlite,
            WriteMode::ReadOnly,
            100,
            PrepareOptions {
                page_offset: 50,
                page_size: Some(25),
            },
        )
        .unwrap();
        assert!(p.server_pagination);
        assert!(p.sql.contains("OFFSET 50"));
        assert!(p.sql.contains("LIMIT 76"));
    }

    #[test]
    fn parameterized_limit_clamp_preserves_placeholder() {
        let p = validate_and_prepare(
            "SELECT * FROM t WHERE id = ? LIMIT 200",
            &[Value::from(1)],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            50,
        )
        .unwrap();
        assert!(p.limit_clamped);
        assert_eq!(placeholder_count(&p.sql, EngineKind::Postgres), 1);
    }

    #[test]
    fn blocks_copy_explicitly() {
        let err = validate_and_prepare(
            "COPY users FROM STDIN",
            &[],
            EngineKind::Postgres,
            WriteMode::AllowDdl,
            100,
        )
        .unwrap_err();
        assert!(err.to_string().contains("COPY"));
    }

    #[test]
    fn blocks_attach_database_sqlite() {
        let err = validate_and_prepare(
            "ATTACH DATABASE 'other.db' AS other",
            &[],
            EngineKind::Sqlite,
            WriteMode::AllowDdl,
            100,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("ATTACH") || err.to_string().contains("not allowed"),
            "{err}"
        );
    }

    #[test]
    fn blocks_excessive_joins() {
        let sql = "SELECT * FROM a \
            JOIN b ON a.id = b.id \
            JOIN c ON b.id = c.id \
            JOIN d ON c.id = d.id \
            JOIN e ON d.id = e.id \
            JOIN f ON e.id = f.id \
            JOIN g ON f.id = g.id \
            JOIN h ON g.id = h.id \
            JOIN i ON h.id = i.id \
            JOIN j ON i.id = j.id";
        let err = validate_and_prepare(sql, &[], EngineKind::Postgres, WriteMode::ReadOnly, 100)
            .unwrap_err();
        assert!(err.to_string().contains("joins"), "{err}");
    }

    // --- SQL-SEC-01: data-modifying CTEs are DML, not reads ---

    #[test]
    fn cte_delete_blocked_in_readonly() {
        let err = validate_and_prepare(
            "WITH t AS (DELETE FROM users RETURNING id) SELECT * FROM t",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap_err();
        assert!(err.to_string().contains("DML blocked"), "{err}");
    }

    #[test]
    fn cte_insert_allowed_with_writes() {
        validate_and_prepare(
            "WITH t AS (INSERT INTO users VALUES (1) RETURNING id) SELECT * FROM t",
            &[],
            EngineKind::Postgres,
            WriteMode::AllowWrites,
            100,
        )
        .unwrap();
    }

    #[test]
    fn plain_cte_still_read_with_limit() {
        let p = validate_and_prepare(
            "WITH t AS (SELECT 1 AS x) SELECT * FROM t",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap();
        assert!(p.limit_injected);
    }

    // --- SQL-SEC-02: SELECT INTO is not a read ---

    #[test]
    fn select_into_blocked_in_readonly() {
        let err = validate_and_prepare(
            "SELECT * INTO new_users FROM users",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap_err();
        assert!(err.to_string().contains("DDL blocked"), "{err}");
    }

    #[test]
    fn mysql_outfile_blocked_in_readonly() {
        let err = validate_and_prepare(
            "SELECT * INTO OUTFILE '/tmp/x.txt' FROM users",
            &[],
            EngineKind::Mysql,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("DDL blocked")
                || err.to_string().contains("parse error")
                || err.to_string().contains("OUTFILE"),
            "{err}"
        );
    }

    // --- SQL-SEC-03/04: denied functions, in every mode ---

    #[test]
    fn pg_sleep_denied_in_readonly_and_ddl_modes() {
        for mode in [WriteMode::ReadOnly, WriteMode::AllowDdl] {
            let err =
                validate_and_prepare("SELECT pg_sleep(60)", &[], EngineKind::Postgres, mode, 100)
                    .unwrap_err();
            assert!(err.to_string().contains("pg_sleep"), "{err}");
            assert!(err.to_string().contains("not allowed"), "{err}");
        }
    }

    #[test]
    fn pg_file_and_state_functions_denied() {
        for sql in [
            "SELECT pg_read_file('/etc/passwd')",
            "SELECT lo_export(1, '/tmp/x')",
            "SELECT * FROM dblink('host=evil', 'SELECT 1') AS t(x int)",
            "SELECT nextval('seq_users')",
            "SELECT set_config('x', 'y', false)",
        ] {
            let err =
                validate_and_prepare(sql, &[], EngineKind::Postgres, WriteMode::AllowWrites, 100)
                    .unwrap_err();
            assert!(err.to_string().contains("not allowed"), "{sql} → {err}");
        }
    }

    #[test]
    fn denied_function_found_in_where_case_and_subquery() {
        for sql in [
            "SELECT id FROM t WHERE x = pg_sleep(1)",
            "SELECT CASE WHEN x THEN pg_sleep(1) ELSE 0 END FROM t",
            "SELECT * FROM t WHERE id IN (SELECT pg_sleep(1))",
            "SELECT coalesce(pg_sleep(1), 0) FROM t",
            "WITH s AS (SELECT pg_sleep(1) AS z) SELECT * FROM s",
        ] {
            let err =
                validate_and_prepare(sql, &[], EngineKind::Postgres, WriteMode::ReadOnly, 100)
                    .unwrap_err();
            assert!(err.to_string().contains("pg_sleep"), "{sql} → {err}");
        }
    }

    #[test]
    fn mysql_dos_and_file_functions_denied() {
        for (sql, name) in [
            ("SELECT SLEEP(60)", "sleep"),
            ("SELECT BENCHMARK(1000000, MD5('x'))", "benchmark"),
            ("SELECT LOAD_FILE('/etc/passwd')", "load_file"),
            ("SELECT GET_LOCK('x', 10)", "get_lock"),
        ] {
            let err = validate_and_prepare(sql, &[], EngineKind::Mysql, WriteMode::ReadOnly, 100)
                .unwrap_err();
            assert!(err.to_string().contains(name), "{sql} → {err}");
        }
    }

    #[test]
    fn sqlite_file_functions_denied() {
        let err = validate_and_prepare(
            "SELECT readfile('/etc/passwd')",
            &[],
            EngineKind::Sqlite,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap_err();
        assert!(err.to_string().contains("readfile"), "{err}");
    }

    #[test]
    fn ordinary_functions_still_allowed() {
        validate_and_prepare(
            "SELECT count(*), coalesce(max(x), 0), upper(name) FROM t GROUP BY name",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap();
    }

    #[test]
    fn qualified_denied_function_still_caught() {
        let err = validate_and_prepare(
            "SELECT pg_catalog.pg_sleep(1)",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap_err();
        assert!(err.to_string().contains("pg_sleep"), "{err}");
    }

    // --- SQL-SEC-12: unverifiable limits are flagged, literal FETCH clamps ---

    #[test]
    fn parameterized_limit_is_unverified_not_clamped() {
        let p = validate_and_prepare(
            "SELECT * FROM t LIMIT ?",
            &[Value::from(1000)],
            EngineKind::Mysql,
            WriteMode::ReadOnly,
            50,
        )
        .unwrap();
        assert!(p.limit_unverified);
        assert!(!p.limit_clamped);
        assert!(!p.limit_injected);
    }

    #[test]
    fn pg_numbered_limit_is_unverified() {
        let p = validate_and_prepare(
            "SELECT * FROM t LIMIT $1",
            &[Value::from(1000)],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            50,
        )
        .unwrap();
        assert!(p.limit_unverified);
    }

    #[test]
    fn fetch_first_literal_is_clamped() {
        let p = validate_and_prepare(
            "SELECT * FROM t FETCH FIRST 200 ROWS ONLY",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            50,
        )
        .unwrap();
        assert!(p.limit_clamped);
        assert!(!p.limit_unverified);
        assert!(p.sql.contains("50"), "sql={}", p.sql);
        assert!(!p.sql.contains("200"), "sql={}", p.sql);
    }

    #[test]
    fn fetch_first_within_max_is_verified() {
        let p = validate_and_prepare(
            "SELECT * FROM t FETCH FIRST 10 ROWS ONLY",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            50,
        )
        .unwrap();
        assert!(!p.limit_clamped);
        assert!(!p.limit_unverified);
    }

    #[test]
    fn fetch_percent_is_unverified() {
        let p = validate_and_prepare(
            "SELECT * FROM t FETCH FIRST 10 PERCENT ROWS ONLY",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            50,
        )
        .unwrap();
        assert!(p.limit_unverified);
    }

    // --- SQL-SEC-13: GRANT/REVOKE are Admin ---

    #[test]
    fn grant_blocked_below_ddl_mode() {
        for mode in [WriteMode::ReadOnly, WriteMode::AllowWrites] {
            let err = validate_and_prepare(
                "GRANT SELECT ON users TO bob",
                &[],
                EngineKind::Postgres,
                mode,
                100,
            )
            .unwrap_err();
            assert!(err.to_string().contains("--allow-grants"), "{err}");
        }
    }

    #[test]
    fn grant_allowed_at_guard_level_in_ddl_mode() {
        // The guard's bar is AllowDdl; the additional --allow-grants opt-in
        // gate is enforced at the config/tools layer (see enforce_write_mode).
        validate_and_prepare(
            "REVOKE SELECT ON users FROM bob",
            &[],
            EngineKind::Postgres,
            WriteMode::AllowDdl,
            100,
        )
        .unwrap();
    }

    // --- SQL-BUG-15: plain EXPLAIN of an allowed DDL is not false-positived ---

    #[test]
    fn explain_of_ddl_allowed_in_ddl_mode() {
        validate_and_prepare(
            "EXPLAIN DROP TABLE users",
            &[],
            EngineKind::Postgres,
            WriteMode::AllowDdl,
            100,
        )
        .unwrap();
    }

    #[test]
    fn explain_of_ddl_still_blocked_in_readonly() {
        let err = validate_and_prepare(
            "EXPLAIN DROP TABLE users",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap_err();
        assert!(err.to_string().contains("DDL blocked"), "{err}");
    }

    // --- SQL-BUG-12: complexity walker sees comma joins & all subqueries ---

    #[test]
    fn blocks_excessive_comma_joins() {
        let err = validate_and_prepare(
            "SELECT * FROM a, b, c, d, e, f, g, h, i, j",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap_err();
        assert!(err.to_string().contains("joins"), "{err}");
    }

    #[test]
    fn moderate_comma_joins_still_allowed() {
        validate_and_prepare(
            "SELECT * FROM a, b, c WHERE a.id = b.id AND b.id = c.id",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap();
    }

    #[test]
    fn blocks_deep_subquery_in_projection() {
        // Six nested scalar subqueries in the SELECT list — the old walker
        // never looked at the projection and let this through.
        let sql = "SELECT (SELECT (SELECT (SELECT (SELECT (SELECT (SELECT 1))))))";
        let err = validate_and_prepare(sql, &[], EngineKind::Postgres, WriteMode::ReadOnly, 100)
            .unwrap_err();
        assert!(err.to_string().contains("subquery depth"), "{err}");
    }

    #[test]
    fn shallow_subqueries_still_allowed() {
        validate_and_prepare(
            "SELECT (SELECT max(x) FROM t2) FROM t1 WHERE EXISTS (SELECT 1 FROM t3)",
            &[],
            EngineKind::Postgres,
            WriteMode::ReadOnly,
            100,
        )
        .unwrap();
    }
}
