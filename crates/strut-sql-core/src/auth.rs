//! Principal + Policy authorization model (PRD §3.4, F1-09/F1-10).
//!
//! One [`Policy::authorize`] function decides every tool invocation and is
//! reused by `list_tools`, so what a caller can see and what a caller can
//! run can never drift apart.
//!
//! Tiers are the v1 [`WriteMode`] vocabulary plus `Admin`:
//! `Read < Write < Ddl < Admin`. `WriteMode` remains the process-level
//! ceiling: a grant can never lift a caller above `--allow-writes` /
//! `--allow-ddl`. `Admin` (GRANT/REVOKE) is additionally gated by the
//! explicit `--allow-grants` process flag, so a grant with `max_tier =
//! "admin"` alone still denies GRANT/REVOKE.
//!
//! # TOML schema
//!
//! ```toml
//! [[principals]]
//! id = "alice"
//! display_name = "Alice"   # optional, informational
//!
//! [[grants]]
//! principal = "alice"
//! source = "*"             # source name or "*" for every source
//! max_tier = "writes"      # read | writes | ddl | admin
//! max_rows = 50            # optional per-call row ceiling
//! max_bytes = 16384        # optional per-call response byte ceiling
//! query_timeout = 5        # optional per-call timeout in seconds
//! ```
//!
//! All tables use `#[serde(deny_unknown_fields)]`; grants are validated
//! when the configuration is loaded. Legacy HTTP token entries
//! (`STRUT_STACK_SQL_HTTP_TOKENS`, `--http-token`) are mapped onto the same
//! model: token name -> principal id, token mode -> `max_tier`, token
//! source allowlist -> one `Grant` per source (or `Any` when empty).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use strut_sql_driver_api::EngineKind;
use strut_sql_guard::{classify_sql_raw, SqlClass, WriteMode};
pub use strut_sql_guard::{ColumnMask, MaskMode, RowFilter, TablePolicy, TableRule};

/// Tools always exposed (every mode, every `full_tools` setting).
pub const CORE_TOOLS: &[&str] = &["search_objects", "execute_sql", "analyze_query_performance"];

/// Introspection tools exposed only with `--full-tools`.
pub const FULL_INTROSPECT_TOOLS: &[&str] = &[
    "list_sources",
    "list_schemas",
    "list_tables",
    "describe_table",
    "list_indexes",
    "list_foreign_keys",
    "schema_overview",
    "get_relationship_graph",
    "suggest_joins",
    "get_schema_context",
];

/// DDL tools exposed only with `--full-tools` (and, by tier, `--allow-ddl`).
pub const DDL_TOOLS: &[&str] = &[
    "schema_mutate",
    "create_table",
    "drop_table",
    "add_column",
    "alter_column",
    "drop_column",
    "truncate_table",
];

/// Static minimum tier for a tool, or `None` when the tool is not exposed
/// by this process at all (unknown name, or `--full-tools` disabled).
///
/// `execute_sql` and `analyze_query_performance` are listed as `Read`;
/// the caller raises the requirement to the classified SQL class before
/// calling [`Policy::authorize`], so a write statement needs a write grant.
pub fn tool_tier(tool: &str, full_tools: bool) -> Option<Tier> {
    // Virtual tool names used by MCP resources: reading the schema/policy
    // resources is a read-tier operation gated by the same Policy.
    if matches!(tool, "resources.read" | "prompts.get") {
        return Some(Tier::Read);
    }
    if CORE_TOOLS.contains(&tool) {
        Some(Tier::Read)
    } else if FULL_INTROSPECT_TOOLS.contains(&tool) {
        full_tools.then_some(Tier::Read)
    } else if DDL_TOOLS.contains(&tool) {
        full_tools.then_some(Tier::Ddl)
    } else {
        None
    }
}

/// The tier a SQL statement class requires.
pub fn sql_class_tier(class: SqlClass) -> Tier {
    match class {
        SqlClass::Read => Tier::Read,
        SqlClass::Dml => Tier::Write,
        SqlClass::Ddl => Tier::Ddl,
        SqlClass::Admin => Tier::Admin,
        // Transaction control / anything unclassified never reaches a
        // handler (the guard rejects it); requiring Ddl keeps a policy from
        // accidentally widening it.
        SqlClass::Txn | SqlClass::Other => Tier::Ddl,
    }
}

/// Minimum tier required to run `sql` on `engine`, or `None` when the
/// statement cannot be parsed (the handler will return the parse error).
pub fn sql_required_tier(sql: &str, engine: EngineKind) -> Option<Tier> {
    classify_sql_raw(sql, engine)
        .ok()
        .map(|class| sql_class_tier(class.sql_class()))
}

/// Authorization tier: the v1 write modes plus privilege management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    #[default]
    Read,
    Write,
    Ddl,
    Admin,
}

impl Tier {
    pub fn from_write_mode(mode: WriteMode) -> Self {
        match mode {
            WriteMode::ReadOnly => Self::Read,
            WriteMode::AllowWrites => Self::Write,
            WriteMode::AllowDdl => Self::Ddl,
        }
    }

    /// Effective guard mode for this tier. `Admin` runs with DDL-level SQL
    /// mode; the Admin gate itself is enforced by [`Policy::authorize`].
    pub fn write_mode(self) -> WriteMode {
        match self {
            Self::Read => WriteMode::ReadOnly,
            Self::Write => WriteMode::AllowWrites,
            Self::Ddl | Self::Admin => WriteMode::AllowDdl,
        }
    }

    pub fn allows_ddl(self) -> bool {
        self >= Self::Ddl
    }

    /// Parse a config tier string: `read`/`readonly`, `writes`/`write`/
    /// `dml`, `ddl`, `admin`.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" | "readonly" | "read-only" | "read_only" | "ro" => Ok(Self::Read),
            "write" | "writes" | "write-only" | "dml" => Ok(Self::Write),
            "ddl" => Ok(Self::Ddl),
            "admin" => Ok(Self::Admin),
            other => anyhow::bail!("invalid tier '{other}' (expected read, writes, ddl, or admin)"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "writes",
            Self::Ddl => "ddl",
            Self::Admin => "admin",
        }
    }
}

/// How a principal proved its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    OAuth,
    ApiKey,
    LocalStdio,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OAuth => "oauth",
            Self::ApiKey => "api_key",
            Self::LocalStdio => "local_stdio",
        }
    }
}

/// An authenticated caller. ``id` is the only identity trusted for
/// authorization; tool arguments are never treated as identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub id: String,
    pub display_name: Option<String>,
    pub auth_method: AuthMethod,
}

impl Principal {
    /// Synthesized principal for stdio and unauthenticated loopback HTTP.
    pub const LOCAL_ID: &'static str = "local";

    pub fn local() -> Self {
        Self {
            id: Self::LOCAL_ID.to_string(),
            display_name: Some("local operator".to_string()),
            auth_method: AuthMethod::LocalStdio,
        }
    }

    pub fn new(id: impl Into<String>, auth_method: AuthMethod) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            auth_method,
        }
    }
}

/// Which sources a grant applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantSource {
    /// One configured source name.
    Name(String),
    /// Every source (`"*"`).
    Any,
}

impl GrantSource {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            anyhow::bail!("grant source must not be empty (use \"*\" for all sources)");
        }
        if raw == "*" {
            Ok(Self::Any)
        } else {
            Ok(Self::Name(raw.to_string()))
        }
    }

    pub fn matches(&self, source: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Name(name) => name == source,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Any => "*",
            Self::Name(name) => name,
        }
    }
}

/// One principal x source x tier grant with optional per-call ceilings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub principal: String,
    pub source: GrantSource,
    pub max_tier: Tier,
    pub max_rows: Option<u32>,
    pub max_bytes: Option<usize>,
    pub query_timeout: Option<Duration>,
    /// Opt-in in-memory session query history for this principal (PRD-F3-08).
    pub query_history: bool,
    /// Table allow/deny lists, row filters and column masks (R2-04).
    pub tables: TablePolicy,
}

impl Grant {
    pub fn new(principal: impl Into<String>, source: GrantSource, max_tier: Tier) -> Self {
        Self {
            principal: principal.into(),
            source,
            max_tier,
            max_rows: None,
            max_bytes: None,
            query_timeout: None,
            query_history: false,
            tables: TablePolicy::default(),
        }
    }
}

/// Result of a successful authorization: the grant's effective tier after
/// the process ceiling and the per-call ceilings to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    pub principal_id: String,
    pub source: String,
    /// Effective tier after the process ceiling (`<=` grant.max_tier).
    pub tier: Tier,
    pub max_rows: Option<u32>,
    pub max_bytes: Option<usize>,
    pub query_timeout: Option<Duration>,
    /// Session query history was explicitly enabled for this grant.
    pub query_history: bool,
    /// Effective table allow/deny, row-filter and mask rules.
    pub tables: TablePolicy,
}

impl Authorization {
    /// Guard/execution mode for this authorization.
    pub fn write_mode(&self) -> WriteMode {
        self.tier.write_mode()
    }
}

/// Why an invocation was rejected. Messages are intentionally generic
/// when surfaced to clients (they name principal/source/tier but never
/// credentials or SQL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    /// Tool is not part of the process tool set (or is unknown), so it is
    /// hidden from `list_tools` and must not be callable directly.
    ToolNotExposed { tool: String },
    /// No grant for this principal on the requested source.
    NoGrant { principal: String, source: String },
    /// The grant exists but is below the required tier.
    TierTooLow {
        required: Tier,
        effective: Tier,
        source: String,
    },
    /// GRANT/REVOKE (or another Admin-class statement) without the
    /// explicit process-level `--allow-grants` opt-in.
    AdminGateDisabled,
}

impl std::fmt::Display for Denial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolNotExposed { tool } => {
                write!(f, "tool '{tool}' is not exposed by this server")
            }
            Self::NoGrant { principal, source } => {
                write!(
                    f,
                    "principal '{principal}' has no grant for source '{source}'"
                )
            }
            Self::TierTooLow {
                required,
                effective,
                source,
            } => write!(
                f,
                "tier '{}' is below the required '{}' on source '{source}'",
                effective.as_str(),
                required.as_str()
            ),
            Self::AdminGateDisabled => write!(
                f,
                "GRANT/REVOKE requires an Admin grant and the --allow-grants process flag"
            ),
        }
    }
}

/// Authorization decision returned by [`Policy::authorize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow(Authorization),
    Deny(Denial),
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow(_))
    }
}

/// The single authorization policy: grants + the process-wide ceiling.
///
/// The process ceiling is the v1 [`WriteMode`] (Read/Write/Ddl). `Admin` is
/// reachable only when BOTH an explicit Admin grant exists AND the process
/// was started with `--allow-grants` (in DDL mode); otherwise an Admin grant
/// is capped back to `Ddl`, so policy can never exceed the process grant.
#[derive(Debug, Clone)]
pub struct Policy {
    grants: Vec<Grant>,
    display_names: std::collections::HashMap<String, String>,
    ceiling: Tier,
    full_tools: bool,
    allow_grants: bool,
}

impl Policy {
    pub fn new(ceiling: Tier, full_tools: bool, allow_grants: bool) -> Self {
        Self {
            grants: Vec::new(),
            display_names: std::collections::HashMap::new(),
            ceiling,
            full_tools,
            allow_grants,
        }
    }

    /// Policy for a process with no configured principals: the synthesized
    /// local principal gets the process ceiling on every source.
    pub fn local_default(ceiling: Tier, full_tools: bool) -> Self {
        Self::new(ceiling, full_tools, false).with_local_default()
    }

    /// Add the synthesized local principal grant if none was configured.
    pub fn with_local_default(mut self) -> Self {
        if !self
            .grants
            .iter()
            .any(|g| g.principal == Principal::LOCAL_ID)
        {
            self.grants.push(Grant::new(
                Principal::LOCAL_ID,
                GrantSource::Any,
                self.ceiling,
            ));
        }
        self
    }

    pub fn add_grant(&mut self, grant: Grant) {
        self.grants.push(grant);
    }

    /// Enable/disable in-memory query history on the synthesized local
    /// principal's wildcard grant (process-wide env opt-in, PRD-F3-08).
    pub fn set_local_query_history(&mut self, enabled: bool) {
        for grant in &mut self.grants {
            if grant.principal == Principal::LOCAL_ID && grant.source == GrantSource::Any {
                grant.query_history = enabled;
            }
        }
    }

    /// Insert a grant, replacing any existing grant for the same
    /// principal + source. Explicit TOML grants use this to override the
    /// implicit grant derived from a legacy HTTP token entry.
    pub fn set_grant(&mut self, grant: Grant) {
        self.grants.retain(|existing| {
            !(existing.principal == grant.principal && existing.source == grant.source)
        });
        self.grants.push(grant);
    }

    pub fn set_display_name(&mut self, id: impl Into<String>, name: impl Into<String>) {
        self.display_names.insert(id.into(), name.into());
    }

    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    pub fn ceiling(&self) -> Tier {
        self.ceiling
    }

    pub fn allow_grants(&self) -> bool {
        self.allow_grants
    }

    /// Build a principal from an authenticated identity, filling in a
    /// configured display name when available.
    pub fn resolve_principal(&self, id: &str, auth_method: AuthMethod) -> Principal {
        Principal {
            id: id.to_string(),
            display_name: self.display_names.get(id).cloned(),
            auth_method,
        }
    }

    /// The synthesized local operator principal.
    pub fn local_principal(&self) -> Principal {
        Principal {
            display_name: self
                .display_names
                .get(Principal::LOCAL_ID)
                .cloned()
                .or_else(|| Some("local operator".to_string())),
            ..Principal::local()
        }
    }

    /// Whether the Admin gate is open: explicit flag AND a DDL-capable
    /// process. Without both, Admin-class statements are always denied.
    pub fn admin_gate_open(&self) -> bool {
        self.allow_grants && self.ceiling.allows_ddl()
    }

    /// The one authorization function. `tool` is checked for process
    /// visibility, the principal's grants are matched on `source`, and the
    /// grant tier is capped by the process ceiling.
    pub fn authorize(
        &self,
        principal: &Principal,
        tool: &str,
        source: &str,
        required_tier: Tier,
    ) -> Decision {
        let Some(base) = tool_tier(tool, self.full_tools) else {
            return Decision::Deny(Denial::ToolNotExposed {
                tool: tool.to_string(),
            });
        };
        let required = base.max(required_tier);

        if required == Tier::Admin && !self.admin_gate_open() {
            return Decision::Deny(Denial::AdminGateDisabled);
        }

        let matching: Vec<&Grant> = self
            .grants
            .iter()
            .filter(|g| g.principal == principal.id && g.source.matches(source))
            .collect();
        let Some(grant) = matching
            .iter()
            .copied()
            // Most permissive matching grant wins; deterministic order.
            .max_by_key(|g| (self.effective_tier(g), g.max_rows.unwrap_or(u32::MAX)))
        else {
            return Decision::Deny(Denial::NoGrant {
                principal: principal.id.clone(),
                source: source.to_string(),
            });
        };

        let effective = self.effective_tier(grant);
        if effective < required {
            return Decision::Deny(Denial::TierTooLow {
                required,
                effective,
                source: source.to_string(),
            });
        }

        // Table rules from every matching grant accumulate: a deny in any of
        // them applies, and row filters stack as AND conditions.
        let mut tables = TablePolicy::default();
        for grant in &matching {
            tables.merge(&grant.tables);
        }
        Decision::Allow(Authorization {
            principal_id: principal.id.clone(),
            source: source.to_string(),
            tier: effective,
            max_rows: grant.max_rows,
            max_bytes: grant.max_bytes,
            query_timeout: grant.query_timeout,
            query_history: grant.query_history,
            tables,
        })
    }

    /// Grant tier after the process ceiling and the Admin flag.
    fn effective_tier(&self, grant: &Grant) -> Tier {
        if grant.max_tier == Tier::Admin {
            // Admin is only unlocked by the explicit process gate; without
            // it the grant is capped to the DDL ceiling.
            if self.admin_gate_open() {
                Tier::Admin
            } else {
                self.ceiling.min(Tier::Ddl)
            }
        } else {
            grant.max_tier.min(self.ceiling)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(ceiling: Tier, allow_grants: bool) -> Policy {
        let mut p = Policy::new(ceiling, true, allow_grants);
        p.add_grant(Grant {
            principal: "alice".into(),
            source: GrantSource::Name("main".into()),
            max_tier: Tier::Read,
            max_rows: Some(10),
            max_bytes: Some(1024),
            query_timeout: Some(Duration::from_secs(2)),
            query_history: true,
            tables: TablePolicy::default(),
        });
        p.add_grant(Grant {
            principal: "bob".into(),
            source: GrantSource::Any,
            max_tier: Tier::Write,
            max_rows: None,
            max_bytes: None,
            query_timeout: None,
            query_history: false,
            tables: TablePolicy::default(),
        });
        p.add_grant(Grant {
            principal: "root".into(),
            source: GrantSource::Any,
            max_tier: Tier::Admin,
            ..Grant::new("root", GrantSource::Any, Tier::Admin)
        });
        p.with_local_default()
    }

    fn allow(decision: Decision) -> Authorization {
        match decision {
            Decision::Allow(a) => a,
            Decision::Deny(d) => panic!("expected allow, got {d}"),
        }
    }

    fn deny(decision: Decision) -> Denial {
        match decision {
            Decision::Allow(a) => panic!("expected deny, got {a:?}"),
            Decision::Deny(d) => d,
        }
    }

    #[test]
    fn tier_ordering_and_parsing() {
        assert!(Tier::Read < Tier::Write);
        assert!(Tier::Write < Tier::Ddl);
        assert!(Tier::Ddl < Tier::Admin);
        assert_eq!(Tier::parse("readonly").unwrap(), Tier::Read);
        assert_eq!(Tier::parse("writes").unwrap(), Tier::Write);
        assert_eq!(Tier::parse("DDL").unwrap(), Tier::Ddl);
        assert_eq!(Tier::parse("admin").unwrap(), Tier::Admin);
        assert!(Tier::parse("root").is_err());
        assert_eq!(Tier::from_write_mode(WriteMode::AllowWrites), Tier::Write);
    }

    #[test]
    fn read_grant_allows_read_tool_and_denies_write_statement() {
        let p = policy(Tier::Ddl, false);
        let alice = p.resolve_principal("alice", AuthMethod::ApiKey);
        let a = allow(p.authorize(&alice, "execute_sql", "main", Tier::Read));
        assert_eq!(a.tier, Tier::Read);
        assert_eq!(a.max_rows, Some(10));
        assert_eq!(a.max_bytes, Some(1024));
        assert_eq!(a.query_timeout, Some(Duration::from_secs(2)));
        assert_eq!(a.write_mode(), WriteMode::ReadOnly);

        let d = deny(p.authorize(&alice, "execute_sql", "main", Tier::Write));
        assert!(matches!(d, Denial::TierTooLow { .. }), "{d}");
    }

    #[test]
    fn source_wildcard_and_specific_source() {
        let p = policy(Tier::Ddl, false);
        let bob = p.resolve_principal("bob", AuthMethod::ApiKey);
        // Any-source grant.
        allow(p.authorize(&bob, "execute_sql", "reporting", Tier::Write));
        // Alice's grant is scoped to "main".
        let alice = p.resolve_principal("alice", AuthMethod::ApiKey);
        allow(p.authorize(&alice, "execute_sql", "main", Tier::Read));
        let d = deny(p.authorize(&alice, "execute_sql", "other", Tier::Read));
        assert!(matches!(d, Denial::NoGrant { .. }), "{d}");
    }

    #[test]
    fn process_ceiling_caps_grants() {
        let p = policy(Tier::Read, false);
        let bob = p.resolve_principal("bob", AuthMethod::ApiKey);
        // Process ceiling is ReadOnly: bob's Write grant caps to Read.
        let a = allow(p.authorize(&bob, "execute_sql", "any", Tier::Read));
        assert_eq!(a.tier, Tier::Read);
        let d = deny(p.authorize(&bob, "execute_sql", "any", Tier::Write));
        assert!(matches!(
            d,
            Denial::TierTooLow {
                effective: Tier::Read,
                ..
            }
        ));
    }

    #[test]
    fn hidden_tools_are_denied_even_for_admin_grants() {
        let mut p = Policy::local_default(Tier::Ddl, false);
        p.add_grant(Grant::new("root", GrantSource::Any, Tier::Admin));
        let root = Principal::new("root", AuthMethod::ApiKey);
        let d = deny(p.authorize(&root, "create_table", "default", Tier::Ddl));
        assert!(matches!(d, Denial::ToolNotExposed { .. }), "{d}");
        // Unknown tool names are never authorized.
        assert!(matches!(
            deny(p.authorize(&root, "not_a_tool", "default", Tier::Read)),
            Denial::ToolNotExposed { .. }
        ));
    }

    #[test]
    fn admin_gate_defaults_closed() {
        let p = policy(Tier::Ddl, false);
        let root = p.resolve_principal("root", AuthMethod::ApiKey);
        let d = deny(p.authorize(&root, "execute_sql", "main", Tier::Admin));
        assert_eq!(d, Denial::AdminGateDisabled);
        // Admin grant still allows ordinary DDL when the gate is closed.
        allow(p.authorize(&root, "execute_sql", "main", Tier::Ddl));
        assert!(!p.admin_gate_open());
    }

    #[test]
    fn admin_gate_open_allows_admin_grant_only() {
        let p = policy(Tier::Ddl, true);
        let root = p.resolve_principal("root", AuthMethod::ApiKey);
        let a = allow(p.authorize(&root, "execute_sql", "main", Tier::Admin));
        assert_eq!(a.tier, Tier::Admin);
        assert_eq!(a.write_mode(), WriteMode::AllowDdl);

        // Non-admin grant cannot reach Admin even with the gate open.
        let bob = p.resolve_principal("bob", AuthMethod::ApiKey);
        let d = deny(p.authorize(&bob, "execute_sql", "main", Tier::Admin));
        assert!(
            matches!(d, Denial::AdminGateDisabled | Denial::TierTooLow { .. }),
            "{d}"
        );
    }

    #[test]
    fn admin_gate_requires_ddl_process() {
        let p = policy(Tier::Write, true);
        let root = p.resolve_principal("root", AuthMethod::ApiKey);
        assert!(!p.admin_gate_open());
        assert_eq!(
            deny(p.authorize(&root, "execute_sql", "main", Tier::Admin)),
            Denial::AdminGateDisabled
        );
    }

    #[test]
    fn unknown_principal_denied() {
        let p = policy(Tier::Ddl, false);
        let ghost = Principal::new("ghost", AuthMethod::OAuth);
        assert!(matches!(
            deny(p.authorize(&ghost, "execute_sql", "main", Tier::Read)),
            Denial::NoGrant { .. }
        ));
    }

    #[test]
    fn local_principal_gets_process_ceiling_on_all_sources() {
        let p = Policy::local_default(Tier::Write, true);
        let local = p.local_principal();
        let a = allow(p.authorize(&local, "execute_sql", "whatever", Tier::Write));
        assert_eq!(a.tier, Tier::Write);
        assert!(matches!(
            deny(p.authorize(&local, "execute_sql", "whatever", Tier::Ddl)),
            Denial::TierTooLow { .. }
        ));
    }

    #[test]
    fn local_grant_can_be_restricted_by_config() {
        let mut p = Policy::new(Tier::Ddl, true, false);
        p.add_grant(Grant::new(
            Principal::LOCAL_ID,
            GrantSource::Name("main".into()),
            Tier::Read,
        ));
        let local = p.local_principal();
        allow(p.authorize(&local, "execute_sql", "main", Tier::Read));
        assert!(matches!(
            deny(p.authorize(&local, "execute_sql", "other", Tier::Read)),
            Denial::NoGrant { .. }
        ));
    }

    #[test]
    fn tool_visibility_follows_full_tools() {
        assert_eq!(tool_tier("execute_sql", false), Some(Tier::Read));
        assert_eq!(tool_tier("describe_table", false), None);
        assert_eq!(tool_tier("describe_table", true), Some(Tier::Read));
        assert_eq!(tool_tier("create_table", true), Some(Tier::Ddl));
        assert_eq!(tool_tier("nope", true), None);
    }

    #[test]
    fn sql_required_tier_maps_classes() {
        use strut_sql_driver_api::EngineKind;
        assert_eq!(
            sql_required_tier("SELECT 1", EngineKind::Postgres),
            Some(Tier::Read)
        );
        assert_eq!(
            sql_required_tier("INSERT INTO t VALUES (1)", EngineKind::Postgres),
            Some(Tier::Write)
        );
        assert_eq!(
            sql_required_tier("DROP TABLE t", EngineKind::Postgres),
            Some(Tier::Ddl)
        );
        assert_eq!(
            sql_required_tier("GRANT SELECT ON t TO bob", EngineKind::Postgres),
            Some(Tier::Admin)
        );
    }
}
