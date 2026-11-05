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
    /// When non-empty, qualified table names may only reference these
    /// databases. The engine decides what a qualifier means: MySQL and
    /// ClickHouse treat the first part as a database, SQLite as an attached
    /// database, SQL Server as a three-part database component; PostgreSQL
    /// and DuckDB qualifiers are schema (or catalog.schema) names and are
    /// not restricted here. Unqualified names always resolve on the
    /// connected database.
    #[serde(default)]
    pub databases: Vec<String>,
    pub row_filters: Vec<RowFilter>,
    pub masks: Vec<ColumnMask>,
}

impl TablePolicy {
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty()
            && self.deny.is_empty()
            && self.databases.is_empty()
            && self.row_filters.is_empty()
            && self.masks.is_empty()
    }

    /// True when the engine's table qualifier denotes a database, so the
    /// database allowlist applies. PostgreSQL and DuckDB schemas are not
    /// databases and stay unrestricted.
    pub fn scopes_databases(engine: strut_sql_driver_api::EngineKind) -> bool {
        matches!(
            engine,
            strut_sql_driver_api::EngineKind::MySql
                | strut_sql_driver_api::EngineKind::ClickHouse
                | strut_sql_driver_api::EngineKind::Sqlite
                | strut_sql_driver_api::EngineKind::SqlServer
        )
    }

    /// Empty list means "no database restriction". Only meaningful for
    /// engines where [`Self::scopes_databases`] is true.
    pub fn allows_database(
        &self,
        engine: strut_sql_driver_api::EngineKind,
        database: &str,
    ) -> bool {
        if self.databases.is_empty() || !Self::scopes_databases(engine) {
            return true;
        }
        self.databases
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(database))
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
        for database in &other.databases {
            if !self
                .databases
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(database))
            {
                self.databases.push(database.clone());
            }
        }
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
///
/// `database` is set only on SQL Server, for names with more than two
/// components (`database.schema.table`, and linked-server
/// `server.database.schema.table`). SQL Server policy enforcement rejects such
/// references instead of guessing which database the name resolves to.
/// PostgreSQL/MySQL keep their existing rule: the schema is everything before
/// the final component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub table: String,
    pub database: Option<String>,
}

pub(crate) fn object_name_parts(name: &sqlparser::ast::ObjectName) -> Vec<String> {
    name.0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|ident| ident.value.clone())
                .unwrap_or_else(|| part.to_string())
        })
        .collect()
}

/// Use the AST identifier components, never a rendered name split on `.`:
/// `dbo.[ledger.2026]]raw]` is schema `dbo` plus table `ledger.2026]raw`, and
/// `"ledger[2026]"` is one table name that happens to contain brackets/dots.
fn table_ref_from_name(
    name: &sqlparser::ast::ObjectName,
    engine: strut_sql_driver_api::EngineKind,
) -> TableRef {
    use strut_sql_driver_api::EngineKind;

    let parts = object_name_parts(name);
    match parts.as_slice() {
        [] => TableRef {
            schema: None,
            table: String::new(),
            database: None,
        },
        [table] => TableRef {
            schema: None,
            table: table.clone(),
            database: None,
        },
        [schema, table] => TableRef {
            schema: Some(schema.clone()),
            table: table.clone(),
            database: None,
        },
        _ if engine == EngineKind::SqlServer => {
            let table = parts[parts.len() - 1].clone();
            let schema = parts[parts.len() - 2].clone();
            TableRef {
                schema: Some(schema),
                table,
                database: Some(parts[..parts.len() - 2].join(".")),
            }
        }
        _ => {
            // PG/MySQL multi-part names keep the pre-existing schema/table
            // split: every component except the last is the schema. This is
            // SQLServer-only policy hardening, not a cross-engine change.
            let table = parts[parts.len() - 1].clone();
            TableRef {
                schema: Some(parts[..parts.len() - 1].join(".")),
                table,
                database: None,
            }
        }
    }
}

fn collect_from_select(
    select: &Select,
    out: &mut Vec<TableRef>,
    engine: strut_sql_driver_api::EngineKind,
) {
    for item in &select.from {
        collect_table_factor(&item.relation, out, engine);
        for join in &item.joins {
            collect_table_factor(&join.relation, out, engine);
        }
    }
}

fn collect_table_factor(
    factor: &TableFactor,
    out: &mut Vec<TableRef>,
    engine: strut_sql_driver_api::EngineKind,
) {
    match factor {
        TableFactor::Table { name, .. } => out.push(table_ref_from_name(name, engine)),
        TableFactor::Derived { subquery, .. } => collect_query(subquery, out, engine),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            collect_table_factor(&table_with_joins.relation, out, engine);
            for join in &table_with_joins.joins {
                collect_table_factor(&join.relation, out, engine);
            }
        }
        _ => {}
    }
}

fn collect_query(query: &Query, out: &mut Vec<TableRef>, engine: strut_sql_driver_api::EngineKind) {
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            collect_query(&cte.query, out, engine);
        }
    }
    collect_set_expr(&query.body, out, engine);
}

fn collect_set_expr(
    body: &SetExpr,
    out: &mut Vec<TableRef>,
    engine: strut_sql_driver_api::EngineKind,
) {
    match body {
        SetExpr::Select(select) => collect_from_select(select, out, engine),
        SetExpr::SetOperation { left, right, .. } => {
            collect_set_expr(left, out, engine);
            collect_set_expr(right, out, engine);
        }
        SetExpr::Query(query) => collect_query(query, out, engine),
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
    let statements = crate::parse_statements_for_engine(sql, engine)?;
    let mut out = Vec::new();
    for statement in &statements {
        collect_statement_tables(statement, &mut out, engine);
    }
    dedup_tables(out)
}

fn collect_statement_tables(
    statement: &Statement,
    out: &mut Vec<TableRef>,
    engine: strut_sql_driver_api::EngineKind,
) {
    fn collect_table_with_joins(
        item: &sqlparser::ast::TableWithJoins,
        out: &mut Vec<TableRef>,
        engine: strut_sql_driver_api::EngineKind,
    ) {
        collect_table_factor(&item.relation, out, engine);
        for join in &item.joins {
            collect_table_factor(&join.relation, out, engine);
        }
    }
    match statement {
        Statement::Query(query) => collect_query(query, out, engine),
        Statement::Insert(insert) => {
            if let sqlparser::ast::TableObject::TableName(name) = &insert.table {
                out.push(table_ref_from_name(name, engine));
            }
            if let Some(source) = &insert.source {
                collect_query(source, out, engine);
            }
        }
        Statement::Update(update) => {
            collect_table_with_joins(&update.table, out, engine);
            if let Some(from) = &update.from {
                let tables = match from {
                    sqlparser::ast::UpdateTableFromKind::BeforeSet(tables)
                    | sqlparser::ast::UpdateTableFromKind::AfterSet(tables) => tables,
                };
                for item in tables {
                    collect_table_with_joins(item, out, engine);
                }
            }
        }
        Statement::Delete(delete) => {
            for name in &delete.tables {
                out.push(table_ref_from_name(name, engine));
            }
            let from_tables = match &delete.from {
                sqlparser::ast::FromTable::WithFromKeyword(tables)
                | sqlparser::ast::FromTable::WithoutKeyword(tables) => tables,
            };
            for item in from_tables {
                collect_table_with_joins(item, out, engine);
            }
            if let Some(using) = &delete.using {
                for item in using {
                    collect_table_with_joins(item, out, engine);
                }
            }
        }
        Statement::Explain { statement, .. } => collect_statement_tables(statement, out, engine),
        Statement::CreateTable(create) => {
            out.push(table_ref_from_name(&create.name, engine));
        }
        Statement::Drop { names, .. } => {
            for name in names {
                out.push(table_ref_from_name(name, engine));
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

/// The database (or attached-database) a name qualification points at, for
/// engines where a qualifier means a database rather than a schema.
fn database_qualifier(engine: strut_sql_driver_api::EngineKind, table: &TableRef) -> Option<&str> {
    use strut_sql_driver_api::EngineKind;
    if !TablePolicy::scopes_databases(engine) || engine == EngineKind::SqlServer {
        return None;
    }
    table.schema.as_deref()
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
        collect_statement_tables(statement, &mut tables, engine);
        tables
    };
    let tables = dedup_tables(tables)?;
    for table in &tables {
        if engine == strut_sql_driver_api::EngineKind::SqlServer {
            if let Some(database) = &table.database {
                // SQL Server's three- and four-part names can resolve to
                // another database (or linked server). Without an explicit
                // database allowlist the name is rejected outright: guessing
                // the connected database would let a qualifier bypass deny
                // rules and row filters.
                if policy.databases.is_empty() {
                    return Err(GuardError::Denied(format!(
                        "table '{database}.{}.{}' is database-qualified; table policy can only \
                         authorize schema.table names on the connected database",
                        table.schema.as_deref().unwrap_or_default(),
                        table.table
                    )));
                }
                if !policy.allows_database(engine, database) {
                    return Err(GuardError::Denied(format!(
                        "database '{database}' is not accessible to this principal"
                    )));
                }
            }
        } else if let Some(database) = database_qualifier(engine, table) {
            if !policy.allows_database(engine, database) {
                return Err(GuardError::Denied(format!(
                    "database '{database}' is not accessible to this principal"
                )));
            }
        }
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
    collect_statement_tables(statement, &mut after, engine);
    for table in dedup_tables(after)? {
        if policy
            .row_filter_for(table.schema.as_deref(), &table.table)
            .is_some()
            && !policy.row_filters.is_empty()
            && !filtered_positions_include(statement, policy, &table, engine)
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
    engine: strut_sql_driver_api::EngineKind,
) -> bool {
    // The rewrite replaces every supported occurrence with a derived table
    // whose subquery already carries the filter; the remaining check only has
    // to prove the table is no longer reachable as a bare FROM target.
    let mut bare = Vec::new();
    collect_bare_tables(statement, &mut bare, engine);
    !bare.iter().any(|t| {
        t == table
            && policy
                .row_filter_for(t.schema.as_deref(), &t.table)
                .is_some()
    })
}

fn collect_bare_tables(
    statement: &Statement,
    out: &mut Vec<TableRef>,
    engine: strut_sql_driver_api::EngineKind,
) {
    // Same traversal as collect_statement_tables but stops at Derived
    // subqueries: a filtered table wrapped in a derived table is no longer a
    // bare reference.
    fn select_bare(
        select: &Select,
        out: &mut Vec<TableRef>,
        engine: strut_sql_driver_api::EngineKind,
    ) {
        for item in &select.from {
            if let TableFactor::Table { name, .. } = &item.relation {
                out.push(table_ref_from_name(name, engine));
            }
            for join in &item.joins {
                if let TableFactor::Table { name, .. } = &join.relation {
                    out.push(table_ref_from_name(name, engine));
                }
            }
        }
    }
    match statement {
        Statement::Query(query) => {
            if let SetExpr::Select(select) = query.body.as_ref() {
                select_bare(select, out, engine);
            }
        }
        Statement::Explain { statement, .. } => collect_bare_tables(statement, out, engine),
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
            let table = table_ref_from_name(name, engine);
            let Some(filter) = policy.row_filter_for(table.schema.as_deref(), &table.table) else {
                return Ok(());
            };
            let predicate_sql = &filter.predicate;
            let dialect_sql = format!("SELECT * FROM {name} WHERE ({predicate_sql})");
            let inner = parse_single_query(&dialect_sql, engine)?;
            let alias = alias.clone().or(Some(TableAlias {
                explicit: false,
                name: if engine == strut_sql_driver_api::EngineKind::SqlServer {
                    name.0
                        .last()
                        .and_then(|part| part.as_ident())
                        .cloned()
                        .unwrap_or_else(|| sqlparser::ast::Ident::new(table.table.clone()))
                } else {
                    sqlparser::ast::Ident::new(table.table.clone())
                },
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
    let statements = crate::parse_statements_for_engine(sql, engine)
        .map_err(|error| GuardError::Denied(format!("row filter does not parse: {error}")))?;
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
