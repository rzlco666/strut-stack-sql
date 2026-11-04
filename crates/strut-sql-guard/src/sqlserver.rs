//! SQL Server row bounds and full-AST safety checks. Public binds stay `?`.

use std::ops::ControlFlow;

use serde_json::Value;
use sqlparser::ast::{
    Expr, Fetch, LimitClause, ObjectName, Offset, OffsetRows, OrderByKind, Query, Select, SetExpr,
    Statement, TableFactor, Top, TopQuantity, Value as SqlValue, Visit, Visitor,
};
use strut_sql_driver_api::EngineKind;

use crate::{GuardError, PrepareOptions, PreparedSql};

fn denied(message: impl Into<String>) -> GuardError {
    GuardError::Denied(message.into())
}

fn number(value: u64) -> Expr {
    Expr::value(SqlValue::Number(value.to_string(), false))
}

fn denied_function(name: &ObjectName) -> Result<(), GuardError> {
    let base = crate::object_name_base_name(name);
    let dangerous = base.starts_with("xp_")
        || base.starts_with("sp_oa")
        || matches!(
            base.as_str(),
            "openrowset"
                | "openquery"
                | "opendatasource"
                | "fn_get_audit_file"
                | "fn_get_audit_file_v2"
                | "fn_xe_file_target_read_file"
                | "fn_xe_telemetry_blob_target_read_file"
                | "fn_trace_gettable"
                | "fn_dblog"
                | "fn_dump_dblog"
                | "fn_db_backup_file_snapshots"
                | "dm_os_enumerate_filesystem"
                | "dm_os_file_exists"
        );
    if dangerous {
        return Err(denied(format!(
            "function '{base}' is not allowed: server file, remote access or external execution is forbidden"
        )));
    }
    Ok(())
}

/// sqlparser's MSSQL tokenizer represents `$12.34` as a placeholder token
/// because T-SQL has no `$N` binds and `$` prefixes money constants. Accept
/// only that literal shape here; every other non-`?` placeholder stays denied.
fn is_money_literal(placeholder: &str) -> bool {
    let Some(value) = placeholder.strip_prefix('$') else {
        return false;
    };
    let (integer, fraction) = match value.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (value, None),
    };
    !integer.is_empty()
        && integer.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.is_none_or(|fraction| fraction.bytes().all(|byte| byte.is_ascii_digit()))
}

fn denied_linked_server(name: &ObjectName) -> Result<(), GuardError> {
    if name.0.len() >= 4 {
        // `server.database.schema.table` addresses a linked server; remote
        // access is forbidden regardless of write mode.
        return Err(denied(
            "linked-server four-part table names are not allowed: remote access is forbidden",
        ));
    }
    Ok(())
}

/// The visitor reaches ORDER BY, TOP, window frames, OUTPUT, MERGE clauses,
/// DML FROM sources and nested TVFs as well as the usual SELECT expressions.
/// Keep this SQL Server-specific: other engines retain their existing rules.
pub(super) fn check_statement(statement: &Statement) -> Result<(), GuardError> {
    struct Safety {
        depth: u32,
        joins: usize,
    }
    impl Visitor for Safety {
        type Break = GuardError;

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            let result = match expr {
                Expr::Function(function) => denied_function(&function.name),
                Expr::Value(value)
                    if matches!(&value.value,
                    SqlValue::Placeholder(placeholder)
                        if placeholder != "?" && !is_money_literal(placeholder)) =>
                {
                    Err(denied("SQL Server public bind placeholders must be ?"))
                }
                _ => Ok(()),
            };
            match result {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(error),
            }
        }

        fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<Self::Break> {
            match denied_linked_server(relation) {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(error),
            }
        }

        fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
            // DELETE's target list and DROP names are ObjectNames that the
            // generic visitor does not route through `visit_relation`.
            let names: &[ObjectName] = match statement {
                Statement::Delete(delete) => &delete.tables,
                Statement::Drop { names, .. } => names,
                _ => &[],
            };
            for name in names {
                if let Err(error) = denied_linked_server(name) {
                    return ControlFlow::Break(error);
                }
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            let result = match factor {
                TableFactor::Table {
                    name,
                    args: Some(_),
                    ..
                }
                | TableFactor::Function { name, .. } => denied_function(name),
                TableFactor::NestedJoin {
                    table_with_joins, ..
                } => {
                    self.joins = self.joins.saturating_add(table_with_joins.joins.len());
                    Ok(())
                }
                _ => Ok(()),
            };
            if self.joins > crate::MAX_JOINS as usize {
                return ControlFlow::Break(denied("query too complex: too many joins"));
            }
            match result {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(error),
            }
        }

        fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Self::Break> {
            self.joins = self
                .joins
                .saturating_add(select.from.len().saturating_sub(1));
            for table in &select.from {
                self.joins = self.joins.saturating_add(table.joins.len());
            }
            if self.joins > crate::MAX_JOINS as usize {
                return ControlFlow::Break(denied("query too complex: too many joins"));
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            self.depth += 1;
            if self.depth > crate::MAX_SUBQUERY_DEPTH + 1 {
                return ControlFlow::Break(denied("query too complex: excessive subquery depth"));
            }
            let top = match query.body.as_ref() {
                SetExpr::Select(select) => select.top.as_ref(),
                _ => None,
            };
            if top.is_some_and(|top| top.with_ties) && query.order_by.is_none() {
                return ControlFlow::Break(denied("SQL Server TOP WITH TIES requires ORDER BY"));
            }
            if let Some(clause) = &query.limit_clause {
                match clause {
                    LimitClause::LimitOffset { limit: None, offset: Some(offset), limit_by }
                        if limit_by.is_empty()
                            && !matches!(offset.rows, OffsetRows::None)
                            && query.order_by.is_some()
                            && top.is_none() => {}
                    _ => return ControlFlow::Break(denied(
                        "SQL Server requires ordered OFFSET ROWS/FETCH; LIMIT and TOP with OFFSET are not supported",
                    )),
                }
            }
            if let Some(fetch) = &query.fetch {
                if query.limit_clause.is_none()
                    || query.order_by.is_none()
                    || top.is_some()
                    || fetch.percent
                    || fetch.with_ties
                    || fetch.quantity.is_none()
                    || fetch.quantity.as_ref().and_then(crate::expr_as_limit) == Some(0)
                {
                    return ControlFlow::Break(denied(
                        "SQL Server FETCH requires ORDER BY OFFSET ROWS and a positive row count, without PERCENT or WITH TIES",
                    ));
                }
            }
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _: &Query) -> ControlFlow<Self::Break> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }
    }
    match statement.visit(&mut Safety { depth: 0, joins: 0 }) {
        ControlFlow::Break(error) => Err(error),
        ControlFlow::Continue(()) => Ok(()),
    }
}

pub(super) fn set_top(query: &mut Query, count: u64) -> Result<(), GuardError> {
    let SetExpr::Select(select) = query.body.as_mut() else {
        return Err(denied("TOP requires a single SELECT body"));
    };
    select.top = Some(Top {
        with_ties: false,
        percent: false,
        quantity: Some(TopQuantity::Expr(number(count))),
    });
    Ok(())
}

pub(super) fn keyset_order_expr(query: &Query) -> Result<Expr, GuardError> {
    match query.order_by.as_ref().map(|order| &order.kind) {
        Some(OrderByKind::Expressions(expressions)) if expressions.len() == 1 => {
            Ok(expressions[0].expr.clone())
        }
        _ => Err(denied("cursor pagination requires one ordering column")),
    }
}

fn top_of(query: &Query) -> Option<&Top> {
    match query.body.as_ref() {
        SetExpr::Select(select) => select.top.as_ref(),
        _ => None,
    }
}

fn top_quantity(top: &Top) -> Option<u64> {
    match &top.quantity {
        Some(TopQuantity::Constant(value)) => Some(*value),
        Some(TopQuantity::Expr(expr)) => crate::expr_as_limit(expr),
        None => None,
    }
}

fn unverified_bound(query: &Query) -> bool {
    top_of(query).is_some_and(|top| top.percent || top.with_ties || top_quantity(top).is_none())
        || query.fetch.as_ref().is_some_and(|fetch| {
            fetch
                .quantity
                .as_ref()
                .and_then(crate::expr_as_limit)
                .is_none()
        })
}

fn set_fetch(query: &mut Query, count: u64) -> Result<(), GuardError> {
    if count == 0 {
        return Err(denied("SQL Server FETCH row bound must be positive"));
    }
    query.fetch = Some(Fetch {
        with_ties: false,
        percent: false,
        quantity: Some(number(count)),
    });
    Ok(())
}

fn set_offset_fetch(query: &mut Query, offset: u64, count: u64) -> Result<(), GuardError> {
    if query.order_by.is_none() {
        return Err(denied(
            "SQL Server OFFSET/FETCH requires an explicit ORDER BY",
        ));
    }
    query.limit_clause = Some(LimitClause::LimitOffset {
        limit: None,
        offset: Some(Offset {
            value: number(offset),
            rows: OffsetRows::Rows,
        }),
        limit_by: vec![],
    });
    set_fetch(query, count)
}

pub(super) fn prepare(
    mut statement: Statement,
    class: crate::StmtClass,
    params: &[Value],
    max_rows: u32,
    options: &PrepareOptions,
    source_sql: &str,
) -> Result<PreparedSql, GuardError> {
    let wants_page = options.page_offset > 0 || options.page_size.is_some();
    let page_size = options
        .page_size
        .unwrap_or(max_rows as usize)
        .min(max_rows as usize) as u32;
    let mut injected = false;
    let mut clamped = false;
    let mut unverified = false;
    let mut server_pagination = false;

    if class.is_select_like() {
        let Statement::Query(query) = &mut statement else {
            return Err(denied("SQL Server row bounds require a SELECT query"));
        };
        unverified = unverified_bound(query);
        let caller_bound = top_of(query).is_some() || query.fetch.is_some();
        if wants_page {
            // TOP (including expressions, PERCENT and WITH TIES) and existing
            // OFFSET/FETCH define the caller's result set. Do not discard or
            // reorder that set, or combine TOP and OFFSET in one SELECT.
            // The shared executor slices the original result client-side.
            if !caller_bound && query.limit_clause.is_some() {
                let count = (options.page_offset as u64)
                    .checked_add(u64::from(page_size) + 1)
                    .ok_or_else(|| denied("pagination row bound overflow"))?;
                set_fetch(query, count)?;
                injected = true;
            }
            if !caller_bound && query.limit_clause.is_none() {
                let probe = u64::from(page_size) + 1;
                if query.order_by.is_some() {
                    set_offset_fetch(query, options.page_offset as u64, probe)?;
                    server_pagination = true;
                    injected = true;
                } else if matches!(query.body.as_ref(), SetExpr::Select(_)) {
                    // An unordered SELECT has no order semantics to extend.
                    // Bound the prefix with TOP and let the executor skip it;
                    // this also works for non-sortable result column types.
                    let count = (options.page_offset as u64)
                        .checked_add(probe)
                        .ok_or_else(|| denied("pagination row bound overflow"))?;
                    set_top(query, count)?;
                    injected = true;
                } else {
                    // A set operation has no safe synthetic ordering:
                    // ORDER BY 1 can reject non-sortable result columns (for
                    // example a Showplan XML column) and would add ordering
                    // the caller never asked for. Leave the SQL untouched;
                    // the executor takes only the requested prefix and
                    // reports its hard cap in the result metadata.
                    unverified = true;
                }
            }
        } else if let Some(top) = top_of(query) {
            if !top.percent
                && !top.with_ties
                && top_quantity(top).is_some_and(|n| n > u64::from(max_rows))
            {
                set_top(query, u64::from(max_rows))?;
                clamped = true;
            }
        } else if let Some(fetch) = &query.fetch {
            if fetch
                .quantity
                .as_ref()
                .and_then(crate::expr_as_limit)
                .is_some_and(|n| n > u64::from(max_rows))
            {
                set_fetch(query, u64::from(max_rows))?;
                clamped = true;
            }
        } else {
            if query.limit_clause.is_some() {
                // Preserve a caller's existing ordered OFFSET.
                set_fetch(query, u64::from(max_rows))?;
                injected = true;
            } else if matches!(query.body.as_ref(), SetExpr::Select(_)) {
                set_top(query, u64::from(max_rows))?;
                injected = true;
            } else if query.order_by.is_some() {
                // An ordered set query has a caller-defined ordering, so
                // OFFSET/FETCH bounds it without inventing semantics.
                set_offset_fetch(query, 0, u64::from(max_rows))?;
                injected = true;
            } else {
                // Same rule as paged set operations: no synthetic ORDER BY,
                // so leave the SQL untouched and let the executor enforce the
                // client-side hard cap.
                unverified = true;
            }
        }
    } else if wants_page {
        return Err(denied(
            "page_offset/page_size only supported on SELECT queries",
        ));
    }

    // SELECT row bounds are expressed by mutating and re-rendering the AST.
    // Every other class is executed from the caller's original T-SQL: the
    // parser may have accepted a dialect-adapted statement (see
    // `parse_statements_for_engine`) that sqlparser would render back as
    // non-TSQL, and non-select statements are never mutated by the guard.
    let mut sql = if class.is_select_like() {
        statement.to_string()
    } else {
        crate::strip_trailing_semicolons(source_sql).to_string()
    };
    // A new line keeps a trailing -- comment from swallowing MERGE's terminator.
    if matches!(statement, Statement::Merge(_)) {
        sql.push_str("\n;");
    }
    crate::validate_param_count(&sql, params, EngineKind::SqlServer)?;
    Ok(PreparedSql {
        sql,
        params: params.to_vec(),
        limit_injected: injected,
        limit_clamped: clamped,
        limit_unverified: unverified,
        class,
        server_pagination,
        page_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        validate_and_prepare, validate_and_prepare_keyset, validate_and_prepare_with_options,
        KeysetRequest, RowFilter, SqlClass, TablePolicy, TableRule, WriteMode,
    };

    fn prepare(sql: &str, params: &[Value]) -> PreparedSql {
        validate_and_prepare(sql, params, EngineKind::SqlServer, WriteMode::ReadOnly, 10).unwrap()
    }

    fn page(sql: &str, params: &[Value], offset: usize) -> PreparedSql {
        validate_and_prepare_with_options(
            sql,
            params,
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            10,
            PrepareOptions {
                page_offset: offset,
                page_size: Some(2),
                table_policy: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn top_injection_clamping_and_unverified_semantics() {
        let injected = prepare("SELECT id FROM dbo.t ORDER BY id DESC", &[]);
        assert_eq!(
            injected.sql,
            "SELECT TOP (10) id FROM dbo.t ORDER BY id DESC"
        );
        assert!(injected.limit_injected);
        for input in ["TOP 200", "TOP (200)"] {
            let result = prepare(&format!("SELECT {input} id FROM dbo.t ORDER BY id"), &[]);
            assert!(result.limit_clamped);
            assert!(result.sql.contains("TOP (10)"));
            assert!(!result.sql.contains("LIMIT"));
        }
        for input in ["TOP (2 + 3)", "TOP (50) PERCENT", "TOP (2) WITH TIES"] {
            let result = prepare(&format!("SELECT {input} id FROM dbo.t ORDER BY id"), &[]);
            assert!(result.limit_unverified, "{input}");
            assert!(!result.limit_clamped && !result.limit_injected);
            assert!(result.sql.contains(input), "{}", result.sql);
        }
        let bound = prepare(
            "SELECT TOP (?) id FROM dbo.t ORDER BY id",
            &[Value::from(50)],
        );
        assert!(bound.limit_unverified);
        assert_eq!(bound.params, [Value::from(50)]);
        assert!(bound.sql.contains("TOP (?)"));
        assert!(!prepare("SELECT TOP 3 id FROM dbo.t", &[]).limit_clamped);
    }

    #[test]
    fn offset_fetch_and_set_queries_never_emit_limit() {
        let result = page("SELECT id FROM dbo.t ORDER BY id DESC", &[], 4);
        assert_eq!(
            result.sql,
            "SELECT id FROM dbo.t ORDER BY id DESC OFFSET 4 ROWS FETCH FIRST 3 ROWS ONLY"
        );
        assert!(result.server_pagination);
        let unordered = page("SELECT id FROM dbo.t", &[], 4);
        assert!(unordered.sql.starts_with("SELECT TOP (7)"));
        assert!(!unordered.server_pagination);
        let set = prepare("SELECT 1 AS id UNION ALL SELECT 2 AS id", &[]);
        // A set operation without ORDER BY has no safe synthetic ordering:
        // ORDER BY 1 can reject non-sortable columns (Showplan XML) and would
        // add semantics. Leave the SQL untouched and cap client-side.
        assert_eq!(set.sql, "SELECT 1 AS id UNION ALL SELECT 2 AS id");
        assert!(set.limit_unverified && !set.limit_injected);
        assert!(!set.sql.contains("ORDER BY") && !set.sql.contains("FETCH"));
        let set_page = page(
            "SELECT 1 AS id UNION ALL SELECT 2 AS id ORDER BY id DESC",
            &[],
            1,
        );
        assert!(set_page
            .sql
            .ends_with("ORDER BY id DESC OFFSET 1 ROWS FETCH FIRST 3 ROWS ONLY"));
        assert!(set_page.server_pagination);
        for result in [result, unordered, set, set_page] {
            assert!(!result.sql.contains("LIMIT"), "{}", result.sql);
            crate::parse_single_statement(&result.sql, EngineKind::SqlServer).unwrap();
        }
    }

    #[test]
    fn caller_top_and_offset_semantics_survive_pagination() {
        for sql in [
            "SELECT TOP (2) id FROM dbo.t ORDER BY id DESC",
            "SELECT TOP (?) WITH TIES id FROM dbo.t ORDER BY id DESC",
            "SELECT TOP (50) PERCENT id FROM dbo.t ORDER BY id DESC",
            "SELECT id FROM dbo.t ORDER BY id DESC OFFSET 2 ROWS FETCH NEXT 4 ROWS ONLY",
        ] {
            let params = if sql.contains('?') {
                vec![Value::from(2)]
            } else {
                vec![]
            };
            let result = page(sql, &params, 1);
            let original = crate::parse_single_statement(sql, EngineKind::SqlServer).unwrap();
            assert_eq!(result.sql, original.to_string());
            assert!(!result.server_pagination && !result.limit_clamped);
            assert_eq!(result.params, params);
        }
        let fetched = prepare(
            "SELECT id FROM dbo.t ORDER BY id OFFSET 2 ROWS FETCH NEXT 20 ROWS ONLY",
            &[],
        );
        assert!(fetched.limit_clamped);
        assert!(fetched
            .sql
            .ends_with("OFFSET 2 ROWS FETCH FIRST 10 ROWS ONLY"));
        let offset = prepare("SELECT id FROM dbo.t ORDER BY id OFFSET 2 ROWS", &[]);
        assert!(offset
            .sql
            .ends_with("OFFSET 2 ROWS FETCH FIRST 10 ROWS ONLY"));
        let offset_page = page(
            "SELECT id FROM dbo.t ORDER BY id DESC OFFSET 2 ROWS",
            &[],
            1,
        );
        assert!(offset_page
            .sql
            .ends_with("OFFSET 2 ROWS FETCH FIRST 4 ROWS ONLY"));
        assert!(!offset_page.server_pagination);
    }

    #[test]
    fn invalid_cross_dialect_bounds_are_denied() {
        for sql in [
            "SELECT id FROM t LIMIT 2",
            "SELECT id FROM t OFFSET 2 ROWS",
            "SELECT TOP (2) id FROM t ORDER BY id OFFSET 1 ROWS",
            "SELECT TOP (2) WITH TIES id FROM t",
            "SELECT id FROM t FETCH FIRST 2 ROWS ONLY",
        ] {
            assert!(
                validate_and_prepare(sql, &[], EngineKind::SqlServer, WriteMode::AllowDdl, 10)
                    .is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn oversized_pages_use_the_configured_ceiling() {
        let result = validate_and_prepare_with_options(
            "SELECT id FROM dbo.t ORDER BY id",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            10,
            PrepareOptions {
                page_offset: 4,
                page_size: Some(usize::MAX),
                table_policy: None,
            },
        )
        .unwrap();
        assert_eq!(result.page_size, 10);
        assert!(result
            .sql
            .ends_with("OFFSET 4 ROWS FETCH FIRST 11 ROWS ONLY"));
        let bounded = validate_and_prepare_with_options(
            "SELECT TOP (20) id FROM dbo.t ORDER BY id",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            10,
            PrepareOptions {
                page_offset: 4,
                page_size: Some(usize::MAX),
                table_policy: None,
            },
        )
        .unwrap();
        assert_eq!(bounded.page_size, 10);
        assert!(bounded.sql.contains("TOP (20)"));
        assert!(!bounded.server_pagination);
    }

    #[test]
    fn escaped_bracket_identifiers_remain_valid_after_rewrites() {
        let result = prepare(
            "SELECT [odd?]]name] FROM dbo.t WHERE id = ?",
            &[Value::from(1)],
        );
        assert!(result.sql.contains("\"odd?]name\""), "{}", result.sql);
        assert_eq!(
            crate::placeholder_count(&result.sql, EngineKind::SqlServer),
            1
        );
        assert!(result.sql.contains("TOP (10)"));
    }

    #[test]
    fn fetch_and_set_query_parameters_keep_their_positions() {
        let params = [Value::from(2), Value::from(50)];
        let fetched = prepare(
            "SELECT id FROM dbo.t ORDER BY id OFFSET ? ROWS FETCH NEXT ? ROWS ONLY",
            &params,
        );
        assert!(fetched.limit_unverified);
        assert_eq!(fetched.params, params);
        assert!(fetched
            .sql
            .ends_with("OFFSET ? ROWS FETCH FIRST ? ROWS ONLY"));
        let set = page(
            "SELECT ? AS id UNION ALL SELECT ? AS id ORDER BY id",
            &params,
            1,
        );
        assert_eq!(set.params, params);
        assert!(set
            .sql
            .ends_with("ORDER BY id OFFSET 1 ROWS FETCH FIRST 3 ROWS ONLY"));
    }

    #[test]
    fn cursor_pages_bound_with_top_and_reject_caller_top() {
        let request = KeysetRequest {
            after: None,
            page_size: 2,
            table_policy: None,
        };
        let (first, _) = validate_and_prepare_keyset(
            "SELECT id FROM dbo.t ORDER BY id",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            &request,
        )
        .unwrap();
        assert_eq!(first.sql, "SELECT TOP (3) id FROM dbo.t ORDER BY id");
        assert!(validate_and_prepare_keyset(
            "SELECT TOP (2) id FROM dbo.t ORDER BY id",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            &request
        )
        .is_err());
    }

    #[test]
    fn complexity_bounds_cover_order_by_subqueries() {
        let error = validate_and_prepare(
            "SELECT id FROM dbo.t ORDER BY (SELECT (SELECT (SELECT (SELECT (SELECT (SELECT 1))))))",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            10,
        )
        .unwrap_err();
        assert!(error.to_string().contains("subquery depth"), "{error}");
    }

    fn tenant_policy() -> TablePolicy {
        TablePolicy {
            allow: vec![TableRule {
                schema: Some("dbo".into()),
                table: "t".into(),
            }],
            row_filters: vec![RowFilter {
                schema: Some("dbo".into()),
                table: "t".into(),
                predicate: "tenant = 7".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn table_policy_and_params_survive_offset_and_keyset_rewrites() {
        let policy = tenant_policy();
        let offset = validate_and_prepare_with_options(
            "SELECT p.id FROM dbo.t p WHERE p.tenant = ? ORDER BY p.id",
            &[Value::from(9)],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            10,
            PrepareOptions {
                page_offset: 1,
                page_size: Some(1),
                table_policy: Some(policy.clone()),
            },
        )
        .unwrap();
        assert!(
            offset
                .sql
                .contains("FROM (SELECT * FROM dbo.t WHERE (tenant = 7))"),
            "{}",
            offset.sql
        );
        assert!(offset.sql.contains("p.tenant = ?"));
        assert!(offset
            .sql
            .ends_with("ORDER BY p.id OFFSET 1 ROWS FETCH FIRST 2 ROWS ONLY"));
        assert_eq!(offset.params, [Value::from(9)]);
        let (cursor, shape) = validate_and_prepare_keyset(
            "SELECT p.[id] FROM [dbo].[t] p WHERE p.tenant = ? ORDER BY p.[id] DESC",
            &[Value::from(7)],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            &KeysetRequest {
                after: Some(Value::from(8)),
                page_size: 2,
                table_policy: Some(policy),
            },
        )
        .unwrap();
        assert_eq!(shape.table_qualified, "dbo.t");
        assert!(cursor.sql.contains("TOP (3)"));
        assert!(cursor.sql.contains("tenant = 7"));
        assert!(cursor.sql.contains("p.[id] < ?"), "{}", cursor.sql);
        assert_eq!(cursor.params, [Value::from(7), Value::from(8)]);
        assert!(!cursor.sql.contains("LIMIT"));
        assert!(validate_and_prepare_with_options(
            "SELECT * FROM dbo.secret",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            10,
            PrepareOptions {
                table_policy: Some(tenant_policy()),
                ..Default::default()
            }
        )
        .is_err());
    }

    #[test]
    fn unsafe_functions_are_denied_throughout_ast_in_every_mode() {
        for mode in [
            WriteMode::ReadOnly,
            WriteMode::AllowWrites,
            WriteMode::AllowDdl,
        ] {
            for sql in [
                "SELECT [sys].[fn_get_audit_file]('x', DEFAULT, DEFAULT)",
                "SELECT * FROM sys.fn_xe_file_target_read_file('x', NULL, NULL, NULL)",
                "SELECT id FROM t ORDER BY sys.fn_dblog(NULL, NULL)",
                "SELECT TOP (sys.fn_trace_gettable('x', 1)) id FROM t",
                "SELECT ROW_NUMBER() OVER (ORDER BY sys.fn_dblog(NULL, NULL)) FROM t",
                "SELECT * FROM OPENQUERY(remote, 'SELECT 1')",
                "SELECT * FROM t CROSS APPLY sys.fn_get_audit_file('x', DEFAULT, DEFAULT) a",
                "UPDATE t SET id = sys.fn_trace_gettable('x', 1)",
                "DELETE FROM t WHERE id IN (SELECT id FROM sys.fn_dblog(NULL, NULL))",
                "SELECT xp_cmdshell('x')",
            ] {
                let error =
                    validate_and_prepare(sql, &[], EngineKind::SqlServer, mode, 10).unwrap_err();
                assert!(
                    error.to_string().contains("function '")
                        && error.to_string().contains("not allowed"),
                    "{sql}: {error}"
                );
            }
        }
    }

    #[test]
    fn write_gates_apply_to_dml_ddl_and_select_into() {
        for sql in [
            "INSERT INTO dbo.t (id) VALUES (1)",
            "UPDATE dbo.t SET id = 2",
            "DELETE FROM dbo.t",
        ] {
            assert!(
                validate_and_prepare(sql, &[], EngineKind::SqlServer, WriteMode::ReadOnly, 10)
                    .is_err()
            );
            let write =
                validate_and_prepare(sql, &[], EngineKind::SqlServer, WriteMode::AllowWrites, 10)
                    .unwrap();
            assert_eq!(write.class.sql_class(), SqlClass::Dml);
        }
        assert!(validate_and_prepare(
            "SELECT id INTO dbo.copy FROM dbo.t",
            &[],
            EngineKind::SqlServer,
            WriteMode::AllowWrites,
            10
        )
        .is_err());
        let into = validate_and_prepare(
            "SELECT id INTO dbo.copy FROM dbo.t",
            &[],
            EngineKind::SqlServer,
            WriteMode::AllowDdl,
            10,
        )
        .unwrap();
        assert_eq!(into.class.sql_class(), SqlClass::Ddl);
        assert!(validate_and_prepare(
            "DROP TABLE dbo.t",
            &[],
            EngineKind::SqlServer,
            WriteMode::AllowWrites,
            10
        )
        .is_err());
        validate_and_prepare(
            "DROP TABLE dbo.t",
            &[],
            EngineKind::SqlServer,
            WriteMode::AllowDdl,
            10,
        )
        .unwrap();
        prepare("SELECT COUNT(*), COALESCE(MAX(id), 0) FROM dbo.t", &[]);
    }

    #[test]
    fn filesystem_tvfs_and_linked_servers_are_denied() {
        for sql in [
            "SELECT * FROM sys.dm_os_enumerate_filesystem('C:\\', '*')",
            "SELECT * FROM sys.dm_os_file_exists('\\\\remote\\share\\file')",
            "SELECT sys.dm_os_file_exists('C:\\file') AS file_exists",
        ] {
            let error =
                validate_and_prepare(sql, &[], EngineKind::SqlServer, WriteMode::ReadOnly, 10)
                    .unwrap_err();
            assert!(error.to_string().contains("not allowed"), "{sql}: {error}");
        }
        for sql in [
            "SELECT * FROM [remote].[app].[dbo].[customers]",
            "SELECT c.id FROM [srv].[db].[dbo].[customers] c",
            "UPDATE [remote].[app].[dbo].[customers] SET id = 1",
            "INSERT INTO [remote].[app].[dbo].[customers] (id) VALUES (1)",
            "DELETE FROM [remote].[app].[dbo].[customers]",
        ] {
            let error =
                validate_and_prepare(sql, &[], EngineKind::SqlServer, WriteMode::ReadOnly, 10)
                    .unwrap_err();
            assert!(
                error.to_string().contains("linked-server"),
                "{sql}: {error}"
            );
        }
    }

    fn dotted_bracket_policy() -> TablePolicy {
        TablePolicy {
            deny: vec![TableRule {
                schema: Some("dbo".into()),
                table: "r3.ledger]raw".into(),
            }],
            row_filters: vec![RowFilter {
                schema: Some("dbo".into()),
                table: "r3.ledger]raw".into(),
                predicate: "tenant = 7".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn policy_matches_quoted_dotted_bracket_table_names() {
        // Two-part name: the escaped bracket identifier must normalize to
        // schema `dbo` + table `r3.ledger]raw`, not schema `dbo.r3` + table
        // `ledger]raw`, or the deny rule would be bypassed.
        for sql in [
            "SELECT * FROM dbo.[r3.ledger]]raw]",
            "SELECT * FROM [dbo].[r3.ledger]]raw]",
        ] {
            let error = validate_and_prepare_with_options(
                sql,
                &[],
                EngineKind::SqlServer,
                WriteMode::ReadOnly,
                10,
                PrepareOptions {
                    table_policy: Some(dotted_bracket_policy()),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("not accessible"),
                "{sql}: {error}"
            );
        }
        // Three-part name: the policy model cannot name a database, so the
        // guard refuses instead of guessing the connected database identity.
        let error = validate_and_prepare_with_options(
            "SELECT * FROM [sql_r3_02].[dbo].[r3.ledger]]raw]",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            10,
            PrepareOptions {
                table_policy: Some(dotted_bracket_policy()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("database-qualified"), "{error}");
    }

    #[test]
    fn row_filter_wraps_quoted_dotted_bracket_table() {
        let result = validate_and_prepare_with_options(
            "SELECT id FROM dbo.[r3.ledger]]raw]",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            10,
            PrepareOptions {
                table_policy: Some(TablePolicy {
                    row_filters: vec![RowFilter {
                        schema: Some("dbo".into()),
                        table: "r3.ledger]raw".into(),
                        predicate: "tenant = 7".into(),
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            result
                .sql
                .contains("FROM (SELECT * FROM dbo.\"r3.ledger]raw\" WHERE (tenant = 7))"),
            "{}",
            result.sql
        );
        assert!(result.sql.contains(") \"r3.ledger]raw\""), "{}", result.sql);
        assert!(result.sql.contains("TOP (10)"), "{}", result.sql);
    }

    #[test]
    fn unordered_set_queries_keep_semantics_and_client_cap() {
        let set = prepare("SELECT 1 AS id UNION ALL SELECT 2 AS id", &[]);
        assert_eq!(set.sql, "SELECT 1 AS id UNION ALL SELECT 2 AS id");
        assert!(set.limit_unverified);
        assert!(!set.limit_injected && !set.server_pagination);
        assert!(!set.sql.contains("ORDER BY") && !set.sql.contains("FETCH"));
        // A Showplan-style XML projection has no sortable first column; the
        // guard must not synthesize ORDER BY 1 for it.
        let plan = prepare(
            "SELECT CONVERT(xml, '<ShowPlanXML/>') AS plan UNION ALL \
             SELECT CONVERT(xml, '<ShowPlanXML/>') AS plan",
            &[],
        );
        assert!(!plan.sql.contains("ORDER BY"), "{}", plan.sql);
        assert!(plan.limit_unverified);
        let paged = page("SELECT 1 AS id UNION ALL SELECT 2 AS id", &[], 1);
        assert_eq!(paged.sql, "SELECT 1 AS id UNION ALL SELECT 2 AS id");
        assert!(!paged.server_pagination && paged.limit_unverified);
    }

    #[test]
    fn money_literals_are_not_bind_placeholders() {
        for sql in ["SELECT $12.34 AS amount", "SELECT $12 AS amount"] {
            let result = prepare(sql, &[]);
            assert!(result.sql.contains("$12"), "{}", result.sql);
            assert!(result.limit_injected);
            assert!(result.params.is_empty());
        }
        let error = validate_and_prepare(
            "SELECT $not_money AS amount",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            10,
        )
        .unwrap_err();
        assert!(error.to_string().contains("must be ?"), "{error}");
    }

    #[test]
    fn keyset_rejects_sqlserver_database_qualified_tables() {
        let request = KeysetRequest {
            after: None,
            page_size: 2,
            table_policy: None,
        };
        let error = validate_and_prepare_keyset(
            "SELECT id FROM [db].[dbo].[t] ORDER BY id",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            &request,
        )
        .unwrap_err();
        assert!(error.to_string().contains("database qualifier"), "{error}");
        // Four-part linked-server names are denied even earlier.
        let error = validate_and_prepare_keyset(
            "SELECT id FROM [srv].[db].[dbo].[t] ORDER BY id",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            &request,
        )
        .unwrap_err();
        assert!(error.to_string().contains("linked-server"), "{error}");
        // The supported two-part shape keeps its exact AST table value.
        let (prepared, shape) = validate_and_prepare_keyset(
            "SELECT id FROM [dbo].[t] ORDER BY id",
            &[],
            EngineKind::SqlServer,
            WriteMode::ReadOnly,
            &request,
        )
        .unwrap();
        assert_eq!(shape.table, "t");
        assert_eq!(shape.table_qualified, "dbo.t");
        assert!(prepared.sql.contains("TOP (3)"));
    }

    #[test]
    fn tsql_alter_column_parses_and_keeps_source_sql() {
        for sql in [
            "ALTER TABLE [dbo].[t] ALTER COLUMN [c] BIGINT NOT NULL",
            "ALTER TABLE [dbo].[t] ALTER COLUMN [c] BIGINT NULL",
            "ALTER TABLE [dbo].[t] ALTER COLUMN [c] NVARCHAR(MAX) NULL",
            "ALTER TABLE [s]]x].[t]]x] ALTER COLUMN [c]]x] NVARCHAR(MAX) NOT NULL",
            "ALTER TABLE dbo.t ALTER COLUMN c BIGINT",
        ] {
            let result =
                validate_and_prepare(sql, &[], EngineKind::SqlServer, WriteMode::AllowDdl, 10)
                    .unwrap();
            assert_eq!(result.sql, sql);
            assert_eq!(result.class.sql_class(), SqlClass::Ddl);
            assert!(result.sql.contains("ALTER COLUMN"), "{}", result.sql);
            assert!(!result.sql.contains("MODIFY"), "{}", result.sql);
            assert!(!result.sql.contains("SET DATA TYPE"), "{}", result.sql);
        }
        // A non-empty table policy must not switch DDL execution to the
        // parser's synthetic rendering.
        let policy = TablePolicy {
            allow: vec![TableRule {
                schema: Some("dbo".into()),
                table: "t".into(),
            }],
            ..Default::default()
        };
        let policed = validate_and_prepare_with_options(
            "ALTER TABLE dbo.t ALTER COLUMN c BIGINT NOT NULL",
            &[],
            EngineKind::SqlServer,
            WriteMode::AllowDdl,
            10,
            PrepareOptions {
                table_policy: Some(policy),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            policed.sql,
            "ALTER TABLE dbo.t ALTER COLUMN c BIGINT NOT NULL"
        );
        assert_eq!(policed.class.sql_class(), SqlClass::Ddl);
    }

    #[test]
    fn tsql_alter_column_adapter_keeps_isolation_and_existing_grammar() {
        let error = validate_and_prepare(
            "ALTER TABLE t ALTER COLUMN c INT; DROP TABLE x",
            &[],
            EngineKind::SqlServer,
            WriteMode::AllowDdl,
            10,
        )
        .unwrap_err();
        assert!(error.to_string().contains("multiple statements"), "{error}");
        let literal = prepare("SELECT 'ALTER TABLE t ALTER COLUMN c INT' AS s", &[]);
        assert!(
            literal.sql.contains("ALTER COLUMN c INT"),
            "{}",
            literal.sql
        );
        for sql in [
            "ALTER TABLE t ALTER COLUMN c SET NOT NULL",
            "ALTER TABLE t ALTER COLUMN c DROP NOT NULL",
            "ALTER TABLE t ADD [c] INT",
            "ALTER TABLE t DROP COLUMN [c]",
        ] {
            validate_and_prepare(sql, &[], EngineKind::SqlServer, WriteMode::AllowDdl, 10).unwrap();
        }
    }
}
