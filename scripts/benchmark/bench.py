#!/usr/bin/env python3
"""Reproducible benchmark harness for Strut Stack SQL (PRD-F2-12/F2-13).

Measures the released binary over stdio and Streamable HTTP on one machine:
  * cold start (spawn -> initialize response)
  * tools/list payload bytes (core tool mode)
  * 100 rows x 8 columns result bytes (before truncation)
  * execute_sql SELECT-by-PK latency p50/p95/p99 (n >= 100 after warmup)
  * RSS idle and under load

Writes raw results with machine specs to scripts/benchmark/results/<release>.json.

Usage:
  python3 scripts/benchmark/bench.py --binary target/release/mcp-sql-rust \
      --release 2.0.0 --out scripts/benchmark/results/2.0.0.json
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import queue
import re
import socket
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

PROTOCOL_VERSION = "2025-03-26"
RESULT_100x8_TARGET = 70 * 1024
TOOLS_LIST_TARGET = 3 * 1024
RSS_IDLE_TARGET_MB = 15.0
COLD_START_P95_TARGET_MS = 25.0
EXEC_P50_TARGET_MS = 1.0


# ---------------------------------------------------------------------------
# Machine + process helpers
# ---------------------------------------------------------------------------


def read_text(path: str) -> str:
    try:
        return Path(path).read_text(errors="replace")
    except OSError:
        return ""


def machine_specs() -> dict:
    cpu_model = ""
    for line in read_text("/proc/cpuinfo").splitlines():
        if line.lower().startswith("model name"):
            cpu_model = line.split(":", 1)[1].strip()
            break
    mem_kb = 0
    for line in read_text("/proc/meminfo").splitlines():
        if line.startswith("MemTotal:"):
            mem_kb = int(line.split()[1])
            break
    os_release = {}
    for line in read_text("/etc/os-release").splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            os_release[key] = value.strip().strip('"')
    return {
        "hostname": socket.gethostname(),
        "kernel": platform.release(),
        "os": os_release.get("PRETTY_NAME", platform.platform()),
        "cpu_model": cpu_model,
        "cpu_count": os.cpu_count(),
        "mem_total_kb": mem_kb,
        "mem_total_gb": round(mem_kb / 1024 / 1024, 2),
        "python": platform.python_version(),
        "reference_machine": "documented WSL2 reference: 6 vCPU, ~5 GB RAM",
        "is_wsl": "microsoft" in platform.release().lower(),
    }


def rss_mb(pid: int) -> float | None:
    status = read_text(f"/proc/{pid}/status")
    match = re.search(r"^VmRSS:\s+(\d+)\s+kB", status, re.MULTILINE)
    if not match:
        return None
    return round(int(match.group(1)) / 1024, 2)


def sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def percentile(values: list[float], pct: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(pct / 100.0 * len(ordered)))
    return round(ordered[rank - 1], 3)


def summarize(values: list[float]) -> dict:
    return {
        "n": len(values),
        "p50": percentile(values, 50),
        "p95": percentile(values, 95),
        "p99": percentile(values, 99),
        "mean": round(statistics.fmean(values), 3) if values else None,
        "min": round(min(values), 3) if values else None,
        "max": round(max(values), 3) if values else None,
    }


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for_port(port: int, timeout: float = 10.0) -> None:
    deadline = time.perf_counter() + timeout
    while time.perf_counter() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.01)
    raise TimeoutError(f"server did not listen on 127.0.0.1:{port}")


# ---------------------------------------------------------------------------
# Database fixture
# ---------------------------------------------------------------------------


def seed_database(path: Path) -> None:
    if path.exists():
        path.unlink()
    conn = sqlite3.connect(path)
    try:
        conn.execute(
            "CREATE TABLE bench_users ("
            "id INTEGER PRIMARY KEY, email TEXT, name TEXT, status TEXT, "
            "created_at TEXT, score REAL, region TEXT, notes TEXT)"
        )
        conn.executemany(
            "INSERT INTO bench_users VALUES (?,?,?,?,?,?,?,?)",
            [
                (
                    i,
                    f"user{i}@example.com",
                    f"User {i}",
                    "active" if i % 3 else "idle",
                    f"2026-01-{(i % 28) + 1:02d}T12:00:00",
                    round((i % 1000) / 10.0, 2),
                    ["us-east", "eu-west", "ap-south"][i % 3],
                    f"benchmark row {i} notes payload",
                )
                for i in range(1, 1001)
            ],
        )
        conn.execute(
            "CREATE TABLE bench_wide (c1 TEXT, c2 TEXT, c3 TEXT, c4 TEXT, "
            "c5 TEXT, c6 TEXT, c7 TEXT, c8 TEXT)"
        )
        conn.executemany(
            "INSERT INTO bench_wide VALUES (?,?,?,?,?,?,?,?)",
            [
                tuple(f"r{i:03d}c{j}" for j in range(1, 9))
                for i in range(1, 101)
            ],
        )
        conn.commit()
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# MCP clients
# ---------------------------------------------------------------------------


class LineReader(threading.Thread):
    def __init__(self, stream):
        super().__init__(daemon=True)
        self.stream = stream
        self.lines: queue.Queue[str | None] = queue.Queue()

    def run(self) -> None:
        for line in self.stream:
            self.lines.put(line)
        self.lines.put(None)


class StdioClient:
    def __init__(self, command: list[str], cwd: str | None = None):
        self.proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
            cwd=cwd,
        )
        assert self.proc.stdout is not None and self.proc.stdin is not None
        self.reader = LineReader(self.proc.stdout)
        self.reader.start()
        self.next_id = 1

    @property
    def pid(self) -> int:
        return self.proc.pid

    def send(self, payload: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
        self.proc.stdin.flush()

    def request(self, method: str, params: dict | None = None, timeout: float = 60.0):
        request_id = self.next_id
        self.next_id += 1
        self.send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params or {},
            }
        )
        deadline = time.perf_counter() + timeout
        while True:
            remaining = deadline - time.perf_counter()
            if remaining <= 0:
                raise TimeoutError(f"no response to {method}")
            try:
                line = self.reader.lines.get(timeout=remaining)
            except queue.Empty as exc:
                raise TimeoutError(f"no response to {method}") from exc
            if line is None:
                raise RuntimeError("server closed stdout")
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("id") == request_id:
                if "error" in message:
                    raise RuntimeError(json.dumps(message["error"]))
                return message.get("result")

    def notify(self, method: str, params: dict | None = None) -> None:
        self.send({"jsonrpc": "2.0", "method": method, "params": params or {}})

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
            self.proc.wait(timeout=5)


class HttpMcpClient:
    def __init__(self, url: str, token: str | None):
        self.url = url
        self.token = token
        self.session_id: str | None = None

    def _headers(self) -> dict:
        headers = {
            "content-type": "application/json",
            "accept": "application/json, text/event-stream",
        }
        if self.token:
            headers["authorization"] = f"Bearer {self.token}"
        if self.session_id:
            headers["mcp-session-id"] = self.session_id
        return headers

    def _post(self, payload: dict, timeout: float = 60.0):
        data = json.dumps(payload, separators=(",", ":")).encode()
        request = urllib.request.Request(
            self.url, data=data, headers=self._headers(), method="POST"
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                body = response.read().decode()
                return response.status, dict(response.headers), body
        except urllib.error.HTTPError as exc:
            return exc.code, dict(exc.headers), exc.read().decode()

    @staticmethod
    def _parse_body(body: str):
        body = body.strip()
        if not body:
            return None
        if body.startswith("{"):
            try:
                return json.loads(body)
            except json.JSONDecodeError:
                return None
        payload = []
        for line in body.splitlines():
            if line.startswith("data:"):
                payload.append(line[5:].strip())
        if not payload:
            return None
        try:
            return json.loads("\n".join(payload))
        except json.JSONDecodeError:
            return None

    def initialize(self):
        request_id = 1
        status, headers, body = self._post(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "strut-bench", "version": "1.0.0"},
                },
            }
        )
        self.session_id = headers.get("mcp-session-id") or headers.get("Mcp-Session-Id")
        if status != 200:
            raise RuntimeError(f"HTTP initialize failed: {status} {body[:400]}")
        message = self._parse_body(body)
        if not message:
            raise RuntimeError("HTTP initialize returned no JSON-RPC body")
        if "error" in message:
            raise RuntimeError(json.dumps(message["error"]))
        # notifications/initialized
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return message.get("result")

    def request(self, method: str, params: dict | None = None):
        request_id = 2 if method == "tools/list" else self.next_id()
        status, _headers, body = self._post(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params or {},
            }
        )
        if status != 200:
            raise RuntimeError(f"HTTP {method} failed: {status} {body[:400]}")
        message = self._parse_body(body)
        if not message:
            raise RuntimeError(f"HTTP {method} returned no JSON-RPC body")
        if "error" in message:
            raise RuntimeError(json.dumps(message["error"]))
        return message.get("result")

    def next_id(self) -> int:
        self._id = getattr(self, "_id", 100) + 1
        return self._id


def mcp_call(client, tool: str, arguments: dict):
    return client.request(
        "tools/call", {"name": tool, "arguments": arguments}
    )


def result_text_bytes(result: dict) -> int:
    content = result.get("content") or []
    if not content:
        return 0
    text = content[0].get("text", "")
    return len(text.encode())


# ---------------------------------------------------------------------------
# Measurements
# ---------------------------------------------------------------------------


def server_command(binary: str, db: Path, extra: list[str]) -> list[str]:
    return [
        binary,
        "--url",
        f"sqlite://{db}",
        "--max-rows",
        "100",
        "--max-bytes",
        "20000000",
        *extra,
    ]


def cold_start_stdio(binary: str, db: Path, count: int) -> list[float]:
    samples = []
    for _ in range(count):
        client = StdioClient(server_command(binary, db, []))
        try:
            started = time.perf_counter()
            client.request(
                "initialize",
                {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "strut-bench", "version": "1.0.0"},
                },
            )
            samples.append((time.perf_counter() - started) * 1000)
        finally:
            client.close()
    return samples


def cold_start_http(binary: str, db: Path, count: int) -> list[float]:
    samples = []
    for _ in range(count):
        port = free_port()
        proc = subprocess.Popen(
            server_command(
                binary,
                db,
                ["--http", f"127.0.0.1:{port}", "--http-token", "bench"],
            ),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            started = time.perf_counter()
            wait_for_port(port)
            client = HttpMcpClient(f"http://127.0.0.1:{port}/mcp", "bench")
            client.initialize()
            samples.append((time.perf_counter() - started) * 1000)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
    return samples


def measure_transport(binary: str, db: Path, transport: str, iterations: int, warmup: int) -> dict:
    if transport == "stdio":
        client = StdioClient(server_command(binary, db, []))
        pid = client.pid
        started = time.perf_counter()
        client.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "strut-bench", "version": "1.0.0"},
            },
        )
        cold_ms = (time.perf_counter() - started) * 1000
        client.notify("notifications/initialized")
        process = client.proc
    else:
        port = free_port()
        process = subprocess.Popen(
            server_command(
                binary,
                db,
                ["--http", f"127.0.0.1:{port}", "--http-token", "bench"],
            ),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        pid = process.pid
        started = time.perf_counter()
        wait_for_port(port)
        client = HttpMcpClient(f"http://127.0.0.1:{port}/mcp", "bench")
        client.initialize()
        cold_ms = (time.perf_counter() - started) * 1000

    try:
        tools = client.request("tools/list")
        tools_list_bytes = len(json.dumps(tools, separators=(",", ":")).encode())
        tool_names = [tool.get("name") for tool in tools.get("tools", [])]

        time.sleep(1.0)
        idle_rss = rss_mb(pid)

        # 100 x 8 result size (before truncation; max-bytes is raised).
        wide = mcp_call(
            client,
            "execute_sql",
            {
                "sql": "SELECT c1, c2, c3, c4, c5, c6, c7, c8 "
                "FROM bench_wide ORDER BY rowid LIMIT 100"
            },
        )
        wide_bytes = result_text_bytes(wide)
        wide_payload = json.loads(wide["content"][0]["text"])
        wide_meta = wide_payload.get("data", {}).get("meta", {})
        wide_rows = len(wide_payload.get("data", {}).get("rows", []))
        wide_cols = len(wide_payload.get("data", {}).get("cols", []))

        select_sql = (
            "SELECT id, email, name, status, created_at, score, region, notes "
            "FROM bench_users WHERE id = ?"
        )
        for i in range(warmup):
            mcp_call(client, "execute_sql", {"sql": select_sql, "params": [(i % 1000) + 1]})

        latencies = []
        errors = 0
        for i in range(iterations):
            started = time.perf_counter()
            try:
                payload = mcp_call(
                    client,
                    "execute_sql",
                    {"sql": select_sql, "params": [(i % 1000) + 1]},
                )
                body = json.loads(payload["content"][0]["text"])
                if not body.get("ok"):
                    errors += 1
            except Exception:
                errors += 1
            latencies.append((time.perf_counter() - started) * 1000)

        load_rss = rss_mb(pid)
        return {
            "transport": transport,
            "cold_start_single_ms": round(cold_ms, 3),
            "tools_list_bytes": tools_list_bytes,
            "tools_list_tool_count": len(tool_names),
            "result_100x8_bytes": wide_bytes,
            "result_100x8_rows": wide_rows,
            "result_100x8_cols": wide_cols,
            "result_100x8_truncated": bool(wide_meta.get("truncated")),
            "execute_sql_select_by_pk_ms": summarize(latencies),
            "execute_sql_errors": errors,
            "idle_rss_mb": idle_rss,
            "load_rss_mb": load_rss,
        }
    finally:
        if transport == "stdio":
            client.close()
        else:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def compare(results: dict, cold_samples: dict) -> dict:
    comparison = {}
    for transport, data in results.items():
        cold = summarize(cold_samples[transport])
        checks = {
            "idle_rss_mb": (
                data["idle_rss_mb"],
                RSS_IDLE_TARGET_MB,
                "MB",
                "le",
            ),
            "cold_start_p95_ms": (
                cold["p95"],
                COLD_START_P95_TARGET_MS,
                "ms",
                "le",
            ),
            "tools_list_bytes": (
                data["tools_list_bytes"],
                TOOLS_LIST_TARGET,
                "bytes",
                "le",
            ),
            "result_100x8_bytes": (
                data["result_100x8_bytes"],
                RESULT_100x8_TARGET,
                "bytes",
                "le",
            ),
            "execute_sql_p50_ms": (
                data["execute_sql_select_by_pk_ms"]["p50"],
                EXEC_P50_TARGET_MS,
                "ms",
                "le",
            ),
        }
        transport_comparison = {}
        for metric, (value, target, unit, direction) in checks.items():
            status = "not_measured"
            if value is not None:
                ok = value <= target if direction == "le" else value >= target
                status = "PASS" if ok else "MISS"
            transport_comparison[metric] = {
                "value": value,
                "target": target,
                "unit": unit,
                "status": status,
            }
        comparison[transport] = transport_comparison
    return comparison


def competitor_status() -> dict:
    has_npx = subprocess.run(
        ["bash", "-lc", "command -v npx"], capture_output=True, text=True
    ).returncode == 0
    if not has_npx:
        return {
            "status": "not_run",
            "reason": "node/npx is not installed on this machine; the pinned "
            "competitor packages require npx and a live PostgreSQL server "
            "(none available). Not attempted.",
        }
    return {
        "status": "not_run",
        "reason": "npx exists but no live PostgreSQL/MySQL server is available "
        "for the pinned competitors; not attempted within the 30-minute budget.",
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/mcp-sql-rust")
    parser.add_argument("--release", default="2.0.0")
    parser.add_argument("--out", default=None)
    parser.add_argument("--iterations", type=int, default=120)
    parser.add_argument("--warmup", type=int, default=20)
    parser.add_argument("--cold-runs", type=int, default=20)
    args = parser.parse_args()

    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2
    out = Path(args.out) if args.out else Path(__file__).parent / "results" / f"{args.release}.json"
    out.parent.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="strut-bench-") as tmp:
        db = Path(tmp) / "bench.db"
        seed_database(db)

        print("measuring cold start (stdio)...", file=sys.stderr)
        cold_stdio = cold_start_stdio(binary, db, args.cold_runs)
        print("measuring cold start (http)...", file=sys.stderr)
        cold_http = cold_start_http(binary, db, args.cold_runs)

        print("measuring stdio...", file=sys.stderr)
        results = {"stdio": measure_transport(binary, db, "stdio", args.iterations, args.warmup)}
        print("measuring http...", file=sys.stderr)
        results["http"] = measure_transport(binary, db, "http", args.iterations, args.warmup)

    cold_samples = {"stdio": cold_stdio, "http": cold_http}
    comparison = compare(results, cold_samples)

    payload = {
        "meta": {
            "release": args.release,
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "binary": binary,
            "binary_sha256": sha256_file(binary),
            "database": "sqlite (temp file, seeded 1000 users + 100x8 table)",
            "iterations": args.iterations,
            "warmup": args.warmup,
            "cold_start_runs": args.cold_runs,
            "protocol_version": PROTOCOL_VERSION,
            "machine": machine_specs(),
            "notes": [
                "execute_sql latency includes local SQLite execution and JSON-RPC framing; "
                "the PRD target is tool overhead outside DB time, which this harness cannot "
                "separate on an in-process SQLite file.",
                "cold start = process spawn + MCP initialize response (HTTP includes listening "
                "socket readiness).",
                "tools/list bytes = JSON-RPC result payload in core tool mode.",
                "result_100x8_bytes = tools/call text payload before truncation "
                "(--max-bytes raised to 20 MB).",
            ],
        },
        "cold_start_samples_ms": {
            transport: summarize(samples) for transport, samples in cold_samples.items()
        },
        "results": results,
        "targets": {
            "idle_rss_mb": RSS_IDLE_TARGET_MB,
            "cold_start_p95_ms": COLD_START_P95_TARGET_MS,
            "tools_list_bytes": TOOLS_LIST_TARGET,
            "result_100x8_bytes": RESULT_100x8_TARGET,
            "execute_sql_p50_ms": EXEC_P50_TARGET_MS,
        },
        "comparison": comparison,
        "engines": {
            "sqlite": "measured (both transports)",
            "postgres": "not_run: no live PostgreSQL server available on this machine",
            "mysql": "not_run: no live MySQL server available on this machine",
        },
        "competitor": competitor_status(),
    }

    out.write_text(json.dumps(payload, indent=2) + "\n")
    print(json.dumps(payload, indent=2))
    print(f"\nWrote {out}", file=sys.stderr)

    print("\nTarget comparison (value / target):", file=sys.stderr)
    for transport, checks in comparison.items():
        for metric, row in checks.items():
            print(
                f"  {transport:5s} {metric:24s} {str(row['value']):>10s} / "
                f"{row['target']} {row['unit']:5s} {row['status']}",
                file=sys.stderr,
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
