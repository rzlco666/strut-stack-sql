//! Foreign-key join graph and join-path suggestions (PRD-F3-02).
//!
//! The graph is built from the same cached schema overview used by
//! `schema_overview`/`get_schema_context` (PRD-F2-04), so repeated graph
//! queries do not re-introspect the catalog. Traversal is undirected:
//! `orders.user_id -> users.id` also connects `users` to `orders`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use serde::Serialize;
use strut_sql_driver_api::EngineDriver;

use crate::introspect::{schema_overview, ForeignKeyInfo};

#[derive(Debug, Clone, Serialize)]
pub struct GraphNode {
    pub table: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approx_rows: Option<i64>,
    pub fk_in: usize,
    pub fk_out: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphEdge {
    pub from_table: String,
    pub from_columns: Vec<String>,
    pub to_table: String,
    pub to_columns: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_update: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphMeta {
    pub n_nodes: usize,
    pub n_edges: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationshipGraph {
    pub schema: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub meta: GraphMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuggestedJoin {
    pub left_table: String,
    pub left_columns: Vec<String>,
    pub right_table: String,
    pub right_columns: Vec<String>,
    pub join_type_suggestion: &'static str,
    /// SQL-ready condition, e.g. `orders.user_id = users.id`.
    pub on: String,
    /// `fk` for graph paths, `guess` for same-name column candidates.
    pub confidence: &'static str,
    #[serde(skip_serializing_if = "is_false")]
    pub guess: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize)]
pub struct SuggestJoins {
    pub schema: String,
    pub joins: Vec<SuggestedJoin>,
    pub unreachable: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

const MAX_GRAPH_NODES: usize = 200;
const MAX_GRAPH_EDGES: usize = 500;
const MAX_DEPTH: usize = 4;

struct JoinGraph {
    schema: String,
    nodes: BTreeMap<String, GraphNode>,
    edges: Vec<GraphEdge>,
    source_truncated: bool,
}

impl JoinGraph {
    fn build(
        schema: String,
        tables: &[(String, Option<i64>)],
        fks: &[ForeignKeyInfo],
        source_truncated: bool,
    ) -> Self {
        let mut nodes: BTreeMap<String, GraphNode> = tables
            .iter()
            .map(|(table, rows)| {
                (
                    table.clone(),
                    GraphNode {
                        table: table.clone(),
                        approx_rows: *rows,
                        fk_in: 0,
                        fk_out: 0,
                    },
                )
            })
            .collect();
        let mut seen_edges: BTreeSet<(String, Vec<String>, String, Vec<String>)> = BTreeSet::new();
        let mut edges = Vec::new();
        for fk in fks {
            let key = (
                fk.table.clone(),
                fk.columns.clone(),
                fk.ref_table.clone(),
                fk.ref_columns.clone(),
            );
            if !seen_edges.insert(key) {
                continue;
            }
            if let Some(node) = nodes.get_mut(&fk.table) {
                node.fk_out += 1;
            }
            if let Some(node) = nodes.get_mut(&fk.ref_table) {
                node.fk_in += 1;
            }
            edges.push(GraphEdge {
                from_table: fk.table.clone(),
                from_columns: fk.columns.clone(),
                to_table: fk.ref_table.clone(),
                to_columns: fk.ref_columns.clone(),
                name: Some(fk.name.clone()).filter(|n| !n.is_empty()),
                on_delete: fk.on_delete.clone(),
                on_update: fk.on_update.clone(),
            });
        }
        edges.sort_by(|a, b| {
            (
                a.from_table.as_str(),
                a.from_columns.as_slice(),
                a.to_table.as_str(),
                a.to_columns.as_slice(),
            )
                .cmp(&(
                    b.from_table.as_str(),
                    b.from_columns.as_slice(),
                    b.to_table.as_str(),
                    b.to_columns.as_slice(),
                ))
        });
        Self {
            schema,
            nodes,
            edges,
            source_truncated,
        }
    }

    fn adjacency(&self) -> BTreeMap<&str, Vec<(&str, &GraphEdge)>> {
        let mut adjacency: BTreeMap<&str, Vec<(&str, &GraphEdge)>> = BTreeMap::new();
        for edge in &self.edges {
            adjacency
                .entry(edge.from_table.as_str())
                .or_default()
                .push((edge.to_table.as_str(), edge));
            adjacency
                .entry(edge.to_table.as_str())
                .or_default()
                .push((edge.from_table.as_str(), edge));
        }
        for neighbours in adjacency.values_mut() {
            neighbours.sort_by(|a, b| {
                (a.0, a.1.from_table.as_str(), a.1.from_columns.as_slice()).cmp(&(
                    b.0,
                    b.1.from_table.as_str(),
                    b.1.from_columns.as_slice(),
                ))
            });
        }
        adjacency
    }

    /// BFS shortest path (deterministic tie-break by sorted neighbours).
    #[cfg(test)]
    fn shortest_path(&self, start: &str, goal: &str) -> Option<Vec<GraphEdge>> {
        if start == goal {
            return Some(Vec::new());
        }
        let adjacency = self.adjacency();
        let mut queue = VecDeque::new();
        let mut visited: BTreeMap<&str, (&str, Option<&GraphEdge>)> = BTreeMap::new();
        visited.insert(start, (start, None));
        queue.push_back(start);
        while let Some(current) = queue.pop_front() {
            for (next, edge) in adjacency.get(current).into_iter().flatten() {
                if visited.contains_key(*next) {
                    continue;
                }
                visited.insert(next, (current, Some(*edge)));
                if *next == goal {
                    let mut path = Vec::new();
                    let mut cursor = goal;
                    while cursor != start {
                        let (previous, edge) = visited[cursor];
                        path.push(edge?.clone());
                        cursor = previous;
                    }
                    path.reverse();
                    return Some(path);
                }
                queue.push_back(next);
            }
        }
        None
    }

    /// Multi-source shortest path used to attach the next requested table to
    /// the already-connected set.
    fn shortest_path_from_set(
        &self,
        starts: &BTreeSet<String>,
        goal: &str,
    ) -> Option<Vec<GraphEdge>> {
        if starts.contains(goal) {
            return Some(Vec::new());
        }
        let adjacency = self.adjacency();
        let mut queue = VecDeque::new();
        let mut visited: BTreeMap<&str, (&str, Option<&GraphEdge>)> = BTreeMap::new();
        for start in starts {
            visited.insert(start.as_str(), (start.as_str(), None));
            queue.push_back(start.as_str());
        }
        while let Some(current) = queue.pop_front() {
            for (next, edge) in adjacency.get(current).into_iter().flatten() {
                if visited.contains_key(*next) {
                    continue;
                }
                visited.insert(next, (current, Some(*edge)));
                if *next == goal {
                    let mut path = Vec::new();
                    let mut cursor = goal;
                    while !starts.iter().any(|s| s == cursor) {
                        let (previous, edge) = visited[cursor];
                        path.push(edge?.clone());
                        cursor = previous;
                    }
                    path.reverse();
                    return Some(path);
                }
                queue.push_back(next);
            }
        }
        None
    }

    fn focus_graph(&self, focus: &str, depth: usize) -> Option<(Vec<GraphNode>, Vec<GraphEdge>)> {
        if !self.nodes.contains_key(focus) {
            return None;
        }
        let adjacency = self.adjacency();
        let mut distance: BTreeMap<&str, usize> = BTreeMap::new();
        let mut queue = VecDeque::new();
        distance.insert(focus, 0);
        queue.push_back(focus);
        while let Some(current) = queue.pop_front() {
            let current_distance = distance[current];
            if current_distance >= depth {
                continue;
            }
            for (next, _) in adjacency.get(current).into_iter().flatten() {
                if distance.contains_key(*next) {
                    continue;
                }
                distance.insert(next, current_distance + 1);
                queue.push_back(next);
            }
        }
        let nodes: Vec<GraphNode> = self
            .nodes
            .values()
            .filter(|n| distance.contains_key(n.table.as_str()))
            .cloned()
            .collect();
        let keep: BTreeSet<&str> = nodes.iter().map(|n| n.table.as_str()).collect();
        let edges = self
            .edges
            .iter()
            .filter(|e| keep.contains(e.from_table.as_str()) && keep.contains(e.to_table.as_str()))
            .cloned()
            .collect();
        Some((nodes, edges))
    }
}

fn cap_graph(
    mut nodes: Vec<GraphNode>,
    mut edges: Vec<GraphEdge>,
    requested_nodes: usize,
    requested_edges: usize,
) -> (Vec<GraphNode>, Vec<GraphEdge>, bool) {
    let mut truncated = false;
    if nodes.len() > MAX_GRAPH_NODES {
        nodes.truncate(MAX_GRAPH_NODES);
        truncated = true;
    }
    let keep: BTreeSet<&str> = nodes.iter().map(|n| n.table.as_str()).collect();
    edges.retain(|e| keep.contains(e.from_table.as_str()) && keep.contains(e.to_table.as_str()));
    if edges.len() > MAX_GRAPH_EDGES {
        edges.truncate(MAX_GRAPH_EDGES);
        truncated = true;
    }
    let changed = requested_nodes != nodes.len() || requested_edges != edges.len();
    (nodes, edges, truncated || changed)
}

async fn load_graph(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
) -> Result<JoinGraph, String> {
    let overview = schema_overview(driver, source, schema).await?;
    let tables: Vec<(String, Option<i64>)> = overview
        .tables
        .iter()
        .map(|t| (t.name.clone(), t.approx_rows))
        .collect();
    Ok(JoinGraph::build(
        overview.schema,
        &tables,
        &overview.foreign_keys,
        overview.meta.truncated,
    ))
}

/// Build the (optionally focus/depth-limited) relationship graph.
pub async fn relationship_graph(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
    focus: Option<&str>,
    depth: Option<usize>,
) -> Result<RelationshipGraph, String> {
    let graph = load_graph(driver, source, schema).await?;
    let depth = depth.unwrap_or(2).min(MAX_DEPTH);
    let (nodes, edges) = match focus {
        Some(table) => graph
            .focus_graph(table, depth)
            .ok_or_else(|| format!("unknown table '{table}' in schema '{}'", graph.schema))?,
        None => (graph.nodes.values().cloned().collect(), graph.edges.clone()),
    };
    let requested_nodes = nodes.len();
    let requested_edges = edges.len();
    let (nodes, edges, capped) = cap_graph(nodes, edges, requested_nodes, requested_edges);
    let truncated = capped || graph.source_truncated;
    Ok(RelationshipGraph {
        schema: graph.schema,
        meta: GraphMeta {
            n_nodes: nodes.len(),
            n_edges: edges.len(),
            truncated,
            focus: focus.map(str::to_string),
            depth: focus.map(|_| depth),
        },
        nodes,
        edges,
    })
}

/// Shortest FK join paths connecting the requested tables in the given
/// order. Tables with no FK path are reported and replaced with explicitly
/// marked same-name column guesses.
pub async fn suggest_joins(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    tables: &[String],
    schema: Option<&str>,
) -> Result<SuggestJoins, String> {
    let graph = load_graph(driver, source, schema).await?;
    let mut joins = Vec::new();
    let mut unreachable = Vec::new();
    let mut notes = Vec::new();
    if graph.source_truncated {
        notes.push("schema graph truncated to the 200-table cap; paths may be incomplete".into());
    }
    let mut connected: BTreeSet<String> = BTreeSet::new();
    let mut ordered = Vec::new();
    for requested in tables {
        let actual = graph
            .nodes
            .keys()
            .find(|name| name.eq_ignore_ascii_case(requested))
            .cloned()
            .unwrap_or_else(|| requested.clone());
        if !graph.nodes.contains_key(&actual) {
            unreachable.push(actual);
            continue;
        }
        ordered.push(actual);
    }
    if let Some(first) = ordered.first() {
        connected.insert(first.clone());
    }
    for table in ordered.iter().skip(1) {
        if connected.contains(table) {
            continue;
        }
        match graph.shortest_path_from_set(&connected, table) {
            Some(path) => {
                for edge in path {
                    joins.push(edge_to_join(&edge, None, None));
                    connected.insert(edge.from_table.clone());
                    connected.insert(edge.to_table.clone());
                }
            }
            None => {
                unreachable.push(table.clone());
                notes.push(format!(
                    "no foreign-key path to '{table}'; same-name column candidates are guesses"
                ));
                if let Some(previous) = ordered.iter().take_while(|t| *t != table).last() {
                    if previous != table {
                        let candidates =
                            same_name_candidates(driver, schema, previous, table).await?;
                        if candidates.is_empty() {
                            notes.push(format!(
                                "no same-name columns between '{previous}' and '{table}'"
                            ));
                        }
                        joins.extend(candidates);
                    }
                }
            }
        }
    }
    Ok(SuggestJoins {
        schema: graph.schema,
        joins,
        unreachable,
        notes,
    })
}

fn edge_to_join(edge: &GraphEdge, from: Option<&str>, to: Option<&str>) -> SuggestedJoin {
    let left_is_child = from.is_none_or(|f| f == edge.from_table);
    let (left_table, left_columns, right_table, right_columns) = if left_is_child {
        (
            edge.from_table.clone(),
            edge.from_columns.clone(),
            edge.to_table.clone(),
            edge.to_columns.clone(),
        )
    } else {
        (
            edge.to_table.clone(),
            edge.to_columns.clone(),
            edge.from_table.clone(),
            edge.from_columns.clone(),
        )
    };
    let _ = to;
    SuggestedJoin {
        on: condition(&left_table, &left_columns, &right_table, &right_columns),
        left_table,
        left_columns,
        right_table,
        right_columns,
        join_type_suggestion: "INNER JOIN",
        confidence: "fk",
        guess: false,
    }
}

pub(crate) fn condition(
    left_table: &str,
    left_columns: &[String],
    right_table: &str,
    right_columns: &[String],
) -> String {
    left_columns
        .iter()
        .zip(right_columns.iter())
        .map(|(l, r)| format!("{left_table}.{l} = {right_table}.{r}"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

async fn same_name_candidates(
    driver: &Arc<dyn EngineDriver>,
    schema: Option<&str>,
    left: &str,
    right: &str,
) -> Result<Vec<SuggestedJoin>, String> {
    let left_info = driver
        .describe_table(schema, left)
        .await
        .map_err(|e| e.to_string())?;
    let right_info = driver
        .describe_table(schema, right)
        .await
        .map_err(|e| e.to_string())?;
    let right_columns: BTreeSet<String> = right_info
        .columns
        .iter()
        .map(|c| c.name.to_ascii_lowercase())
        .collect();
    let mut candidates = Vec::new();
    for column in &left_info.columns {
        if right_columns.contains(&column.name.to_ascii_lowercase()) {
            let cols = vec![column.name.clone()];
            candidates.push(SuggestedJoin {
                left_table: left.to_string(),
                left_columns: cols.clone(),
                right_table: right.to_string(),
                right_columns: cols.clone(),
                join_type_suggestion: "INNER JOIN",
                on: condition(left, &cols, right, &cols),
                confidence: "guess",
                guess: true,
            });
        }
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: &str, from_col: &str, to: &str, to_col: &str) -> GraphEdge {
        GraphEdge {
            from_table: from.into(),
            from_columns: vec![from_col.into()],
            to_table: to.into(),
            to_columns: vec![to_col.into()],
            name: None,
            on_delete: None,
            on_update: None,
        }
    }

    fn graph() -> JoinGraph {
        JoinGraph {
            schema: "main".into(),
            nodes: BTreeMap::from([
                (
                    "users".to_string(),
                    GraphNode {
                        table: "users".into(),
                        approx_rows: Some(10),
                        fk_in: 1,
                        fk_out: 0,
                    },
                ),
                (
                    "orders".to_string(),
                    GraphNode {
                        table: "orders".into(),
                        approx_rows: Some(100),
                        fk_in: 1,
                        fk_out: 0,
                    },
                ),
                (
                    "order_items".to_string(),
                    GraphNode {
                        table: "order_items".into(),
                        approx_rows: Some(500),
                        fk_in: 0,
                        fk_out: 2,
                    },
                ),
                (
                    "audit".to_string(),
                    GraphNode {
                        table: "audit".into(),
                        approx_rows: Some(1),
                        fk_in: 0,
                        fk_out: 0,
                    },
                ),
            ]),
            edges: vec![
                edge("orders", "user_id", "users", "id"),
                edge("order_items", "order_id", "orders", "id"),
            ],
            source_truncated: false,
        }
    }

    #[test]
    fn shortest_path_is_deterministic_and_correct() {
        let path = graph()
            .shortest_path("order_items", "users")
            .expect("path through orders");
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].from_table, "order_items");
        assert_eq!(path[1].to_table, "users");
    }

    #[test]
    fn missing_path_is_none() {
        assert!(graph().shortest_path("audit", "users").is_none());
    }

    #[test]
    fn focus_depth_limits_nodes() {
        let (nodes, _) = graph().focus_graph("users", 1).expect("focus");
        let names: Vec<&str> = nodes.iter().map(|n| n.table.as_str()).collect();
        assert_eq!(names, vec!["orders", "users"]);
    }

    #[test]
    fn composite_condition_joins_columns() {
        let condition = condition(
            "child",
            &["a".into(), "b".into()],
            "parent",
            &["x".into(), "y".into()],
        );
        assert_eq!(condition, "child.a = parent.x AND child.b = parent.y");
    }
}
