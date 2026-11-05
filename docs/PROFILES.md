# Tool profiles

**The default is `lite`.** Running the binary with no profile and no tool flags
exposes exactly three tools — `search_objects`, `execute_sql`,
`analyze_query_performance` — read-only. That is the lite profile, not a bug and
not a missing installation: the other 17 tools are opt-in so the always-loaded
`tools/list` payload stays small.

`--profile lite|standard|full|admin` is a **preset** over the existing
configuration. It sets flags and a grant tier; it does not introduce a second
authorization system, and every preset is expressible as ordinary grant TOML
(asserted by `crates/strut-sql-core/src/profiles.rs` tests).

| Profile | Write mode | `--full-tools` | `--allow-grants` | Approvals | Tools |
|---|---|---|---|---|---|
| *(none — default)* / `lite` | read-only | no | no | no | 3 core |
| `standard` | read-only | yes | no | no | 13 (core + introspection) |
| `full` | writes | yes | no | no | 13 + writes on core |
| `admin` | ddl | yes | yes | required | 20 (core + introspection + DDL) |

`lite` and "no flag at all" are the same surface. Everything above `lite` is
reachable with explicit flags (`--full-tools`, `--allow-writes`, `--allow-ddl`)
or with one preset.

Equivalent grant for a principal:

```toml
[[grants]]
principal = "<principal>"
source = "*"
max_tier = "read"   # lite / standard; "write" for full; "ddl" for admin
```

Rules:

- Explicit flags may **raise** what a profile sets (`--profile standard
  --allow-writes` is valid and ends at writes). Nothing requires lowering.
- `admin` opens the Admin gate **and** turns on destructive-DDL approvals; the
  `confirm: true` flag remains a separate legacy confirmation and is never an
  approval.
- A profile never widens a grant beyond the process ceiling, and per-principal
  grants in `[[grants]]` still apply — a profile only chooses the defaults for
  the process.
- Unknown profile names fail at startup; there is no silent fallback.
