use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Query, Select, SetExpr,
    Statement,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlClass {
    Read,
    Dml,
    Ddl,
    /// GRANT / REVOKE — privilege management. Deliberately NOT lumped in with
    /// Ddl (SQL-SEC-13): holding DDL rights must not silently include the
    /// right to hand out database privileges.
    Admin,
    Txn,
    Other,
}

#[derive(Debug, Clone)]
pub struct StmtClass {
    class: SqlClass,
    label: String,
    explain_analyze: bool,
}

impl StmtClass {
    pub fn sql_class(&self) -> SqlClass {
        self.class
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn is_select_like(&self) -> bool {
        matches!(self.class, SqlClass::Read) && self.label == "SELECT"
    }

    pub fn requires_writes_for_explain(&self) -> bool {
        self.explain_analyze
    }
}

pub fn classify(stmt: &Statement) -> Result<StmtClass, crate::guard::GuardError> {
    match stmt {
        // SQL-SEC-01/02 fix: a Query is NOT automatically a read. Its CTEs and
        // body are inspected recursively — a data-modifying CTE
        // (`WITH t AS (DELETE … RETURNING …) SELECT …`), a `SELECT … INTO`,
        // or a locking clause (`FOR UPDATE`) all change the class.
        Statement::Query(query) => Ok(classify_query(query)),

        Statement::Explain {
            analyze, statement, ..
        } => {
            let inner = classify(statement)?;
            // SQL-BUG-15 fix: a plain EXPLAIN keeps the class of the statement
            // it explains, so `EXPLAIN <ddl>` in a DDL-allowing mode is no
            // longer false-positived as a read (and `EXPLAIN <dml>` in
            // read-only mode is rejected for the right reason). EXPLAIN
            // ANALYZE actually executes the inner statement, so it keeps
            // requiring write permission via `explain_analyze`.
            Ok(StmtClass {
                class: inner.class,
                label: if *analyze {
                    "EXPLAIN ANALYZE".into()
                } else {
                    "EXPLAIN".into()
                },
                explain_analyze: *analyze || inner.requires_writes_for_explain(),
            })
        }

        Statement::ExplainTable { .. } => Ok(StmtClass {
            class: SqlClass::Read,
            label: "DESCRIBE".into(),
            explain_analyze: false,
        }),

        Statement::ShowTables { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowCatalogs { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowCollation { .. }
        | Statement::ShowProcessList { .. } => Ok(StmtClass {
            class: SqlClass::Read,
            label: "SHOW".into(),
            explain_analyze: false,
        }),

        Statement::Insert(_) => Ok(dml("INSERT")),
        Statement::Update { .. } => Ok(dml("UPDATE")),
        Statement::Delete(_) => Ok(dml("DELETE")),
        Statement::Merge { .. } => Ok(dml("MERGE")),

        Statement::Truncate { .. } => Ok(ddl("TRUNCATE")),
        Statement::Drop { .. } => Ok(ddl("DROP")),
        Statement::AlterTable { .. } => Ok(ddl("ALTER TABLE")),
        Statement::AlterIndex { .. } => Ok(ddl("ALTER INDEX")),
        Statement::CreateTable(_) => Ok(ddl("CREATE TABLE")),
        Statement::CreateView { .. } => Ok(ddl("CREATE VIEW")),
        Statement::CreateIndex(_) => Ok(ddl("CREATE INDEX")),
        Statement::CreateSchema { .. } => Ok(ddl("CREATE SCHEMA")),
        Statement::CreateDatabase { .. } => Ok(ddl("CREATE DATABASE")),
        Statement::CreateFunction(_) => Ok(ddl("CREATE FUNCTION")),
        // SQL-SEC-13 fix: privilege management gets its own class.
        Statement::Grant { .. } => Ok(admin("GRANT")),
        Statement::Revoke { .. } => Ok(admin("REVOKE")),

        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. } => Ok(StmtClass {
            class: SqlClass::Txn,
            label: "TRANSACTION".into(),
            explain_analyze: false,
        }),

        // Explicit denylist (always SqlClass::Other → enforce denies).
        Statement::Copy { .. } => Ok(other("COPY")),
        Statement::Call(_) => Ok(other("CALL")),
        Statement::AttachDatabase { .. } => Ok(other("ATTACH DATABASE")),
        Statement::LoadData { .. } => Ok(other("LOAD DATA")),
        Statement::Execute { .. } => Ok(other("EXECUTE")),
        Statement::Prepare { .. } => Ok(other("PREPARE")),

        other => {
            let dbg = format!("{other:?}");
            let head = dbg
                .split(['(', ' ', '{'])
                .next()
                .unwrap_or("statement")
                .to_string();
            Ok(StmtClass {
                class: SqlClass::Other,
                label: head,
                explain_analyze: false,
            })
        }
    }
}

/// What kind of hidden write a syntactically-SELECT query carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HiddenWrite {
    /// Data-modifying statement nested in a CTE or set-operation branch.
    Dml(&'static str),
    /// `SELECT … INTO …` (creates a table, or — MySQL — writes a server file
    /// via INTO OUTFILE / INTO DUMPFILE; both surface as `Select.into`).
    Into,
}

fn classify_query(query: &Query) -> StmtClass {
    if let Some(write) = query_hidden_write(query) {
        return match write {
            HiddenWrite::Dml(kind) => StmtClass {
                class: SqlClass::Dml,
                label: format!("CTE {kind}"),
                explain_analyze: false,
            },
            HiddenWrite::Into => StmtClass {
                class: SqlClass::Ddl,
                label: "SELECT INTO".into(),
                explain_analyze: false,
            },
        };
    }
    // `SELECT … FOR UPDATE / FOR SHARE` takes row locks — not a pure read.
    if !query.locks.is_empty() {
        return StmtClass {
            class: SqlClass::Dml,
            label: "SELECT FOR UPDATE".into(),
            explain_analyze: false,
        };
    }
    StmtClass {
        class: SqlClass::Read,
        label: "SELECT".into(),
        explain_analyze: false,
    }
}

/// Recursively look for a hidden write inside a query: in every CTE, in the
/// query body (including DML set-expressions, `Select.into`, derived tables)
/// and inside subqueries hiding in expressions.
fn query_hidden_write(query: &Query) -> Option<HiddenWrite> {
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            if let Some(w) = query_hidden_write(&cte.query) {
                return Some(w);
            }
        }
    }
    set_expr_hidden_write(query.body.as_ref())
}

fn set_expr_hidden_write(body: &SetExpr) -> Option<HiddenWrite> {
    match body {
        SetExpr::Insert(_) => Some(HiddenWrite::Dml("INSERT")),
        SetExpr::Update(_) => Some(HiddenWrite::Dml("UPDATE")),
        SetExpr::Delete(_) => Some(HiddenWrite::Dml("DELETE")),
        SetExpr::Merge(_) => Some(HiddenWrite::Dml("MERGE")),
        SetExpr::Select(select) => select_hidden_write(select),
        SetExpr::Query(query) => query_hidden_write(query),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_hidden_write(left).or_else(|| set_expr_hidden_write(right))
        }
        SetExpr::Values(_) | SetExpr::Table(_) => None,
    }
}

fn select_hidden_write(select: &Select) -> Option<HiddenWrite> {
    // SQL-SEC-02: SELECT INTO (new table) and MySQL INTO OUTFILE/DUMPFILE
    // (server-side file write) are both represented by `Select.into`.
    if select.into.is_some() {
        return Some(HiddenWrite::Into);
    }
    for item in &select.projection {
        match item {
            sqlparser::ast::SelectItem::UnnamedExpr(expr) => {
                if let Some(w) = expr_hidden_write(expr) {
                    return Some(w);
                }
            }
            sqlparser::ast::SelectItem::ExprWithAlias { expr, .. } => {
                if let Some(w) = expr_hidden_write(expr) {
                    return Some(w);
                }
            }
            _ => {}
        }
    }
    for twj in &select.from {
        if let Some(w) = table_factor_hidden_write(&twj.relation) {
            return Some(w);
        }
        for join in &twj.joins {
            if let Some(w) = table_factor_hidden_write(&join.relation) {
                return Some(w);
            }
        }
    }
    if let Some(selection) = &select.selection {
        if let Some(w) = expr_hidden_write(selection) {
            return Some(w);
        }
    }
    if let Some(having) = &select.having {
        if let Some(w) = expr_hidden_write(having) {
            return Some(w);
        }
    }
    None
}

fn table_factor_hidden_write(factor: &sqlparser::ast::TableFactor) -> Option<HiddenWrite> {
    match factor {
        sqlparser::ast::TableFactor::Derived { subquery, .. } => query_hidden_write(subquery),
        sqlparser::ast::TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            if let Some(w) = table_factor_hidden_write(&table_with_joins.relation) {
                return Some(w);
            }
            for join in &table_with_joins.joins {
                if let Some(w) = table_factor_hidden_write(&join.relation) {
                    return Some(w);
                }
            }
            None
        }
        _ => None,
    }
}

/// Descend into an expression looking for a subquery that hides a write.
/// Only subquery-bearing positions matter here; scalar structure is walked
/// just far enough to reach them.
fn expr_hidden_write(expr: &Expr) -> Option<HiddenWrite> {
    match expr {
        Expr::Subquery(query) => query_hidden_write(query),
        Expr::InSubquery { subquery, .. } => query_hidden_write(subquery),
        Expr::Exists { subquery, .. } => query_hidden_write(subquery),
        Expr::BinaryOp { left, right, .. } => {
            expr_hidden_write(left).or_else(|| expr_hidden_write(right))
        }
        Expr::UnaryOp { expr, .. } => expr_hidden_write(expr),
        Expr::Nested(inner) => expr_hidden_write(inner),
        Expr::Function(func) => function_hidden_write(func),
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                if let Some(w) = expr_hidden_write(op) {
                    return Some(w);
                }
            }
            for when in conditions {
                if let Some(w) = expr_hidden_write(&when.condition) {
                    return Some(w);
                }
                if let Some(w) = expr_hidden_write(&when.result) {
                    return Some(w);
                }
            }
            if let Some(el) = else_result {
                return expr_hidden_write(el);
            }
            None
        }
        Expr::InList { expr, list, .. } => {
            if let Some(w) = expr_hidden_write(expr) {
                return Some(w);
            }
            for item in list {
                if let Some(w) = expr_hidden_write(item) {
                    return Some(w);
                }
            }
            None
        }
        Expr::Between {
            expr, low, high, ..
        } => expr_hidden_write(expr)
            .or_else(|| expr_hidden_write(low))
            .or_else(|| expr_hidden_write(high)),
        Expr::Tuple(items) => {
            for item in items {
                if let Some(w) = expr_hidden_write(item) {
                    return Some(w);
                }
            }
            None
        }
        Expr::Cast { expr, .. } | Expr::Collate { expr, .. } => expr_hidden_write(expr),
        _ => None,
    }
}

fn function_hidden_write(func: &Function) -> Option<HiddenWrite> {
    for args in [&func.parameters, &func.args] {
        match args {
            FunctionArguments::Subquery(query) => {
                if let Some(w) = query_hidden_write(query) {
                    return Some(w);
                }
            }
            FunctionArguments::List(list) => {
                for arg in &list.args {
                    let expr = match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                        FunctionArg::Named { arg, .. } | FunctionArg::ExprNamed { arg, .. } => {
                            match arg {
                                FunctionArgExpr::Expr(e) => Some(e),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(e) = expr {
                        if let Some(w) = expr_hidden_write(e) {
                            return Some(w);
                        }
                    }
                }
            }
            FunctionArguments::None => {}
        }
    }
    if let Some(filter) = &func.filter {
        return expr_hidden_write(filter);
    }
    None
}

fn dml(label: &str) -> StmtClass {
    StmtClass {
        class: SqlClass::Dml,
        label: label.into(),
        explain_analyze: false,
    }
}

fn ddl(label: &str) -> StmtClass {
    StmtClass {
        class: SqlClass::Ddl,
        label: label.into(),
        explain_analyze: false,
    }
}

fn admin(label: &str) -> StmtClass {
    StmtClass {
        class: SqlClass::Admin,
        label: label.into(),
        explain_analyze: false,
    }
}

fn other(label: &str) -> StmtClass {
    StmtClass {
        class: SqlClass::Other,
        label: label.into(),
        explain_analyze: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn classify_pg(sql: &str) -> StmtClass {
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap();
        assert_eq!(stmts.len(), 1);
        classify(&stmts[0]).unwrap()
    }

    #[test]
    fn plain_select_is_read() {
        let c = classify_pg("SELECT id FROM users WHERE id = 1");
        assert_eq!(c.sql_class(), SqlClass::Read);
        assert!(c.is_select_like());
    }

    #[test]
    fn plain_cte_select_is_read() {
        let c = classify_pg("WITH t AS (SELECT 1 AS x) SELECT * FROM t");
        assert_eq!(c.sql_class(), SqlClass::Read);
        assert!(c.is_select_like());
    }

    // SQL-SEC-01 regression tests.
    #[test]
    fn cte_delete_is_dml_not_read() {
        let c = classify_pg("WITH t AS (DELETE FROM users RETURNING id) SELECT * FROM t");
        assert_eq!(c.sql_class(), SqlClass::Dml, "label={}", c.label());
        assert!(!c.is_select_like());
    }

    #[test]
    fn cte_update_and_insert_are_dml() {
        let u = classify_pg("WITH t AS (UPDATE users SET x = 1 RETURNING id) SELECT * FROM t");
        assert_eq!(u.sql_class(), SqlClass::Dml);
        let i =
            classify_pg("WITH t AS (INSERT INTO users VALUES (1) RETURNING id) SELECT * FROM t");
        assert_eq!(i.sql_class(), SqlClass::Dml);
    }

    #[test]
    fn nested_cte_write_is_found() {
        let c = classify_pg(
            "WITH a AS (SELECT 1), b AS (DELETE FROM t RETURNING id) SELECT * FROM a, b",
        );
        assert_eq!(c.sql_class(), SqlClass::Dml);
    }

    // SQL-SEC-02 regression tests.
    #[test]
    fn select_into_is_not_read() {
        let c = classify_pg("SELECT * INTO new_users FROM users");
        assert_eq!(c.sql_class(), SqlClass::Ddl, "label={}", c.label());
    }

    #[test]
    fn mysql_into_outfile_is_not_read() {
        // sqlparser 0.62 cannot represent INTO OUTFILE at all, so the guard
        // rejects it pre-parse with an explicit denial (SQL-SEC-02) instead
        // of ever classifying it as a read.
        let err = crate::guard::classify_sql(
            "SELECT * FROM users INTO OUTFILE '/tmp/x.txt'",
            crate::db::EngineKind::Mysql,
        )
        .unwrap_err();
        assert!(err.to_string().contains("OUTFILE"), "{err}");
    }

    #[test]
    fn select_for_update_is_not_plain_read() {
        let c = classify_pg("SELECT * FROM users WHERE id = 1 FOR UPDATE");
        assert_eq!(c.sql_class(), SqlClass::Dml);
    }

    // SQL-SEC-13 regression tests.
    #[test]
    fn grant_and_revoke_are_admin_not_ddl() {
        let g = classify_pg("GRANT SELECT ON users TO bob");
        assert_eq!(g.sql_class(), SqlClass::Admin);
        let r = classify_pg("REVOKE SELECT ON users FROM bob");
        assert_eq!(r.sql_class(), SqlClass::Admin);
    }

    // SQL-BUG-15 regression tests.
    #[test]
    fn plain_explain_of_ddl_keeps_ddl_class() {
        let c = classify_pg("EXPLAIN DROP TABLE users");
        assert_eq!(c.sql_class(), SqlClass::Ddl, "label={}", c.label());
        assert!(!c.requires_writes_for_explain());
    }

    #[test]
    fn plain_explain_of_select_stays_read() {
        let c = classify_pg("EXPLAIN SELECT * FROM users");
        assert_eq!(c.sql_class(), SqlClass::Read);
    }

    #[test]
    fn explain_analyze_still_requires_writes() {
        let c = classify_pg("EXPLAIN ANALYZE SELECT * FROM users");
        assert!(c.requires_writes_for_explain());
    }
}
