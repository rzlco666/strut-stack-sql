# Tool profiles

`--profile lite|standard|full|admin` is a **preset** over the existing
configuration. It sets flags and a grant tier; it does not introduce a second
authorization system, and every preset is expressible as ordinary grant TOML
(asserted by `crates/strut-sql-core/src/profiles.rs` tests).

| Profile | Write mode | `--full-tools` | `--allow-grants` | Approvals |
|---|---|---|---|---|
| `lite` | read-only | no | no | no |
| `standard` | read-only | yes | no | no |
| `full` | writes | yes | no | no |
| `admin` | ddl | yes | yes | required |

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
