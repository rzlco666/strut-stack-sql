pub mod bind;
pub mod exec;
pub mod explain;
pub mod pool;
pub mod schema;
pub mod value;

pub use exec::{execute_batch, execute_query, BatchResult, ExecError, ExecOptions, QueryResult};
pub use explain::{analyze_query, ExplainSummary};
pub use pool::{
    detect_engine_from_url, parse_host_port, EngineKind, EnginePool, LazyEnginePool,
    CONNECT_TIMEOUT, DEFAULT_CONNECT_TIMEOUT_SECS, TCP_PREFLIGHT_TIMEOUT,
};
pub use schema::{
    describe_table, list_foreign_keys, list_indexes, listed_index_from_schema_object, list_schemas,
    list_tables, schema_overview, search_objects, ForeignKeyInfo, ListedIndex, ObjectType,
    SchemaObject, SchemaOverview,
};
