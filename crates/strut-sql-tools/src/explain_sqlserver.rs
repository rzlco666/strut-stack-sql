//! SQL Server Showplan XML normalization. The driver owns session SET state;
//! this module only sees plan XML, never the rows produced by an analyzed query.

use roxmltree::Node;
use strut_sql_driver_api::SqlValue;

use super::{ExplainError, ExplainSummary, PlanNode};

const SHOWPLAN_NS: &str = "http://schemas.microsoft.com/sqlserver/2004/07/showplan";

fn named(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == name
        && node.tag_name().namespace() == Some(SHOWPLAN_NS)
}

fn number(node: Node<'_, '_>, name: &str) -> Option<f64> {
    node.attribute(name)
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
}

fn rows(value: Option<f64>) -> Option<i64> {
    value.map(|v| v.round() as i64)
}

fn identifier(value: &str) -> String {
    value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value)
        .replace("]]", "]")
}

/// A parent operator must not inherit a child's Object, predicate or counters.
fn belongs_to(node: Node<'_, '_>, operator: Node<'_, '_>) -> bool {
    node.ancestors().skip(1).find(|n| named(*n, "RelOp")) == Some(operator)
}

pub(super) fn normalize(
    query: &str,
    result_rows: &[Vec<SqlValue>],
) -> Result<ExplainSummary, ExplainError> {
    let mut nodes = Vec::new();
    let mut total_cost = None;
    let mut plan_rows = None;
    for row in result_rows {
        let Some(SqlValue::String(xml)) = row.first() else {
            return Err(ExplainError::Other(
                "invalid SQL Server execution plan value".into(),
            ));
        };
        // roxmltree does not expand external entities; DTDs are disabled by default.
        let document = roxmltree::Document::parse(xml)
            .map_err(|_| ExplainError::Other("invalid SQL Server Showplan XML".into()))?;
        if !named(document.root_element(), "ShowPlanXML") {
            return Err(ExplainError::Other(
                "invalid SQL Server Showplan XML root".into(),
            ));
        }
        for operator in document.descendants().filter(|n| named(*n, "RelOp")) {
            let depth = operator
                .ancestors()
                .skip(1)
                .filter(|n| named(*n, "RelOp"))
                .count() as u32;
            let cost = number(operator, "EstimatedTotalSubtreeCost");
            let estimated_rows = rows(number(operator, "EstimateRows"));
            if depth == 0 {
                if let Some(cost) = cost {
                    total_cost = Some(total_cost.unwrap_or(0.0) + cost);
                }
                plan_rows = match (plan_rows, estimated_rows) {
                    (Some(previous), Some(current)) => Some(i64::saturating_add(previous, current)),
                    (None, current) => current,
                    (previous, None) => previous,
                };
            }
            let mut relation = None;
            let mut filter = None;
            let mut actual_rows = None;
            let mut actual_time_ms: Option<f64> = None;
            let mut spill = None;
            for detail in operator.descendants().filter(|n| belongs_to(*n, operator)) {
                if relation.is_none() && named(detail, "Object") {
                    relation = detail.attribute("Table").map(identifier);
                }
                if filter.is_none()
                    && named(detail, "ScalarOperator")
                    && detail
                        .ancestors()
                        .any(|a| named(a, "Predicate") || named(a, "SeekPredicates"))
                {
                    filter = detail.attribute("ScalarString").map(str::to_string);
                }
                if named(detail, "RunTimeCountersPerThread") {
                    if let Some(count) = rows(number(detail, "ActualRows")) {
                        actual_rows = Some(i64::saturating_add(actual_rows.unwrap_or(0), count));
                    }
                    if let Some(elapsed) = number(detail, "ActualElapsedms") {
                        // Parallel thread times overlap; do not add their wall times.
                        actual_time_ms = Some(actual_time_ms.unwrap_or(0.0).max(elapsed));
                    }
                }
                if named(detail, "SpillToTempDb") || named(detail, "SpillOccurred") {
                    spill = Some("tempdb spill".to_string());
                }
            }
            let logical = operator.attribute("LogicalOp").unwrap_or("");
            nodes.push(PlanNode {
                node_type: operator
                    .attribute("PhysicalOp")
                    .unwrap_or("Unknown")
                    .to_string(),
                depth,
                relation,
                cost,
                rows: estimated_rows,
                actual_rows,
                actual_time_ms,
                filter,
                join_type: logical
                    .to_ascii_uppercase()
                    .contains("JOIN")
                    .then(|| logical.to_string()),
                spill,
            });
        }
    }
    if nodes.is_empty() {
        return Err(ExplainError::Other(
            "SQL Server execution plan contains no operators".into(),
        ));
    }
    Ok(ExplainSummary {
        engine: "sqlserver".into(),
        query: query.into(),
        analyze: false,
        total_cost,
        plan_rows,
        nodes,
        issues: Vec::new(),
        index_suggestions: Vec::new(),
        warnings: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use strut_sql_driver_api::{
        BoundQuery, ConnectOptions, DriverError, EngineDriver, EngineKind, ForeignKeyInfo,
        IndexInfo, QueryMeta, QueryResult, RoutineInfo, SchemaObject, TableInfo, TableRowCount,
    };
    use strut_sql_guard::{RowFilter, TablePolicy, TableRule, WriteMode};

    use super::super::{analyze_query_with_options, AnalyzeOptions};

    #[derive(Default)]
    struct PlanDriver {
        calls: Mutex<Vec<(BoundQuery, bool)>>,
    }

    #[async_trait::async_trait]
    impl EngineDriver for PlanDriver {
        fn kind(&self) -> EngineKind {
            EngineKind::SqlServer
        }

        async fn from_connection_string_with_options(
            _: &str,
            _: &ConnectOptions,
        ) -> Result<Box<Self>, DriverError> {
            unreachable!("fixture does not connect")
        }

        async fn execute(&self, _: &BoundQuery) -> Result<QueryResult, DriverError> {
            panic!("plans must not use ordinary pooled execution")
        }

        async fn explain(
            &self,
            query: &BoundQuery,
            analyze: bool,
        ) -> Result<QueryResult, DriverError> {
            self.calls.lock().unwrap().push((query.clone(), analyze));
            Ok(QueryResult {
                cols: Vec::new(),
                rows: plan(
                    r#"<RelOp PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="0.1"><IndexScan><Object Table="[allowed]"/></IndexScan></RelOp>"#,
                ),
                meta: QueryMeta {
                    row_count: 1,
                    truncated: false,
                    rows_affected: None,
                },
            })
        }

        async fn list_schemas(&self) -> Result<Vec<SchemaObject>, DriverError> {
            Ok(Vec::new())
        }
        async fn list_tables(&self, _: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
            Ok(Vec::new())
        }
        async fn list_columns(&self, _: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
            Ok(Vec::new())
        }
        async fn describe_table(&self, _: Option<&str>, _: &str) -> Result<TableInfo, DriverError> {
            unreachable!("plan fixture does not describe tables")
        }
        async fn list_indexes(
            &self,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Vec<IndexInfo>, DriverError> {
            Ok(Vec::new())
        }
        async fn list_foreign_keys(
            &self,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Vec<ForeignKeyInfo>, DriverError> {
            Ok(Vec::new())
        }
        async fn resolve_schema(&self, _: Option<&str>) -> Result<String, DriverError> {
            Ok("dbo".into())
        }
        async fn approximate_row_counts(
            &self,
            _: Option<&str>,
        ) -> Result<Vec<TableRowCount>, DriverError> {
            Ok(Vec::new())
        }
        async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError> {
            Ok(Vec::new())
        }
        async fn ping(&self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn plan_session_receives_only_guarded_policy_filtered_single_select() {
        let fixture = Arc::new(PlanDriver::default());
        let driver: Arc<dyn EngineDriver> = fixture.clone();
        let policy = TablePolicy {
            allow: vec![TableRule {
                schema: Some("dbo".into()),
                table: "allowed".into(),
            }],
            row_filters: vec![RowFilter {
                schema: Some("dbo".into()),
                table: "allowed".into(),
                predicate: "tenant_id = 7".into(),
            }],
            ..TablePolicy::default()
        };
        let summary = analyze_query_with_options(
            &driver,
            "SELECT id FROM dbo.allowed WHERE id = ?",
            &[serde_json::json!(42)],
            WriteMode::ReadOnly,
            Duration::from_secs(1),
            AnalyzeOptions {
                table_policy: Some(policy),
                ..AnalyzeOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(summary.nodes[0].node_type, "Index Seek");
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].1);
        assert!(calls[0].0.sql.contains("tenant_id = 7"));
        assert!(!calls[0].0.sql.contains("SHOWPLAN"));
        assert_eq!(calls[0].0.params, vec![SqlValue::I64(42)]);
    }

    #[tokio::test]
    async fn low_level_analyze_enforces_write_gate_before_driver() {
        let fixture = Arc::new(PlanDriver::default());
        let driver: Arc<dyn EngineDriver> = fixture.clone();
        let result = analyze_query_with_options(
            &driver,
            "SELECT 1",
            &[],
            WriteMode::ReadOnly,
            Duration::from_secs(1),
            AnalyzeOptions {
                analyze: true,
                ..AnalyzeOptions::default()
            },
        )
        .await;
        assert!(matches!(result, Err(ExplainError::Guard(_))));
        assert!(fixture.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn plan_helper_cannot_bypass_single_statement_or_table_policy_guards() {
        let fixture = Arc::new(PlanDriver::default());
        let driver: Arc<dyn EngineDriver> = fixture.clone();
        for sql in [
            "SELECT 1; DROP TABLE dbo.allowed",
            "SELECT * FROM dbo.secret",
            "SELECT id INTO dbo.copy FROM dbo.allowed",
        ] {
            let result = analyze_query_with_options(
                &driver,
                sql,
                &[],
                WriteMode::AllowDdl,
                Duration::from_secs(1),
                AnalyzeOptions {
                    table_policy: Some(TablePolicy {
                        deny: vec![TableRule {
                            schema: Some("dbo".into()),
                            table: "secret".into(),
                        }],
                        ..TablePolicy::default()
                    }),
                    ..AnalyzeOptions::default()
                },
            )
            .await;
            assert!(result.is_err(), "{sql}");
        }
        assert!(fixture.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn index_advice_quotes_sqlserver_identifiers_without_executing_them() {
        assert_eq!(
            super::super::create_index_sql(EngineKind::SqlServer, "t]x", &["c]x".into()]),
            "CREATE INDEX [idx_t_x_c_x] ON [t]]x] ([c]]x]);"
        );
    }

    fn plan(body: &str) -> Vec<Vec<SqlValue>> {
        vec![vec![SqlValue::String(format!(
            r#"<ShowPlanXML xmlns="{SHOWPLAN_NS}"><BatchSequence><Batch><Statements><StmtSimple><QueryPlan>{body}</QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#
        ))]]
    }

    #[test]
    fn normalizes_namespaced_operators_without_stealing_child_metadata() {
        let input = plan(
            r#"<RelOp PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="2.5" EstimatedTotalSubtreeCost="4.2"><NestedLoops><RelOp PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="0.2"><IndexScan><Object Schema="[dbo]" Table="[a]]b]"/><SeekPredicates><ScalarOperator ScalarString="[a]]b].[id]&gt;@P1"/></SeekPredicates></IndexScan></RelOp></NestedLoops></RelOp>"#,
        );
        let summary = normalize("SELECT id FROM [a]]b]", &input).unwrap();
        assert_eq!(summary.engine, "sqlserver");
        assert_eq!(summary.total_cost, Some(4.2));
        assert_eq!(summary.plan_rows, Some(3));
        assert_eq!(summary.nodes[0].relation, None);
        assert_eq!(summary.nodes[0].filter, None);
        assert_eq!(summary.nodes[0].join_type.as_deref(), Some("Inner Join"));
        assert_eq!(summary.nodes[1].depth, 1);
        assert_eq!(summary.nodes[1].relation.as_deref(), Some("a]b"));
        assert_eq!(summary.nodes[1].filter.as_deref(), Some("[a]]b].[id]>@P1"));
        assert_eq!(summary.nodes[1].actual_rows, None);
    }

    #[test]
    fn aggregates_actual_parallel_rows_but_not_overlapping_elapsed_times() {
        let input = plan(
            r#"<RelOp PhysicalOp="Table Scan" LogicalOp="Table Scan" EstimateRows="10"><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="7" ActualElapsedms="4"/><RunTimeCountersPerThread Thread="1" ActualRows="9" ActualElapsedms="6"/></RunTimeInformation><Warnings><SpillToTempDb SpillLevel="1"/></Warnings></RelOp>"#,
        );
        let summary = normalize("SELECT * FROM t", &input).unwrap();
        assert_eq!(summary.nodes[0].actual_rows, Some(16));
        assert_eq!(summary.nodes[0].actual_time_ms, Some(6.0));
        assert_eq!(summary.nodes[0].spill.as_deref(), Some("tempdb spill"));
    }

    #[test]
    fn rejects_missing_malformed_and_unrelated_xml_without_echoing_payload() {
        assert!(normalize("SELECT 1", &[]).is_err());
        for xml in [
            "<broken secret",
            "<ShowPlanXML/>",
            "<other/>",
            "<!DOCTYPE x [<!ENTITY secret 'password'>]><x>&secret;</x>",
        ] {
            let result = normalize("SELECT 1", &[vec![SqlValue::String(xml.into())]]);
            let message = result.unwrap_err().to_string();
            assert!(!message.contains("password"));
            assert!(!message.contains("secret"));
        }
        assert!(normalize("SELECT 1", &plan("")).is_err());
    }
}
