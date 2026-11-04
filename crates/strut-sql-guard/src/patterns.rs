//! AST-level query anti-pattern detection (PRD-F3-06).
//!
//! Detection runs on the parsed statement, never by executing SQL. The
//! caller may supply catalog context (wide tables, indexed columns) so
//! `SELECT *` on wide tables and functions over indexed columns can be
//! judged; structural patterns are detected without any catalog access.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, JoinConstraint, ObjectName, Query,
    Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
};
use strut_sql_driver_api::EngineKind;

use crate::GuardError;

/// One structured anti-pattern finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AntiPattern {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub suggestion: String,
}

/// Catalog context for the two context-dependent detectors.
#[derive(Debug, Clone)]
pub struct PatternContext {
    /// Table name -> column count.
    pub wide_tables: BTreeMap<String, usize>,
    /// A table is "wide" at or above this column count.
    pub wide_threshold: usize,
    /// Table name -> indexed column names (case-insensitive comparison).
    pub indexed_columns: BTreeMap<String, BTreeSet<String>>,
}

impl Default for PatternContext {
    fn default() -> Self {
        Self {
            wide_tables: BTreeMap::new(),
            wide_threshold: 20,
            indexed_columns: BTreeMap::new(),
        }
    }
}

impl PatternContext {
    fn is_wide(&self, table: &str) -> bool {
        self.wide_tables
            .get(table)
            .is_some_and(|n| *n >= self.wide_threshold)
    }

    fn is_indexed(&self, table: Option<&str>, column: &str) -> bool {
        let column = column.to_ascii_lowercase();
        match table {
            Some(table) => self
                .indexed_columns
                .get(table)
                .is_some_and(|cols| cols.iter().any(|c| c.eq_ignore_ascii_case(&column))),
            None => self
                .indexed_columns
                .values()
                .any(|cols| cols.iter().any(|c| c.eq_ignore_ascii_case(&column))),
        }
    }
}

const OVERSIZED_IN: usize = 100;

/// Detect anti-patterns in one SQL statement. Parse failures return the
/// guard's parse error so callers can decide whether to suppress it.
pub fn detect_anti_patterns(
    sql: &str,
    engine: EngineKind,
    ctx: &PatternContext,
) -> Result<Vec<AntiPattern>, GuardError> {
    let stmt = crate::parse_single_statement(sql, engine)?;
    let mut found = Vec::new();
    walk_statement(&stmt, ctx, &mut found);
    found.sort_by(|a, b| {
        (a.code.as_str(), a.message.as_str()).cmp(&(b.code.as_str(), b.message.as_str()))
    });
    found.dedup();
    Ok(found)
}

fn walk_statement(stmt: &Statement, ctx: &PatternContext, out: &mut Vec<AntiPattern>) {
    match stmt {
        Statement::Query(q) => walk_query(q, ctx, &[], out),
        Statement::Explain { statement, .. } => walk_statement(statement, ctx, out),
        Statement::Insert(insert) => {
            if let Some(source) = &insert.source {
                walk_query(source, ctx, &[], out);
            }
        }
        Statement::Update { .. } | Statement::Delete(_) => {}
        _ => {}
    }
}

fn walk_query(query: &Query, ctx: &PatternContext, outer: &[String], out: &mut Vec<AntiPattern>) {
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            walk_query(&cte.query, ctx, outer, out);
        }
    }
    walk_set_expr(&query.body, ctx, outer, out);
}

fn walk_set_expr(
    body: &SetExpr,
    ctx: &PatternContext,
    outer: &[String],
    out: &mut Vec<AntiPattern>,
) {
    match body {
        SetExpr::Select(select) => walk_select(select, ctx, outer, out),
        SetExpr::Query(q) => walk_query(q, ctx, outer, out),
        SetExpr::SetOperation { left, right, .. } => {
            walk_set_expr(left, ctx, outer, out);
            walk_set_expr(right, ctx, outer, out);
        }
        _ => {}
    }
}

fn walk_select(
    select: &Select,
    ctx: &PatternContext,
    outer: &[String],
    out: &mut Vec<AntiPattern>,
) {
    let tables = select_table_names(select);
    let mut local_outer = outer.to_vec();
    local_outer.extend(tables.iter().cloned());

    // Comma joins / join without predicate.
    if select.from.len() > 1 && select.selection.is_none() {
        out.push(pattern(
            "comma_join_cartesian",
            "warning",
            "multiple FROM tables without a WHERE predicate (Cartesian product)",
            "Add an explicit JOIN ... ON predicate between the tables.",
        ));
    }
    for twj in &select.from {
        for join in &twj.joins {
            if matches!(
                join.join_operator,
                sqlparser::ast::JoinOperator::CrossJoin(_)
            ) || matches!(
                &join.join_operator,
                sqlparser::ast::JoinOperator::Inner(JoinConstraint::None)
                    | sqlparser::ast::JoinOperator::Left(JoinConstraint::None)
                    | sqlparser::ast::JoinOperator::Right(JoinConstraint::None)
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::None)
            ) {
                out.push(pattern(
                    "comma_join_cartesian",
                    "warning",
                    "join without an ON/USING predicate (Cartesian product)",
                    "Add an explicit join predicate.",
                ));
            }
        }
    }

    // SELECT * on a wide table.
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => {
                if tables.iter().any(|t| ctx.is_wide(t)) {
                    out.push(pattern(
                        "select_star_wide",
                        "warning",
                        "SELECT * projects every column of a wide table",
                        "Project only the columns you need.",
                    ));
                }
            }
            SelectItem::QualifiedWildcard(kind, _) => {
                let table = match kind {
                    sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(name) => {
                        object_name_last(name)
                    }
                    sqlparser::ast::SelectItemQualifiedWildcardKind::Expr(expr) => {
                        expr_identifier(expr).map(|(_, column)| column)
                    }
                };
                if let Some(table) = table {
                    if ctx.is_wide(&table) {
                        out.push(pattern(
                            "select_star_wide",
                            "warning",
                            &format!("SELECT {table}.* projects every column of a wide table"),
                            "Project only the columns you need.",
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    // Correlated subquery in the projection list.
    for item in &select.projection {
        if let SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } = item {
            if expr_has_correlated_subquery(expr, &local_outer) {
                out.push(pattern(
                    "correlated_subquery_projection",
                    "warning",
                    "correlated subquery in the projection list",
                    "Rewrite as a join/derived table or aggregate the subquery once.",
                ));
            }
        }
    }

    // WHERE/HAVING expression patterns.
    for expr in [&select.selection, &select.having].into_iter().flatten() {
        walk_predicate(expr, ctx, out);
    }

    // Derived tables / subqueries in FROM are analyzed with this query's
    // tables as their outer scope.
    for twj in &select.from {
        walk_table_factor(&twj.relation, ctx, &local_outer, out);
        for join in &twj.joins {
            walk_table_factor(&join.relation, ctx, &local_outer, out);
        }
    }
}

fn walk_table_factor(
    factor: &TableFactor,
    ctx: &PatternContext,
    outer: &[String],
    out: &mut Vec<AntiPattern>,
) {
    match factor {
        TableFactor::Derived { subquery, .. } => walk_query(subquery, ctx, outer, out),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            walk_table_factor(&table_with_joins.relation, ctx, outer, out);
            for join in &table_with_joins.joins {
                walk_table_factor(&join.relation, ctx, outer, out);
            }
        }
        _ => {}
    }
}

fn walk_predicate(expr: &Expr, ctx: &PatternContext, out: &mut Vec<AntiPattern>) {
    match expr {
        Expr::Like {
            pattern: like_pattern,
            ..
        }
        | Expr::ILike {
            pattern: like_pattern,
            ..
        } => {
            if pattern_starts_with_wildcard(like_pattern) {
                out.push(pattern(
                    "leading_wildcard_like",
                    "warning",
                    "LIKE pattern starts with a wildcard; B-tree indexes cannot serve it",
                    "Use a prefix/trigram index or restructure the predicate.",
                ));
            }
            walk_predicate(like_pattern, ctx, out);
        }
        Expr::InList { list, .. } if list.len() >= OVERSIZED_IN => {
            out.push(pattern(
                "oversized_in_list",
                "info",
                &format!("IN list has {} values", list.len()),
                "Use a VALUES join, a temporary table, or a range predicate.",
            ));
        }
        Expr::Function(func) => {
            if let Some((table, column)) = first_identifier_arg(&func.args) {
                if ctx.is_indexed(table.as_deref(), &column) {
                    out.push(pattern(
                        "function_on_indexed_column",
                        "warning",
                        "function wraps an indexed column in a WHERE/HAVING predicate",
                        "Move the function to the constant side or add an expression index.",
                    ));
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            walk_predicate(left, ctx, out);
            walk_predicate(right, ctx, out);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. } => walk_predicate(expr, ctx, out),
        Expr::Between {
            expr, low, high, ..
        } => {
            walk_predicate(expr, ctx, out);
            walk_predicate(low, ctx, out);
            walk_predicate(high, ctx, out);
        }
        Expr::InList { expr, list, .. } => {
            walk_predicate(expr, ctx, out);
            for item in list {
                walk_predicate(item, ctx, out);
            }
        }
        Expr::InSubquery { expr, subquery, .. } => {
            walk_predicate(expr, ctx, out);
            walk_predicate_query(subquery, ctx, out);
        }
        Expr::Exists { subquery, .. } | Expr::Subquery(subquery) => {
            walk_predicate_query(subquery, ctx, out);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                walk_predicate(operand, ctx, out);
            }
            for when in conditions {
                walk_predicate(&when.condition, ctx, out);
                walk_predicate(&when.result, ctx, out);
            }
            if let Some(else_result) = else_result {
                walk_predicate(else_result, ctx, out);
            }
        }
        _ => {}
    }
}

fn walk_predicate_query(query: &Query, ctx: &PatternContext, out: &mut Vec<AntiPattern>) {
    walk_query(query, ctx, &[], out);
}

fn pattern_starts_with_wildcard(pattern: &Expr) -> bool {
    match pattern {
        Expr::Value(value) => value
            .value
            .clone()
            .into_string()
            .is_some_and(|s| s.starts_with('%') || s.starts_with('_')),
        _ => false,
    }
}

fn expr_has_correlated_subquery(expr: &Expr, outer: &[String]) -> bool {
    match expr {
        Expr::Subquery(query)
        | Expr::Exists {
            subquery: query, ..
        } => query_references_tables(query, outer),
        Expr::InSubquery { expr, subquery, .. } => {
            expr_has_correlated_subquery(expr, outer) || query_references_tables(subquery, outer)
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_correlated_subquery(left, outer) || expr_has_correlated_subquery(right, outer)
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. } => expr_has_correlated_subquery(expr, outer),
        Expr::Function(func) => match &func.args {
            FunctionArguments::List(list) => list.args.iter().any(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                }
                | FunctionArg::ExprNamed {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } => expr_has_correlated_subquery(e, outer),
                _ => false,
            }),
            _ => false,
        },
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            operand
                .as_ref()
                .is_some_and(|o| expr_has_correlated_subquery(o, outer))
                || conditions.iter().any(|c| {
                    expr_has_correlated_subquery(&c.condition, outer)
                        || expr_has_correlated_subquery(&c.result, outer)
                })
                || else_result
                    .as_ref()
                    .is_some_and(|e| expr_has_correlated_subquery(e, outer))
        }
        _ => false,
    }
}

fn query_references_tables(query: &Query, outer: &[String]) -> bool {
    if outer.is_empty() {
        return false;
    }
    let mut names = Vec::new();
    collect_referenced_identifiers(query, &mut names);
    names.iter().any(|(table, _)| {
        table
            .as_ref()
            .is_some_and(|t| outer.iter().any(|o| o.eq_ignore_ascii_case(t)))
    })
}

fn collect_referenced_identifiers(query: &Query, out: &mut Vec<(Option<String>, String)>) {
    let mut collector = IdentifierCollector { out };
    collector.query(query);
}

struct IdentifierCollector<'a> {
    out: &'a mut Vec<(Option<String>, String)>,
}

impl IdentifierCollector<'_> {
    fn query(&mut self, query: &Query) {
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                self.query(&cte.query);
            }
        }
        self.set_expr(&query.body);
    }

    fn set_expr(&mut self, body: &SetExpr) {
        match body {
            SetExpr::Select(select) => self.select(select),
            SetExpr::Query(q) => self.query(q),
            SetExpr::SetOperation { left, right, .. } => {
                self.set_expr(left);
                self.set_expr(right);
            }
            _ => {}
        }
    }

    fn select(&mut self, select: &Select) {
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    self.expr(expr)
                }
                _ => {}
            }
        }
        if let Some(selection) = &select.selection {
            self.expr(selection);
        }
        if let Some(having) = &select.having {
            self.expr(having);
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Identifier(ident) => self.out.push((None, ident.value.clone())),
            Expr::CompoundIdentifier(parts) => {
                if parts.len() >= 2 {
                    self.out.push((
                        Some(parts[parts.len() - 2].value.clone()),
                        parts[parts.len() - 1].value.clone(),
                    ));
                }
            }
            Expr::BinaryOp { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Nested(expr)
            | Expr::Cast { expr, .. }
            | Expr::Collate { expr, .. } => self.expr(expr),
            Expr::Function(func) => {
                if let FunctionArguments::List(list) = &func.args {
                    for arg in &list.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            self.expr(e);
                        }
                    }
                }
            }
            Expr::Subquery(q) | Expr::Exists { subquery: q, .. } => self.query(q),
            Expr::InSubquery { expr, subquery, .. } => {
                self.expr(expr);
                self.query(subquery);
            }
            _ => {}
        }
    }
}

fn first_identifier_arg(args: &FunctionArguments) -> Option<(Option<String>, String)> {
    let FunctionArguments::List(list) = args else {
        return None;
    };
    for arg in &list.args {
        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg {
            if let Some(found) = expr_identifier(expr) {
                return Some(found);
            }
        }
    }
    None
}

fn expr_identifier(expr: &Expr) -> Option<(Option<String>, String)> {
    match expr {
        Expr::Identifier(ident) => Some((None, ident.value.clone())),
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => Some((
            Some(parts[parts.len() - 2].value.clone()),
            parts[parts.len() - 1].value.clone(),
        )),
        Expr::Cast { expr, .. } | Expr::Nested(expr) | Expr::Collate { expr, .. } => {
            expr_identifier(expr)
        }
        _ => None,
    }
}

fn select_table_names(select: &Select) -> Vec<String> {
    let mut names = Vec::new();
    for twj in &select.from {
        collect_factor_names(&twj.relation, &mut names);
        for join in &twj.joins {
            collect_factor_names(&join.relation, &mut names);
        }
    }
    names
}

fn collect_factor_names(factor: &TableFactor, names: &mut Vec<String>) {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            if let Some(last) = object_name_last(name) {
                names.push(last);
            }
            if let Some(alias) = alias {
                names.push(alias.name.value.clone());
            }
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => collect_twj_names(table_with_joins, names),
        _ => {}
    }
}

fn collect_twj_names(twj: &TableWithJoins, names: &mut Vec<String>) {
    collect_factor_names(&twj.relation, names);
    for join in &twj.joins {
        collect_factor_names(&join.relation, names);
    }
}

fn object_name_last(name: &ObjectName) -> Option<String> {
    name.0
        .last()
        .and_then(|part| part.as_ident())
        .map(|ident| ident.value.clone())
}

fn pattern(code: &str, severity: &str, message: &str, suggestion: &str) -> AntiPattern {
    AntiPattern {
        code: code.to_string(),
        severity: severity.to_string(),
        message: message.to_string(),
        suggestion: suggestion.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn ctx() -> PatternContext {
        let mut context = PatternContext {
            wide_threshold: 20,
            ..PatternContext::default()
        };
        context.wide_tables.insert("users".into(), 32);
        context
            .indexed_columns
            .insert("users".into(), BTreeSet::from(["email".into()]));
        context
    }

    fn codes(sql: &str) -> Vec<String> {
        detect_anti_patterns(sql, EngineKind::Postgres, &ctx())
            .expect("parse")
            .into_iter()
            .map(|p| p.code)
            .collect()
    }

    #[test]
    fn detects_all_six_required_patterns() {
        assert!(codes("SELECT * FROM users").contains(&"select_star_wide".to_string()));
        assert!(codes("SELECT id FROM a, b").contains(&"comma_join_cartesian".to_string()));
        assert!(codes("SELECT id FROM users WHERE lower(email) = 'x'")
            .contains(&"function_on_indexed_column".to_string()));
        assert!(codes("SELECT id FROM users WHERE name LIKE '%bob'")
            .contains(&"leading_wildcard_like".to_string()));
        let values = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        assert!(
            codes(&format!("SELECT id FROM users WHERE id IN ({values})"))
                .contains(&"oversized_in_list".to_string())
        );
        assert!(codes(
            "SELECT (SELECT max(o.total) FROM orders o WHERE o.user_id = u.id) FROM users u"
        )
        .contains(&"correlated_subquery_projection".to_string()));
    }

    #[test]
    fn clean_query_has_no_findings() {
        assert!(codes("SELECT id FROM users WHERE id = 1").is_empty());
    }

    #[test]
    fn wide_table_context_is_required_for_select_star() {
        let mut without = PatternContext::default();
        without.wide_tables.insert("users".into(), 5);
        let found = detect_anti_patterns("SELECT * FROM users", EngineKind::Postgres, &without)
            .expect("parse");
        assert!(found.is_empty());
    }

    #[test]
    fn map_default_threshold_is_deterministic() {
        let map = BTreeMap::from([("users".to_string(), 20usize)]);
        let context = PatternContext {
            wide_tables: map,
            ..PatternContext::default()
        };
        assert!(context.is_wide("users"));
        assert!(!context.is_wide("orders"));
    }
}
