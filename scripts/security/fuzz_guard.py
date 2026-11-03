#!/usr/bin/env python3
"""Guard fuzz gate (R1-06): smuggled writes must be denied, never executed.

The server under test runs in the default read-only *process* mode against a
writable SQLite source, so a guard bypass would actually mutate the database
instead of failing on a read-only connection. That makes every `ok: true` on a
write-shaped statement a real bypass, and it lets a database digest prove
whether anything slipped through.

Generates write attempts from a grammar (CTE writes, SELECT INTO, PRAGMA
writes, keyword comment-splitting, unicode/whitespace variants, multi-statement
smuggling, dangerous functions) and records:

  * attempts, denied, bypasses (`ok: true` on a write attempt)
  * crashes (server process died, no valid JSON-RPC response)
  * database digest before/after (rows + table list + file sha256)
  * stray files created next to the database (INTO OUTFILE / ATTACH targets)

Usage:
  scripts/security/fuzz_guard.py --seconds 3600 --out /tmp/guard-fuzz.json
  scripts/security/fuzz_guard.py --seconds 60 --seed 7   # smoke

Exit code 0 only when bypasses == 0, crashes == 0, and the digest is unchanged.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PROTOCOL_VERSION = "2025-06-18"

WRITE_VERBS = [
    "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "TRUNCATE", "CREATE",
    "REPLACE", "MERGE", "GRANT", "REVOKE", "VACUUM", "ATTACH", "PRAGMA",
]

TEMPLATES = [
    # Direct write statements.
    "{verb} INTO fuzz_target VALUES (1)",
    "{verb} fuzz_target SET name = 'x'",
    "{verb} TABLE fuzz_target",
    "{verb} TABLE IF EXISTS fuzz_target",
    "{verb} INDEX idx_fuzz ON fuzz_target(name)",
    # CTE smuggling.
    "WITH w AS ({verb} INTO fuzz_target VALUES (1)) SELECT 1 FROM fuzz_target",
    "WITH w AS (DELETE FROM fuzz_target RETURNING *) SELECT * FROM w",
    "WITH RECURSIVE w(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM w WHERE n < 3) "
    "DELETE FROM fuzz_target WHERE id IN (SELECT n FROM w)",
    "WITH a AS (SELECT 1), b AS (UPDATE fuzz_target SET name = 'x' RETURNING id) "
    "SELECT * FROM b",
    # SELECT ... INTO / file writes.
    "SELECT * INTO fuzz_copy FROM fuzz_target",
    "SELECT 'x' INTO OUTFILE '/tmp/fuzz-outfile.txt'",
    "SELECT 'x' INTO DUMPFILE '/tmp/fuzz-dumpfile.txt'",
    # Multi-statement smuggling.
    "SELECT 1; {verb} INTO fuzz_target VALUES (1)",
    "SELECT 1; DROP TABLE fuzz_target",
    # Dangerous functions inside innocent-looking reads.
    "SELECT load_extension('/tmp/evil.so')",
    "SELECT writefile('/tmp/fuzz-writefile.txt', 'x')",
    "SELECT readfile('/etc/passwd')",
    "SELECT pg_read_file('/etc/passwd')",
    "SELECT nextval('fuzz_seq')",
    "SELECT * FROM fuzz_target WHERE id = (SELECT 1 FROM (SELECT 1) x); DELETE FROM fuzz_target",
    # Transaction / session control.
    "BEGIN; DELETE FROM fuzz_target; COMMIT",
    "PRAGMA writable_schema = 1",
    "PRAGMA journal_mode = DELETE",
    # Statement-class confusion.
    "EXPLAIN ANALYZE DELETE FROM fuzz_target",
    "",
    "   ",
    "/* only a comment */",
]


def mutate(rng: random.Random, sql: str) -> str:
    """Whitespace / comment / case mutation around the write keyword."""
    if "{verb}" in sql:
        verb = rng.choice(WRITE_VERBS)
        form = rng.random()
        if form < 0.2:
            verb = verb.lower()
        elif form < 0.4 and len(verb) > 2:
            verb = f"{verb[:2]}/**/{verb[2:]}"
        sql = sql.replace("{verb}", verb)
    if rng.random() < 0.3:
        sql = sql.replace(" ", rng.choice(["\t", "\n", "  ", "/**/", "\u00a0"]))
    if rng.random() < 0.2:
        sql = sql + rng.choice([";", " ;", "\n;", "-- trailing", "/* tail */"])
    if rng.random() < 0.1:
        sql = "\ufeff" + sql
    if rng.random() < 0.1:
        sql = sql.replace("SELECT", "SeLeCt")
    return sql


class Server:
    def __init__(self, binary: str, db: Path):
        self.proc = subprocess.Popen(
            [
                binary,
                "--url",
                f"sqlite://{db}",
                "--max-rows",
                "100",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        self.next_id = 1
        self.initialize()

    def request(self, method: str, params: dict | None = None, timeout: float = 20.0):
        request_id = self.next_id
        self.next_id += 1
        assert self.proc.stdin is not None
        self.proc.stdin.write(
            json.dumps(
                {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params or {}},
                separators=(",", ":"),
            )
            + "\n"
        )
        self.proc.stdin.flush()
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = self.proc.stdout.readline() if self.proc.stdout else ""
            if not line:
                raise RuntimeError("server closed stdout")
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                raise RuntimeError(f"non-JSON output: {line[:200]}")
            if message.get("id") == request_id:
                return message
        raise TimeoutError(method)

    def initialize(self) -> None:
        self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "guard-fuzz", "version": "1.0.0"},
            },
        )
        assert self.proc.stdin is not None
        self.proc.stdin.write(
            json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n"
        )
        self.proc.stdin.flush()

    def alive(self) -> bool:
        return self.proc.poll() is None

    def close(self) -> None:
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
        except OSError:
            pass
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def digest(db: Path, root: Path) -> dict:
    conn = sqlite3.connect(db)
    try:
        tables = [
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
            )
        ]
        rows = {name: conn.execute(f'SELECT COUNT(*) FROM "{name}"').fetchone()[0] for name in tables}
        data = conn.execute("SELECT id, name FROM fuzz_target ORDER BY id").fetchall()
    finally:
        conn.close()
    files = sorted(
        p.name for p in root.iterdir() if p.is_file() and p.name != db.name
    )
    return {
        "tables": tables,
        "rows": rows,
        "fuzz_target": data,
        "stray_files": files,
        "sha256": hashlib.sha256(db.read_bytes()).hexdigest(),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/strut-stack-sql")
    parser.add_argument("--seconds", type=int, default=3600)
    parser.add_argument("--seed", type=int, default=20261103)
    parser.add_argument("--out", default=None)
    parser.add_argument("--max-attempts", type=int, default=0, help="0 = unlimited until --seconds")
    args = parser.parse_args()

    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    rng = random.Random(args.seed)
    started_at = time.time()
    with tempfile.TemporaryDirectory(prefix="guard-fuzz-") as tmp:
        root = Path(tmp)
        db = root / "fuzz.db"
        conn = sqlite3.connect(db)
        conn.execute("CREATE TABLE fuzz_target (id INTEGER PRIMARY KEY, name TEXT)")
        conn.execute("INSERT INTO fuzz_target VALUES (1, 'original')")
        conn.commit()
        conn.close()

        before = digest(db, root)
        server = Server(binary, db)
        attempts = denied = bypasses = 0
        errors = 0
        crashes: list[str] = []
        bypass_samples: list[dict] = []
        deadline = started_at + args.seconds
        try:
            while time.time() < deadline:
                if args.max_attempts and attempts >= args.max_attempts:
                    break
                sql = mutate(rng, rng.choice(TEMPLATES))
                params = None
                if rng.random() < 0.15:
                    sql = sql.replace("VALUES (1)", "VALUES (?)", 1)
                    params = [rng.choice([1, "x", None])]
                attempts += 1
                try:
                    message = server.request(
                        "tools/call",
                        {"name": "execute_sql", "arguments": {"sql": sql, "params": params}},
                    )
                except Exception as exc:  # crash, timeout, protocol corruption
                    crashes.append(f"{type(exc).__name__}: {exc}")
                    if not server.alive():
                        break
                    continue
                if not server.alive():
                    crashes.append("process exited after a response")
                    break
                result = message.get("result") or {}
                content = result.get("content") or []
                if message.get("error") or not content:
                    denied += 1
                    continue
                text = content[0].get("text", "")
                try:
                    body = json.loads(text)
                except json.JSONDecodeError:
                    errors += 1
                    continue
                if body.get("ok") is True:
                    bypasses += 1
                    if len(bypass_samples) < 20:
                        bypass_samples.append({"sql": sql, "params": params, "body": body})
                else:
                    denied += 1
        finally:
            server.close()

        after = digest(db, root)

    elapsed = time.time() - started_at
    payload = {
        "gate": "R1-06 guard fuzz",
        "binary": binary,
        "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(started_at)),
        "seconds_requested": args.seconds,
        "seconds_elapsed": round(elapsed, 1),
        "seed": args.seed,
        "attempts": attempts,
        "denied": denied,
        "bypasses": bypasses,
        "protocol_errors": errors,
        "crashes": crashes,
        "db_before": before,
        "db_after": after,
        "db_unchanged": before == after,
        "bypass_samples": bypass_samples,
        "result": "PASS"
        if bypasses == 0 and not crashes and errors == 0 and before == after
        else "FAIL",
    }
    text = json.dumps(payload, indent=2) + "\n"
    if args.out:
        Path(args.out).write_text(text)
    print(text)
    return 0 if payload["result"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
