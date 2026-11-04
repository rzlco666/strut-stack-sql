//! Table allow/deny rules, row filters and column masks (R2-04).
//!
//! These types live in the guard because the guard is where a query's table
//! references are visible: enforcing them anywhere else would leave the raw
//! SQL path unguarded. `strut-sql-core` re-exports them for config/policy.

use sqlparser::ast::{Query, Select, SetExpr, Statement, TableAlias, TableFactor};

use crate::GuardError;

/// One table in an allow/deny list. `schema = None` matches any schema.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TableRule {
    pub schema: Option<String>,
    pub table: String,
}

impl TableRule {
    pub fn matches(&self, schema: Option<&str>, table: &str) -> bool {
        self.table.eq_ignore_ascii_case(table)
            && match (&self.schema, schema) {
                (None, _) => true,
                (Some(expected), Some(actual)) => expected.eq_ignore_ascii_case(actual),
                (Some(_), None) => false,
            }
    }
}

/// A row filter is a boolean SQL predicate injected as `AND (<predicate>)`
/// into queries that reference the table. It is validated at startup by the
/// guard, so it cannot carry a second statement or a dangerous function.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RowFilter {
    pub schema: Option<String>,
    pub table: String,
    pub predicate: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MaskMode {
    /// Keep a recognisable prefix (first character, plus the domain for
    /// email-like values).
    Partial,
    /// Replace the value with its SHA-256 hex digest.
    Hash,
    /// Replace the value with JSON null.
    Null,
}

/// Column mask. `table = None` applies to a column name on every table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ColumnMask {
    #[serde(default)]
    pub table: Option<TableRule>,
    pub column: String,
    pub mode: MaskMode,
}

/// Effective table/column rules for one authorization.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TablePolicy {
    /// When non-empty, only these tables are reachable.
    pub allow: Vec<TableRule>,
    /// Always unreachable; deny wins over allow.
    pub deny: Vec<TableRule>,
    pub row_filters: Vec<RowFilter>,
    pub masks: Vec<ColumnMask>,
}

impl TablePolicy {
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty()
            && self.deny.is_empty()
            && self.row_filters.is_empty()
            && self.masks.is_empty()
    }

    /// Deny wins over allow. An empty allow list means "every table not denied".
    pub fn allows_table(&self, schema: Option<&str>, table: &str) -> bool {
        if self.deny.iter().any(|rule| rule.matches(schema, table)) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.iter().any(|rule| rule.matches(schema, table))
    }

    pub fn row_filter_for(&self, schema: Option<&str>, table: &str) -> Option<&RowFilter> {
        self.row_filters.iter().find(|filter| {
            filter.table.eq_ignore_ascii_case(table)
                && match (&filter.schema, schema) {
                    (None, _) => true,
                    (Some(expected), Some(actual)) => expected.eq_ignore_ascii_case(actual),
                    (Some(_), None) => false,
                }
        })
    }

    pub fn mask_for(&self, table: &str, column: &str) -> Option<MaskMode> {
        self.masks
            .iter()
            .filter(|mask| mask.column.eq_ignore_ascii_case(column))
            .find(|mask| match &mask.table {
                None => true,
                Some(rule) => rule.matches(None, table) || rule.table.eq_ignore_ascii_case(table),
            })
            .map(|mask| mask.mode)
    }

    /// Union of several grants for the same source: any deny wins, allows are
    /// unioned, filters and masks accumulate.
    pub fn merge(&mut self, other: &TablePolicy) {
        self.allow.extend(other.allow.iter().cloned());
        self.deny.extend(other.deny.iter().cloned());
        for filter in &other.row_filters {
            if !self.row_filters.iter().any(|f| {
                f.table.eq_ignore_ascii_case(&filter.table) && f.predicate == filter.predicate
            }) {
                self.row_filters.push(filter.clone());
            }
        }
        self.masks.extend(other.masks.iter().cloned());
    }
}

/// A table reference found in a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub table: String,
}

fn table_ref_from_name(name: &sqlparser::ast::ObjectName) -> TableRef {
    let qualified = name.to_string().replace(['"', '`'], "");
    match qualified.rsplit_once('.') {
        Some((schema, table)) => TableRef {
            schema: Some(schema.to_string()),
            table: table.to_string(),
        },
        None => TableRef {
            schema: None,
            table: qualified,
        },
    }
}

fn collect_from_select(select: &Select, out: &mut Vec<TableRef>) {
    for item in &select.from {
        collect_table_factor(&item.relation, out);
        for join in &item.joins {
            collect_table_factor(&join.relation, out);
        }
    }
}

fn collect_table_factor(factor: &TableFactor, out: &mut Vec<TableRef>) {
    match factor {
        TableFactor::Table { name, .. } => out.push(table_ref_from_name(name)),
        TableFactor::Derived { subquery, .. } => collect_query(subquery, out),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            collect_table_factor(&table_with_joins.relation, out);
            for join in &table_with_joins.joins {
                collect_table_factor(&join.relation, out);
            }
        }
        _ => {}
    }
}

fn collect_query(query: &Query, out: &mut Vec<TableRef>) {
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            collect_query(&cte.query, out);
        }
    }
    collect_set_expr(&query.body, out);
}

fn collect_set_expr(body: &SetExpr, out: &mut Vec<TableRef>) {
    match body {
        SetExpr::Select(select) => collect_from_select(select, out),
        SetExpr::SetOperation { left, right, .. } => {
            collect_set_expr(left, out);
            collect_set_expr(right, out);
        }
        SetExpr::Query(query) => collect_query(query, out),
        _ => {}
    }
}

/// Every real table the statement reads or writes: FROM/JOIN targets,
/// derived-table subqueries, CTE bodies, and INSERT/UPDATE/DELETE targets.
/// CTE names themselves are not reported (they are not tables).
pub fn referenced_tables(
    sql: &str,
    engine: strut_sql_driver_api::EngineKind,
) -> Result<Vec<TableRef>, GuardError> {
    use crate::dialect_for;
    use sqlparser::parser::Parser;

    let normalized = crate::normalize_sql_for_engine(sql, engine);
    let statements = Parser::parse_sql(dialect_for(engine).as_ref(), &normalized)
        .map_err(|e| GuardError::Denied(format!("parse error: {e}")))?;
    let mut out = Vec::new();
    for statement in &statements {
        collect_statement_tables(statement, &mut out);
    }
    dedup_tables(out)
}

fn collect_statement_tables(statement: &Statement, out: &mut Vec<TableRef>) {
    fn collect_table_with_joins(item: &sqlparser::ast::TableWithJoins, out: &mut Vec<TableRef>) {
        collect_table_factor(&item.relation, out);
        for join in &item.joins {
            collect_table_factor(&join.relation, out);
        }
    }
    match statement {
        Statement::Query(query) => collect_query(query, out),
        Statement::Insert(insert) => {
            if let sqlparser::ast::TableObject::TableName(name) = &insert.table {
                out.push(table_ref_from_name(name));
            }
            if let Some(source) = &insert.source {
                collect_query(source, out);
            }
        }
        Statement::Update(update) => {
            collect_table_with_joins(&update.table, out);
            if let Some(from) = &update.from {
                let tables = match from {
                    sqlparser::ast::UpdateTableFromKind::BeforeSet(tables)
                    | sqlparser::ast::UpdateTableFromKind::AfterSet(tables) => tables,
                };
                for item in tables {
                    collect_table_with_joins(item, out);
                }
            }
        }
        Statement::Delete(delete) => {
            for name in &delete.tables {
                out.push(table_ref_from_name(name));
            }
            let from_tables = match &delete.from {
                sqlparser::ast::FromTable::WithFromKeyword(tables)
                | sqlparser::ast::FromTable::WithoutKeyword(tables) => tables,
            };
            for item in from_tables {
                collect_table_with_joins(item, out);
            }
            if let Some(using) = &delete.using {
                for item in using {
                    collect_table_with_joins(item, out);
                }
            }
        }
        Statement::Explain { statement, .. } => collect_statement_tables(statement, out),
        Statement::CreateTable(create) => {
            out.push(table_ref_from_name(&create.name));
        }
        Statement::Drop { names, .. } => {
            for name in names {
                out.push(table_ref_from_name(name));
            }
        }
        _ => {}
    }
}

fn dedup_tables(tables: Vec<TableRef>) -> Result<Vec<TableRef>, GuardError> {
    let mut out: Vec<TableRef> = Vec::new();
    for table in tables {
        if !out.iter().any(|t| t == &table) {
            out.push(table);
        }
    }
    Ok(out)
}

/// Check the allow/deny rules for every referenced table and wrap filtered
/// tables as `FROM (SELECT * FROM t WHERE (predicate)) AS t`.
///
/// Row filters are only applied to SELECT paths. A statement whose family is
/// DML/DDL is rejected when any referenced table carries a filter: returning
/// unfiltered rows or mutating unfiltered rows is never acceptable.
pub fn enforce_table_policy(
    statement: &mut Statement,
    class: &crate::StmtClass,
    policy: &TablePolicy,
    engine: strut_sql_driver_api::EngineKind,
) -> Result<(), GuardError> {
    let tables = if engine == strut_sql_driver_api::EngineKind::DuckDb {
        crate::duckdb::policy_tables(statement, policy)?
    } else {
        let mut tables = Vec::new();
        collect_statement_tables(statement, &mut tables);
        tables
    };
    let tables = dedup_tables(tables)?;
    for table in &tables {
        if !policy.allows_table(table.schema.as_deref(), &table.table) {
            return Err(GuardError::Denied(format!(
                "table '{}' is not accessible to this principal",
                table.table
            )));
        }
    }
    if policy.row_filters.is_empty() {
        return Ok(());
    }
    let select = match statement {
        Statement::Query(query) => query,
        Statement::Explain { statement, .. } => match statement.as_mut() {
            Statement::Query(query) => query,
            _ => {
                return Err(GuardError::Denied(
                    "row filters cannot be applied to this statement".into(),
                ))
            }
        },
        _ => {
            return Err(GuardError::Denied(
                "row filters apply to SELECT statements only; this statement is rejected \
                 rather than run unfiltered"
                    .into(),
            ));
        }
    };
    apply_filters_to_query(select, policy, engine)?;
    // Any table that still lacks its filter means the traversal missed a
    // reference (expression subquery, unusual shape): reject instead of
    // silently running unfiltered.
    let mut after = Vec::new();
    collect_statement_tables(statement, &mut after);
    for table in dedup_tables(after)? {
        if policy
            .row_filter_for(table.schema.as_deref(), &table.table)
            .is_some()
            && !policy.row_filters.is_empty()
            && !filtered_positions_include(statement, policy, &table)
        {
            return Err(GuardError::Denied(format!(
                "row filter for table '{}' could not be applied to this statement; \
                 rewrite it as a simple SELECT",
                table.table
            )));
        }
    }
    if !class.is_select_like() {
        return Err(GuardError::Denied(
            "row filters apply to SELECT statements only".into(),
        ));
    }
    Ok(())
}

fn filtered_positions_include(
    statement: &Statement,
    policy: &TablePolicy,
    table: &TableRef,
) -> bool {
    // The rewrite replaces every supported occurrence with a derived table
    // whose subquery already carries the filter; the remaining check only has
    // to prove the table is no longer reachable as a bare FROM target.
    let mut bare = Vec::new();
    collect_bare_tables(statement, &mut bare);
    !bare.iter().any(|t| {
        t == table
            && policy
                .row_filter_for(t.schema.as_deref(), &t.table)
                .is_some()
    })
}

fn collect_bare_tables(statement: &Statement, out: &mut Vec<TableRef>) {
    // Same traversal as collect_statement_tables but stops at Derived
    // subqueries: a filtered table wrapped in a derived table is no longer a
    // bare reference.
    fn select_bare(select: &Select, out: &mut Vec<TableRef>) {
        for item in &select.from {
            if let TableFactor::Table { name, .. } = &item.relation {
                out.push(table_ref_from_name(name));
            }
            for join in &item.joins {
                if let TableFactor::Table { name, .. } = &join.relation {
                    out.push(table_ref_from_name(name));
                }
            }
        }
    }
    match statement {
        Statement::Query(query) => {
            if let SetExpr::Select(select) = query.body.as_ref() {
                select_bare(select, out);
            }
        }
        Statement::Explain { statement, .. } => collect_bare_tables(statement, out),
        _ => {}
    }
}

fn apply_filters_to_query(
    query: &mut Query,
    policy: &TablePolicy,
    engine: strut_sql_driver_api::EngineKind,
) -> Result<(), GuardError> {
    if let Some(with) = &mut query.with {
        for cte in &mut with.cte_tables {
            apply_filters_to_query(&mut cte.query, policy, engine)?;
        }
    }
    match query.body.as_mut() {
        SetExpr::Select(select) => apply_filters_to_select(select, policy, engine),
        SetExpr::SetOperation { left, right, .. } => {
            apply_filters_to_set_expr(left, policy, engine)?;
            apply_filters_to_set_expr(right, policy, engine)
        }
        SetExpr::Query(inner) => apply_filters_to_query(inner, policy, engine),
        _ => Ok(()),
    }
}

fn apply_filters_to_set_expr(
    body: &mut SetExpr,
    policy: &TablePolicy,
    engine: strut_sql_driver_api::EngineKind,
) -> Result<(), GuardError> {
    match body {
        SetExpr::Select(select) => apply_filters_to_select(select, policy, engine),
        SetExpr::SetOperation { left, right, .. } => {
            apply_filters_to_set_expr(left, policy, engine)?;
            apply_filters_to_set_expr(right, policy, engine)
        }
        SetExpr::Query(inner) => apply_filters_to_query(inner, policy, engine),
        _ => Ok(()),
    }
}

fn apply_filters_to_select(
    select: &mut Select,
    policy: &TablePolicy,
    engine: strut_sql_driver_api::EngineKind,
) -> Result<(), GuardError> {
    for item in &mut select.from {
        apply_filter_to_factor(&mut item.relation, policy, engine)?;
        for join in &mut item.joins {
            apply_filter_to_factor(&mut join.relation, policy, engine)?;
        }
    }
    Ok(())
}

fn apply_filter_to_factor(
    factor: &mut TableFactor,
    policy: &TablePolicy,
    engine: strut_sql_driver_api::EngineKind,
) -> Result<(), GuardError> {
    match factor {
        TableFactor::Derived { subquery, .. } => apply_filters_to_query(subquery, policy, engine),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            apply_filter_to_factor(&mut table_with_joins.relation, policy, engine)?;
            for join in &mut table_with_joins.joins {
                apply_filter_to_factor(&mut join.relation, policy, engine)?;
            }
            Ok(())
        }
        TableFactor::Table { name, alias, .. } => {
            let table = table_ref_from_name(name);
            let Some(filter) = policy.row_filter_for(table.schema.as_deref(), &table.table) else {
                return Ok(());
            };
            let predicate_sql = &filter.predicate;
            let dialect_sql = format!("SELECT * FROM {name} WHERE ({predicate_sql})");
            let inner = parse_single_query(&dialect_sql, engine)?;
            let alias = alias.clone().or(Some(TableAlias {
                explicit: false,
                name: sqlparser::ast::Ident::new(table.table.clone()),
                columns: vec![],
                at: None,
            }));
            *factor = TableFactor::Derived {
                lateral: false,
                subquery: Box::new(inner),
                alias,
                sample: None,
            };
            Ok(())
        }
        _ => Ok(()),
    }
}

fn parse_single_query(
    sql: &str,
    engine: strut_sql_driver_api::EngineKind,
) -> Result<Query, GuardError> {
    use crate::dialect_for;
    use sqlparser::parser::Parser;
    let statements = Parser::parse_sql(dialect_for(engine).as_ref(), sql)
        .map_err(|e| GuardError::Denied(format!("row filter does not parse: {e}")))?;
    match statements.into_iter().next() {
        Some(Statement::Query(query)) => Ok(*query),
        _ => Err(GuardError::Denied("row filter is not a SELECT".into())),
    }
}

impl TablePolicy {
    /// Mask for a returned column, ignoring table provenance (results do not
    /// carry it). Table-scoped masks still match their column name.
    pub fn mask_mode_for_column(&self, column: &str) -> Option<MaskMode> {
        self.masks
            .iter()
            .find(|mask| mask.column.eq_ignore_ascii_case(column))
            .map(|mask| mask.mode)
    }
}
