use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, FunctionArguments, ObjectName, Query, SetExpr, Statement, TableFactor, Visit, Visitor,
};

use crate::{function_base_name, object_name_base_name, FnScan, GuardError, TablePolicy, TableRef};

// Extension readers have many aliases, and new extensions add more. Deny the
// external-reader families as well as the built-in dynamic SQL entry points.
// This is defense in depth: the connection also disables external access.
pub(crate) fn denied_function_reason(name: &str) -> Option<&'static str> {
    if name.starts_with("read_")
        || name.starts_with("write_")
        || name.ends_with("_scan")
        || name.ends_with("_scan_pushdown")
        || name.ends_with("_attach")
        || name.ends_with("_query")
        || name.ends_with("_execute")
        || name.starts_with("iceberg_")
        || name.starts_with("ducklake_")
        || name.starts_with("st_read")
        || matches!(name, "glob" | "getenv" | "http_get" | "http_post")
    {
        return Some("external file, network, or database access is forbidden");
    }
    if matches!(name, "query" | "query_table") {
        return Some("dynamic SQL or dynamic table access bypasses SQL/table policy checks");
    }
    if matches!(
        name,
        "install_extension"
            | "load_extension"
            | "force_install"
            | "update_extensions"
            | "enable_profiling"
            | "disable_profiling"
            | "create_secret"
            | "which_secret"
            | "duckdb_secrets"
            | "register_filesystem"
            | "register_arrow"
    ) {
        return Some("extension, secret, or connection-state primitives are not exposed");
    }
    None
}

pub(crate) fn scan_functions(stmt: &Statement, on_fn: &mut FnScan<'_>) -> Result<(), GuardError> {
    struct Functions<'a, 'b> {
        on_fn: &'a mut FnScan<'b>,
    }
    impl Visitor for Functions<'_, '_> {
        type Break = GuardError;

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            if let Expr::Function(function) = expr {
                if let Err(error) = (self.on_fn)(&function_base_name(function)) {
                    return ControlFlow::Break(error);
                }
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            if contains_table_command(&query.body) {
                return ControlFlow::Break(GuardError::Denied(
                    "DuckDB TABLE shorthand is not exposed; use SELECT FROM an explicit table"
                        .into(),
                ));
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            let name = match factor {
                TableFactor::Table { name, args, .. } => {
                    // DuckDB replacement scans interpret filenames as external
                    // readers, including quoted names rather than function calls.
                    if args.is_none() && replacement_scan_name(name) {
                        return ControlFlow::Break(GuardError::Denied(
                            "DuckDB file/path replacement scans are not allowed".into(),
                        ));
                    }
                    args.as_ref().map(|_| name)
                }
                TableFactor::Function { name, .. } => Some(name),
                _ => None,
            };
            if let Some(name) = name {
                if let Err(error) = (self.on_fn)(&object_name_base_name(name)) {
                    return ControlFlow::Break(error);
                }
            }
            ControlFlow::Continue(())
        }
    }
    match stmt.visit(&mut Functions { on_fn }) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

fn replacement_scan_name(name: &ObjectName) -> bool {
    name.0.iter().any(|part| {
        let Some(ident) = part.as_ident() else {
            return true;
        };
        // A dot inside one identifier is a filename-like name, unlike the
        // separate identifiers in schema.table. Reject ambiguous names rather
        // than guess which replacement-scan extensions are installed.
        ident.quote_style == Some('\'') || ident.value.contains(['.', '/', '\\', ':', '*', '?'])
    })
}

fn contains_table_command(body: &SetExpr) -> bool {
    match body {
        SetExpr::Table(_) => true,
        SetExpr::SetOperation { left, right, .. } => {
            contains_table_command(left) || contains_table_command(right)
        }
        _ => false,
    }
}

// The older engines retain their existing traversal. DuckDB's extra expression
// and relation forms require the derived visitor, otherwise e.g. a table in a
// struct expression or an ASOF/PIVOT subtree can escape table authorization.
pub(crate) fn policy_tables(
    stmt: &Statement,
    policy: &TablePolicy,
) -> Result<Vec<TableRef>, GuardError> {
    struct Tables<'a> {
        policy: &'a TablePolicy,
        tables: Vec<TableRef>,
    }
    impl Visitor for Tables<'_> {
        type Break = GuardError;

        fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
            if !matches!(
                statement,
                Statement::Query(_)
                    | Statement::Explain { .. }
                    | Statement::Insert(_)
                    | Statement::Update(_)
                    | Statement::Delete(_)
                    | Statement::Merge(_)
            ) {
                return ControlFlow::Break(GuardError::Denied(
                    "DuckDB table policy cannot be proven for this statement; use an explicit SELECT or DML statement".into(),
                ));
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            if contains_table_command(&query.body) {
                return ControlFlow::Break(GuardError::Denied(
                    "DuckDB TABLE shorthand is not supported with a table policy".into(),
                ));
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_relation(&mut self, name: &ObjectName) -> ControlFlow<Self::Break> {
            if name.0.len() > 2 || replacement_scan_name(name) {
                // Policies identify schema.table, not catalog.schema.table.
                return ControlFlow::Break(GuardError::Denied(
                    "DuckDB table policy requires an explicit schema.table reference without a catalog or path".into(),
                ));
            }
            if !self.policy.row_filters.is_empty()
                && name
                    .0
                    .iter()
                    .filter_map(|part| part.as_ident())
                    .any(|ident| ident.value.contains(['"', '`']))
            {
                return ControlFlow::Break(GuardError::Denied(
                    "DuckDB row filters cannot be proven for identifiers containing quote characters"
                        .into(),
                ));
            }
            let Some(last) = name.0.last().and_then(|part| part.as_ident()) else {
                return ControlFlow::Break(GuardError::Denied(
                    "DuckDB table policy requires a literal table name".into(),
                ));
            };
            let table = TableRef {
                schema: (name.0.len() == 2)
                    .then(|| name.0[0].as_ident().map(|ident| ident.value.clone()))
                    .flatten(),
                table: last.value.clone(),
            };
            self.tables.push(table);
            ControlFlow::Continue(())
        }

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            if !self.policy.row_filters.is_empty()
                && (matches!(
                    expr,
                    Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. }
                ) || matches!(expr, Expr::Function(function)
                    if matches!(&function.args, FunctionArguments::Subquery(_))
                        || matches!(&function.parameters, FunctionArguments::Subquery(_))))
            {
                return ControlFlow::Break(GuardError::Denied(
                    "DuckDB row filters cannot be proven inside expression subqueries; use a FROM subquery".into(),
                ));
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            // The shared row-filter rewriter knows only these shapes. Reject
            // table functions/macros and new relation forms under a policy,
            // rather than infer which hidden tables they might access.
            if !matches!(
                factor,
                TableFactor::Table { args: None, .. }
                    | TableFactor::Derived { .. }
                    | TableFactor::NestedJoin { .. }
            ) {
                return ControlFlow::Break(GuardError::Denied(
                    "DuckDB table policy cannot be proven for table functions or this relation form"
                        .into(),
                ));
            }
            ControlFlow::Continue(())
        }
    }
    let mut visitor = Tables {
        policy,
        tables: Vec::new(),
    };
    match stmt.visit(&mut visitor) {
        ControlFlow::Continue(()) => Ok(visitor.tables),
        ControlFlow::Break(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        validate_and_prepare, validate_and_prepare_with_options, PrepareOptions, WriteMode,
    };
    use strut_sql_driver_api::EngineKind;

    #[test]
    fn duckdb_native_select_and_question_parameters_are_supported() {
        let prepared = validate_and_prepare(
            "FROM items SELECT ? AS id,",
            &[serde_json::json!(7)],
            EngineKind::DuckDb,
            WriteMode::ReadOnly,
            10,
        )
        .unwrap();
        assert!(prepared.class.is_select_like());
        assert!(prepared.limit_injected);
        assert_eq!(prepared.params, vec![serde_json::json!(7)]);
    }

    #[test]
    fn dangerous_duckdb_calls_are_denied_in_every_position_and_mode() {
        for sql in [
            "SELECT * FROM read_csv('secret.csv')",
            "SELECT * FROM main.query_table('secret')",
            "SELECT * FROM query('SELECT * FROM secret')",
            "SELECT {'hidden': read_text('secret.txt')}",
            "SELECT 1 ORDER BY getenv('HOME')",
            "SELECT * FROM parquet_scan('https://example.test/data.parquet')",
            "CREATE TABLE stolen AS SELECT * FROM read_json('secret.json')",
            "SELECT * FROM 'secret.parquet'",
            "SELECT * FROM \"secret.csv\"",
            "INSTALL httpfs",
            "LOAD httpfs",
            "ATTACH 'other.duckdb' AS other",
            "COPY (SELECT 1) TO 'out.csv'",
        ] {
            for mode in [
                WriteMode::ReadOnly,
                WriteMode::AllowWrites,
                WriteMode::AllowDdl,
            ] {
                assert!(
                    validate_and_prepare(sql, &[], EngineKind::DuckDb, mode, 10).is_err(),
                    "unsafe DuckDB SQL accepted: {sql}"
                );
            }
        }
        assert!(validate_and_prepare(
            "SELECT * FROM range(3)",
            &[],
            EngineKind::DuckDb,
            WriteMode::ReadOnly,
            10,
        )
        .is_ok());
    }

    #[test]
    fn duckdb_policy_covers_expression_subqueries_and_fails_closed_for_filters() {
        let policy = TablePolicy {
            deny: vec![crate::TableRule {
                schema: None,
                table: "secret".into(),
            }],
            ..TablePolicy::default()
        };
        let run = |sql: &str, policy: TablePolicy| {
            validate_and_prepare_with_options(
                sql,
                &[],
                EngineKind::DuckDb,
                WriteMode::ReadOnly,
                10,
                PrepareOptions {
                    table_policy: Some(policy),
                    ..PrepareOptions::default()
                },
            )
        };
        assert!(run("SELECT {'hidden': (SELECT id FROM secret)}", policy.clone()).is_err());
        assert!(run("SELECT * FROM main.secret", policy.clone()).is_err());
        assert!(run("SELECT * FROM memory.main.secret", policy.clone()).is_err());
        assert!(run("SELECT * FROM range(3)", policy).is_err());
        let filtered = TablePolicy {
            row_filters: vec![crate::RowFilter {
                schema: None,
                table: "items".into(),
                predicate: "tenant_id = 7".into(),
            }],
            ..TablePolicy::default()
        };
        assert!(run("SELECT (SELECT id FROM items)", filtered.clone()).is_err());
        let prepared = run("SELECT * FROM items", filtered).unwrap();
        assert!(prepared.sql.contains("tenant_id"));
    }
}
