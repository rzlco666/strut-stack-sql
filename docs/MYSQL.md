# MySQL notes — strut-stack-sql

MySQL support is first-class alongside PostgreSQL and SQLite.

## Docker Compose demo

```bash
docker compose up -d mysql
```

```env
DATABASE_URL=mysql://demo:demo@localhost:3307/demo
```

Seed data: [`docker/seed/mysql.sql`](../docker/seed/mysql.sql) — `users` (200), `products` (50), `orders` (1000).

## Schema = database

In MySQL, the `schema` tool parameter is the **database name** (`table_schema` in `information_schema`).

When `schema` is omitted, the server resolves the current database via `SELECT DATABASE()` or the database name in `DATABASE_URL`. Use `schema: "*"` only to search all databases on the server.

## Type fidelity

`information_schema` aggregates (`SUM(table_rows)`, `ROUND(...)`, `COUNT(*)`) return JSON **numbers** without `CAST(... AS CHAR)` workarounds. `DECIMAL` columns decode via `bigdecimal`.

### Strings vs binary

- Character types (`VARCHAR`, `CHAR`, `TEXT`, `ENUM`, `SET`, `JSON`) → UTF-8 JSON strings in `execute_sql` (columnar and `format: rows`).
- Binary types (`BLOB`, `BINARY`, `VARBINARY`, `BIT`) → hex strings (lowercase, no `0x` prefix).
- Schema tools (`describe_table`, `search_objects`, `list_foreign_keys`) already returned readable text; execute path now matches.

## `describe_table`

- Pass `table` as the bare table name (`users`), not `database.users` in the `table` field (qualified `db.table` is accepted and split).
- Backticks in names are stripped.
- Column metadata from `information_schema.columns`, with `SHOW FULL COLUMNS` fallback.

## Placeholders

MySQL uses `?` placeholders. PostgreSQL `$N` in SQL text is rewritten when the source engine is MySQL-bound via the guard.

## Columnar JSON / `COUNT(*)`

MySQL may map small integers to boolean in some drivers. strut-stack-sql decodes **numeric types before boolean** so `COUNT(*)` returns numbers, not booleans.

## `analyze_query_performance`

MySQL `EXPLAIN FORMAT=JSON` differs from PostgreSQL:

- `total_cost` and `plan_rows` are **best-effort**
- Sequential scan warnings (`access_type: ALL`) still apply
- Supports `params` for parameterized EXPLAIN

## Read-only sessions

Without `--allow-writes`: `SET SESSION TRANSACTION READ ONLY` on pool checkout (plus AST guard).

## Integration tests

```bash
MYSQL_DATABASE_URL='mysql://demo:demo@localhost:3307/demo' \
  cargo test --test mysql_integration -- --ignored
```

Or with compose:

```bash
docker compose up -d mysql
MYSQL_DATABASE_URL=mysql://demo:demo@127.0.0.1:3307/demo cargo test --test mysql_integration -- --ignored
```
