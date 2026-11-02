//! Thin facade over the workspace crates, retained so integration tests and
//! embedders can keep importing `strut_stack_sql::<area>`.

pub use strut_sql_core as core;
pub use strut_sql_core::{config, redact};
pub use strut_sql_driver_api as driver_api;
pub use strut_sql_format as format;
pub use strut_sql_guard as guard;
pub use strut_sql_server as server;
pub use strut_sql_tools as tools;

pub use strut_sql_core::{AppConfig, WriteMode};
pub use strut_sql_driver_api::EngineKind;
pub use strut_sql_server::{build_http_router, McpSqlServer};

/// Compatibility module for legacy `strut_stack_sql::db::*` imports.
pub mod db {
    pub use strut_sql_driver_api::EngineKind;
    pub use strut_sql_tools::exec::redact_denied_columns;

    pub mod exec {
        pub use strut_sql_tools::exec::redact_denied_columns;
    }
}
