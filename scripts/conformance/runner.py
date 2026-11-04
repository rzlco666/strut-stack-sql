#!/usr/bin/env python3
"""Self-contained MCP conformance runner for strut-stack-sql.

Python 3 standard library only. Exercises the release binary over stdio and
Streamable HTTP, writes a machine-readable JSON report, prints a human
summary, and exits non-zero when a required check fails.

Every check carries the spec / PRD clause it covers. Checks that target
optional or client-side behaviour are marked ``optional`` and never fail the
run; their observed result is still reported.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import queue
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable
from urllib.parse import urlsplit

SUPPORTED_VERSION = "2025-06-18"
SPEC_BASE = "https://modelcontextprotocol.io/specification/2025-06-18"
PRD_CONF = "PRD-V2 §6.1 (PRD-CONF-01/02/03)"

CORE_TOOLS = {"search_objects", "execute_sql", "analyze_query_performance"}
# tools/list payload caps (R1-05): the default surface must stay under the
# PRD-F2-12 3,072-byte budget; the full 20-tool surface has its own cap so
# description creep fails CI instead of being discovered by clients.
CORE_TOOLS_LIST_BYTES_CAP = 3072
FULL_TOOLS_LIST_BYTES_CAP = 12000
FULL_TOOLS = {
    "add_column",
    "alter_column",
    "analyze_query_performance",
    "create_table",
    "describe_table",
    "drop_column",
    "drop_table",
    "execute_sql",
    "get_relationship_graph",
    "get_schema_context",
    "list_foreign_keys",
    "list_indexes",
    "list_schemas",
    "list_sources",
    "list_tables",
    "schema_mutate",
    "schema_overview",
    "search_objects",
    "suggest_joins",
    "truncate_table",
}

# Minimal RSA public key for the fake authorization server. Signature
# validation is not exercised here (invalid tokens are rejected before
# signature checks); the JWKS only has to parse and be non-empty.
FAKE_JWK = {
    "kty": "RSA",
    "kid": "conformance-key",
    "use": "sig",
    "alg": "RS256",
    "n": (
        "_FWn0dY0gI3Kav7VCpsG8sywVTbgOqpj6qmZ9zj7zWne8X_KC-UZxslggqQvuglWvkj2FBeN9e638A0qECIMSL6kPp__i20xNrA0VDyuOw7eA77cqUXtcCxxIbbM-AnEvgIV27NT51yc2sqKlnFA-ac9HjX4oZ0CazB_2hJq2KrXyMOSvSLr_bM-ouyOfVGDu9BEQOuZJ9DR9KDLYHGv9CW-_Q1FNYH70qwCu4m4pS_uz2n69TZLDOkC3h-CODOwkg4zq6z25f26k8Hn10B5j_o2-RoalZfx_7tBHyCrQ7dgNAEtpoqYAWQD49FRkeBi1UoojaqwLWzaX9VXL1z09Q"
    ),
    "e": "AQAB",
}


# ---------------------------------------------------------------------------
# Results
# ---------------------------------------------------------------------------


@dataclass
class Check:
    id: str
    group: str
    transport: str
    description: str
    clause: str
    status: str  # pass | fail | optional | not_run
    optional: bool = False
    detail: dict[str, Any] = field(default_factory=dict)
    error: str | None = None


class Results:
    def __init__(self) -> None:
        self.checks: list[Check] = []

    def record(
        self,
        check_id: str,
        group: str,
        transport: str,
        description: str,
        clause: str,
        ok: bool | None,
        *,
        optional: bool = False,
        detail: dict[str, Any] | None = None,
        error: str | None = None,
    ) -> None:
        if ok is None:
            status = "not_run"
        elif ok:
            status = "pass"
        elif optional:
            status = "optional"
        else:
            status = "fail"
        self.checks.append(
            Check(
                id=check_id,
                group=group,
                transport=transport,
                description=description,
                clause=clause,
                status=status,
                optional=optional,
                detail=detail or {},
                error=error,
            )
        )

    def ok(self, *args: Any, **kwargs: Any) -> None:
        if len(args) >= 6 or "ok" in kwargs:
            self.record(*args, **kwargs)
        else:
            self.record(*args, ok=True, **kwargs)

    def fail(self, *args: Any, **kwargs: Any) -> None:
        if len(args) >= 6 or "ok" in kwargs:
            self.record(*args, **kwargs)
        else:
            self.record(*args, ok=False, **kwargs)

    def not_run(self, *args: Any, **kwargs: Any) -> None:
        if len(args) >= 6 or "ok" in kwargs:
            self.record(*args, **kwargs)
        else:
            self.record(*args, ok=None, **kwargs)

    def counts(self) -> dict[str, int]:
        counts = {"pass": 0, "fail": 0, "optional": 0, "not_run": 0}
        for check in self.checks:
            counts[check.status] += 1
        return counts

    def failed_required(self) -> list[Check]:
        return [c for c in self.checks if c.status == "fail"]


# ---------------------------------------------------------------------------
# JSON Schema validation (enough for MCP tool inputSchema)
# ---------------------------------------------------------------------------

JSON_TYPES = {"string", "number", "integer", "boolean", "object", "array", "null"}


def schema_errors(schema: Any, path: str = "inputSchema") -> list[str]:
    errors: list[str] = []
    if not isinstance(schema, dict):
        return [f"{path}: must be a JSON object"]
    schema_type = schema.get("type")
    if schema_type is None:
        if not any(k in schema for k in ("anyOf", "oneOf", "allOf", "$ref", "enum", "const")):
            errors.append(f"{path}: missing 'type'")
    elif isinstance(schema_type, str):
        if schema_type not in JSON_TYPES:
            errors.append(f"{path}.type: invalid value {schema_type!r}")
    elif isinstance(schema_type, list):
        if not schema_type or any(t not in JSON_TYPES for t in schema_type):
            errors.append(f"{path}.type: invalid type list {schema_type!r}")
    else:
        errors.append(f"{path}.type: must be a string or array")

    props = schema.get("properties")
    if props is not None:
        if not isinstance(props, dict):
            errors.append(f"{path}.properties: must be an object")
        else:
            for name, prop in props.items():
                if not isinstance(name, str) or not name:
                    errors.append(f"{path}.properties: empty/non-string property name")
                errors.extend(schema_errors(prop, f"{path}.properties.{name}"))
            required = schema.get("required", [])
            if required is not None:
                if not isinstance(required, list) or any(
                    not isinstance(v, str) for v in required
                ):
                    errors.append(f"{path}.required: must be an array of strings")
                else:
                    missing = [name for name in required if name not in props]
                    if missing:
                        errors.append(
                            f"{path}.required: names not in properties: {missing}"
                        )

    for key in ("items", "additionalProperties"):
        value = schema.get(key)
        if value is not None and not isinstance(value, (dict, bool)):
            errors.append(f"{path}.{key}: must be an object or boolean")
        elif isinstance(value, dict):
            errors.extend(schema_errors(value, f"{path}.{key}"))

    for key in ("anyOf", "oneOf", "allOf"):
        variants = schema.get(key)
        if variants is not None:
            if not isinstance(variants, list) or not variants:
                errors.append(f"{path}.{key}: must be a non-empty array")
            else:
                for index, variant in enumerate(variants):
                    errors.extend(schema_errors(variant, f"{path}.{key}[{index}]"))

    defs = schema.get("$defs")
    if defs is not None:
        if not isinstance(defs, dict):
            errors.append(f"{path}.$defs: must be an object")
        else:
            for name, definition in defs.items():
                errors.extend(schema_errors(definition, f"{path}.$defs.{name}"))

    return errors


def payload_bytes(payload: Any) -> int:
    """Compact JSON size of a JSON-RPC result payload, in bytes."""
    return len(json.dumps(payload, separators=(",", ":")).encode())


def validate_tool(tool: Any) -> list[str]:
    errors: list[str] = []
    if not isinstance(tool, dict):
        return ["tool entry is not an object"]
    name = tool.get("name")
    if not isinstance(name, str) or not name:
        errors.append("name: missing or empty")
    elif not all(ch.isalnum() or ch in "_-" for ch in name):
        errors.append(f"name: {name!r} uses characters outside [A-Za-z0-9_-]")
    description = tool.get("description")
    if not isinstance(description, str) or not description.strip():
        errors.append("description: missing or empty")
    errors.extend(schema_errors(tool.get("inputSchema")))
    return errors


# ---------------------------------------------------------------------------
# stdio client
# ---------------------------------------------------------------------------


class StdioClient:
    def __init__(self, binary: str, args: list[str], env: dict[str, str]) -> None:
        self.proc = subprocess.Popen(
            [binary, *args],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
            env=env,
        )
        self.messages: queue.Queue[Any] = queue.Queue()
        self.next_id = 1
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    def _read_loop(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                self.messages.put(json.loads(line))
            except json.JSONDecodeError:
                self.messages.put({"__raw__": line})
        self.messages.put(None)

    def send(self, payload: Any) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
        self.proc.stdin.flush()

    def send_raw(self, text: str) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(text + "\n")
        self.proc.stdin.flush()

    def read(self, timeout: float = 10.0) -> Any:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("timed out waiting for stdio message")
            try:
                message = self.messages.get(timeout=remaining)
            except queue.Empty as exc:
                raise TimeoutError("timed out waiting for stdio message") from exc
            if message is None:
                raise EOFError("server closed stdout")
            return message

    def request(self, method: str, params: Any = None, timeout: float = 10.0) -> dict:
        request_id = self.next_id
        self.next_id += 1
        payload: dict[str, Any] = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            payload["params"] = params
        self.send(payload)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for response to {method}")
            message = self.read(remaining)
            if isinstance(message, dict) and message.get("id") == request_id:
                return message

    def notify(self, method: str, params: Any = None) -> None:
        payload: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            payload["params"] = params
        self.send(payload)

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)


def stdio_initialize(client: StdioClient, version: str = SUPPORTED_VERSION) -> dict:
    response = client.request(
        "initialize",
        {
            "protocolVersion": version,
            "capabilities": {},
            "clientInfo": {"name": "strut-conformance", "version": "1.0.0"},
        },
    )
    if "error" in response:
        raise RuntimeError(f"initialize failed: {response['error']}")
    client.notify("notifications/initialized")
    return response["result"]


# ---------------------------------------------------------------------------
# HTTP client / server
# ---------------------------------------------------------------------------


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class HttpServer:
    def __init__(
        self,
        binary: str,
        name: str,
        make_launch: Callable[[int, str], tuple[list[str], dict[str, str]]],
        env: dict[str, str],
    ) -> None:
        self.name = name
        self.port = free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        args, extra_env = make_launch(self.port, self.base)
        process_env = dict(env)
        process_env.update(extra_env)
        self.stderr = tempfile.TemporaryFile()
        self.proc = subprocess.Popen(
            [binary, *args],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=self.stderr,
            env=process_env,
        )
        self._wait_ready()

    def _wait_ready(self) -> None:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(
                    f"{self.name} exited early: {self.stderr_tail()}"
                )
            try:
                connection = http.client.HTTPConnection(
                    "127.0.0.1", self.port, timeout=0.5
                )
                connection.request("GET", "/livez")
                response = connection.getresponse()
                response.read()
                connection.close()
                if response.status == 200:
                    return
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(0.1)
        raise RuntimeError(f"{self.name} did not become ready: {self.stderr_tail()}")

    def stderr_tail(self) -> str:
        self.stderr.seek(0)
        data = self.stderr.read()
        return data.decode("utf-8", "replace")[-2000:]

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)


@dataclass
class HttpResponse:
    status: int
    headers: dict[str, str]
    body: bytes

    @property
    def content_type(self) -> str:
        return self.headers.get("content-type", "")

    def messages(self) -> list[Any]:
        text = self.body.decode("utf-8", "replace")
        if "text/event-stream" in self.content_type:
            return parse_sse(text)
        try:
            value = json.loads(text)
        except json.JSONDecodeError:
            return []
        return value if isinstance(value, list) else [value]

    def first(self) -> Any:
        messages = self.messages()
        return messages[0] if messages else None

    def header(self, name: str) -> str | None:
        return self.headers.get(name.lower())


def parse_sse(text: str) -> list[Any]:
    messages: list[Any] = []
    event_lines: list[str] = []
    for line in text.splitlines() + [""]:
        if line == "":
            if event_lines:
                payload = "\n".join(event_lines)
                event_lines = []
                try:
                    messages.append(json.loads(payload))
                except json.JSONDecodeError:
                    pass
            continue
        if line.startswith("data:"):
            event_lines.append(line[5:].lstrip())
    return messages


class HttpClient:
    def __init__(self, base: str, token: str | None = None) -> None:
        self.base = base
        parts = urlsplit(base)
        self.host = parts.hostname or "127.0.0.1"
        self.port = parts.port or 80
        self.token = token
        self.session: str | None = None
        self.protocol_version: str | None = None

    def _raw(
        self,
        method: str,
        path: str,
        headers: dict[str, str],
        body: bytes | None = None,
        timeout: float = 15.0,
    ) -> HttpResponse:
        connection = http.client.HTTPConnection(self.host, self.port, timeout=timeout)
        if body is not None and "Content-Length" not in headers:
            headers = {**headers, "Content-Length": str(len(body))}
        connection.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
        for name, value in headers.items():
            connection.putheader(name, value)
        connection.endheaders(body)
        response = connection.getresponse()
        data = response.read()
        response_headers = {
            name.lower(): value for name, value in response.getheaders()
        }
        connection.close()
        return HttpResponse(response.status, response_headers, data)

    def post_raw(
        self,
        body: bytes,
        *,
        session: bool = True,
        protocol_header: str | None = SUPPORTED_VERSION,
        auth: bool = True,
        extra_headers: dict[str, str] | None = None,
        timeout: float = 15.0,
    ) -> HttpResponse:
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "Host": f"{self.host}:{self.port}",
        }
        if auth and self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if session and self.session:
            headers["mcp-session-id"] = self.session
        if protocol_header and self.protocol_version:
            headers["mcp-protocol-version"] = protocol_header
        if extra_headers:
            headers.update(extra_headers)
        return self._raw("POST", "/mcp", headers, body, timeout=timeout)

    def post_json(self, payload: Any, **kwargs: Any) -> HttpResponse:
        return self.post_raw(
            json.dumps(payload, separators=(",", ":")).encode(), **kwargs
        )

    def initialize(
        self,
        version: str = SUPPORTED_VERSION,
        *,
        auth: bool = True,
        extra_headers: dict[str, str] | None = None,
        send_protocol_header: bool = False,
    ) -> tuple[HttpResponse, dict]:
        response = self.post_json(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": version,
                    "capabilities": {},
                    "clientInfo": {"name": "strut-conformance", "version": "1.0.0"},
                },
            },
            session=False,
            protocol_header=SUPPORTED_VERSION if send_protocol_header else None,
            auth=auth,
            extra_headers=(
                {**(extra_headers or {}), "mcp-protocol-version": version}
                if send_protocol_header
                else extra_headers
            ),
        )
        message = response.first()
        if isinstance(message, dict) and "result" in message:
            self.session = response.header("mcp-session-id")
            self.protocol_version = message["result"].get("protocolVersion")
        return response, message

    def initialized(self) -> HttpResponse:
        return self.post_json(
            {"jsonrpc": "2.0", "method": "notifications/initialized"}
        )

    def rpc(
        self,
        method: str,
        params: Any = None,
        *,
        request_id: int = 2,
        protocol_header: str | None = SUPPORTED_VERSION,
        session: bool = True,
    ) -> tuple[HttpResponse, Any]:
        payload: dict[str, Any] = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            payload["params"] = params
        response = self.post_json(
            payload,
            session=session,
            protocol_header=protocol_header,
        )
        for message in response.messages():
            if isinstance(message, dict) and message.get("id") == request_id:
                return response, message
        return response, None

    def get(self, path: str, headers: dict[str, str] | None = None) -> HttpResponse:
        base_headers = {
            "Accept": "text/event-stream",
            "Host": f"{self.host}:{self.port}",
        }
        if headers:
            base_headers.update(headers)
        return self._raw("GET", path, base_headers, timeout=5)

    def delete(self) -> HttpResponse:
        headers = {
            "Accept": "application/json, text/event-stream",
            "Host": f"{self.host}:{self.port}",
        }
        if self.session:
            headers["mcp-session-id"] = self.session
        return self._raw("DELETE", "/mcp", headers, timeout=5)


# ---------------------------------------------------------------------------
# Fake OAuth authorization server
# ---------------------------------------------------------------------------


class FakeOAuthServer:
    def __init__(self) -> None:
        self.port = free_port()
        self.issuer = f"http://127.0.0.1:{self.port}"
        self.jwk = FAKE_JWK
        self.httpd = ThreadingHTTPServer(("127.0.0.1", self.port), self._handler())
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    def _handler(self) -> type[BaseHTTPRequestHandler]:
        jwk = self.jwk
        issuer = self.issuer

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args: Any) -> None:  # noqa: D401
                pass

            def do_GET(self) -> None:  # noqa: N802
                if self.path in (
                    "/.well-known/oauth-authorization-server",
                    "/.well-known/openid-configuration",
                ):
                    body = json.dumps(
                        {
                            "issuer": issuer,
                            "jwks_uri": f"{issuer}/jwks",
                            "authorization_endpoint": f"{issuer}/authorize",
                            "token_endpoint": f"{issuer}/token",
                        }
                    ).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
                if self.path == "/jwks":
                    body = json.dumps({"keys": [jwk]}).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
                self.send_response(404)
                self.end_headers()

        return Handler

    def stop(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()


# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------


def is_error_message(message: Any) -> bool:
    return isinstance(message, dict) and isinstance(message.get("error"), dict)


def error_code(message: Any) -> Any:
    if is_error_message(message):
        return message["error"].get("code")
    return None


def result_json(result: Any) -> Any:
    """Parse the JSON text block returned by a tool call."""
    if not isinstance(result, dict):
        return None
    content = result.get("content")
    if not isinstance(content, list):
        return None
    for block in content:
        if isinstance(block, dict) and block.get("type") == "text":
            try:
                return json.loads(block.get("text", ""))
            except json.JSONDecodeError:
                return block.get("text")
    return None


def call_tool_stdio(client: StdioClient, name: str, arguments: dict) -> Any:
    return client.request("tools/call", {"name": name, "arguments": arguments})


def tool_call_ok(response: Any) -> tuple[bool, str]:
    if is_error_message(response):
        return False, f"JSON-RPC error: {response['error']}"
    result = response.get("result") if isinstance(response, dict) else None
    if not isinstance(result, dict):
        return False, f"missing result: {response!r}"
    if result.get("isError") is True:
        return False, f"isError=true: {result_json(result)!r}"
    if not result.get("content"):
        return False, f"empty content: {result!r}"
    return True, ""


def run_checks(results: Results, binary: str, keep_temp: bool) -> int:
    tmpdir = Path(tempfile.mkdtemp(prefix="strut-conformance-"))
    db_path = tmpdir / "conformance.db"
    seed_sqlite(db_path)

    env = dict(os.environ)
    for key in (
        "DATABASE_URL",
        "MYSQL_URL",
        "POSTGRES_URL",
        "SQLITE_URL",
        "STRUT_STACK_SQL_HTTP_TOKENS",
        "STRUT_STACK_SQL_OAUTH_ISSUER",
        "STRUT_STACK_SQL_OAUTH_AUDIENCE",
        "STRUT_STACK_SQL_ALLOWED_ORIGINS",
        "STRUT_STACK_SQL_EPHEMERAL",
    ):
        env.pop(key, None)
    env["MCP_SQL_LOG"] = "error"

    sqlite_url = f"sqlite://{db_path}"
    stdio_default: StdioClient | None = None
    stdio_full: StdioClient | None = None
    http_public: HttpServer | None = None
    http_token: HttpServer | None = None
    http_oauth: HttpServer | None = None
    oauth_as: FakeOAuthServer | None = None

    try:
        oauth_as = FakeOAuthServer()

        http_public = HttpServer(
            binary,
            "http-public",
            lambda port, base: (
                [
                    "--url",
                    sqlite_url,
                    "--http",
                    f"127.0.0.1:{port}",
                    "--full-tools",
                    "--allow-ddl",
                ],
                {},
            ),
            env,
        )
        http_token = HttpServer(
            binary,
            "http-token",
            lambda port, base: (
                [
                    "--url",
                    sqlite_url,
                    "--http",
                    f"127.0.0.1:{port}",
                    "--http-token",
                    "conformance-token",
                    "--full-tools",
                    "--allow-ddl",
                ],
                {},
            ),
            env,
        )
        http_oauth = HttpServer(
            binary,
            "http-oauth",
            lambda port, base: (
                [
                    "--url",
                    sqlite_url,
                    "--http",
                    f"127.0.0.1:{port}",
                    "--full-tools",
                    "--allow-ddl",
                ],
                {
                    "STRUT_STACK_SQL_OAUTH_ISSUER": oauth_as.issuer,
                    "STRUT_STACK_SQL_OAUTH_AUDIENCE": f"{base}/mcp",
                },
            ),
            env,
        )

        stdio_default = StdioClient(
            binary,
            ["--url", sqlite_url],
            env,
        )
        stdio_full = StdioClient(
            binary,
            ["--url", sqlite_url, "--full-tools", "--allow-ddl"],
            env,
        )

        run_stdio_checks(results, stdio_default, stdio_full, env)
        run_http_checks(results, http_public, http_token, http_oauth, oauth_as)

    except Exception:
        results.fail(
            "runner.setup",
            "runner",
            "both",
            "Conformance environment starts",
            "PRD-CONF-01 (runnable gate)",
            error=traceback.format_exc(limit=3),
        )
    finally:
        for client in (stdio_default, stdio_full):
            if client is not None:
                client.close()
        for server in (http_public, http_token, http_oauth):
            if server is not None:
                server.stop()
        if oauth_as is not None:
            oauth_as.stop()
        if not keep_temp:
            shutil.rmtree(tmpdir, ignore_errors=True)

    return 0


def seed_sqlite(path: Path) -> None:
    connection = sqlite3.connect(path)
    connection.executescript(
        """
        CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT UNIQUE
        );
        CREATE TABLE orders (
            id INTEGER PRIMARY KEY,
            user_id INTEGER NOT NULL REFERENCES users(id),
            total REAL NOT NULL
        );
        CREATE INDEX idx_orders_user ON orders(user_id);
        INSERT INTO users (id, name, email) VALUES
            (1, 'alice', 'alice@example.com'),
            (2, 'bob', 'bob@example.com');
        INSERT INTO orders (id, user_id, total) VALUES
            (1, 1, 19.5),
            (2, 2, 42.0);
        """
    )
    connection.close()


def run_stdio_checks(
    results: Results,
    default: StdioClient,
    full: StdioClient,
    env: dict[str, str],
) -> None:
    # --- lifecycle ---------------------------------------------------------
    try:
        init = stdio_initialize(default, SUPPORTED_VERSION)
        version_ok = init.get("protocolVersion") == SUPPORTED_VERSION
        server_info = init.get("serverInfo") or {}
        identity_ok = (
            server_info.get("name") == "strut-stack-sql"
            and isinstance(server_info.get("version"), str)
            and bool(server_info.get("version"))
        )
        capabilities_ok = "tools" in (init.get("capabilities") or {})
        results.ok(
            "stdio.initialize.supported",
            "stdio.lifecycle",
            "stdio",
            "initialize handshake succeeds and reports the pinned spec version",
            f"{SPEC_BASE}/basic/lifecycle#initialization; {PRD_CONF}",
            version_ok and identity_ok and capabilities_ok,
            detail=init,
            error=None
            if version_ok and identity_ok and capabilities_ok
            else "protocolVersion/serverInfo/capabilities mismatch",
        )
        ping = default.request("ping")
        results.ok(
            "stdio.ping",
            "stdio.lifecycle",
            "stdio",
            "ping returns an empty result object",
            f"{SPEC_BASE}/basic/utilities/ping",
            ping.get("result") == {},
            detail=ping,
            error=None if ping.get("result") == {} else "non-empty ping result",
        )
    except Exception as exc:
        results.fail(
            "stdio.initialize.supported",
            "stdio.lifecycle",
            "stdio",
            "initialize handshake succeeds and reports the pinned spec version",
            f"{SPEC_BASE}/basic/lifecycle#initialization; {PRD_CONF}",
            error=f"{exc}\n{traceback.format_exc(limit=2)}",
        )
        results.not_run(
            "stdio.ping",
            "stdio.lifecycle",
            "stdio",
            "ping returns an empty result object",
            f"{SPEC_BASE}/basic/utilities/ping",
            detail={"reason": "initialize failed"},
        )

    # Unsupported version fallback (new process: negotiation is per connection).
    fallback: StdioClient | None = None
    try:
        fallback = StdioClient(default.proc.args[0], list(default.proc.args[1:]), env)
        init = stdio_initialize(fallback, "2025-03-26")
        got = init.get("protocolVersion")
        results.ok(
            "stdio.initialize.unsupported_fallback",
            "stdio.lifecycle",
            "stdio",
            "unsupported requested version falls back to the pinned version",
            f"{SPEC_BASE}/basic/lifecycle#version-negotiation",
            got == SUPPORTED_VERSION,
            detail={"requested": "2025-03-26", "negotiated": got},
            error=None if got == SUPPORTED_VERSION else f"negotiated {got!r}",
        )
    except Exception as exc:
        results.fail(
            "stdio.initialize.unsupported_fallback",
            "stdio.lifecycle",
            "stdio",
            "unsupported requested version falls back to the pinned version",
            f"{SPEC_BASE}/basic/lifecycle#version-negotiation",
            error=str(exc),
        )
    finally:
        if fallback is not None:
            fallback.close()

    # --- tools/list --------------------------------------------------------
    try:
        response = default.request("tools/list")
        tools = (response.get("result") or {}).get("tools")
        names = {tool.get("name") for tool in tools or []}
        ok = isinstance(tools, list) and names == CORE_TOOLS
        results.ok(
            "stdio.tools.list.default_surface",
            "stdio.tools",
            "stdio",
            "default (read-only) surface lists exactly the three core tools",
            f"{SPEC_BASE}/server/tools#listing-tools; PRD §8.1 token economy",
            ok,
            detail={"count": len(names), "names": sorted(names)},
            error=None if ok else f"expected {sorted(CORE_TOOLS)}",
        )
        core_bytes = payload_bytes(response.get("result") or {})
        results.ok(
            "stdio.tools.list.default_bytes",
            "stdio.tools",
            "stdio",
            f"default tools/list payload is within {CORE_TOOLS_LIST_BYTES_CAP} bytes",
            f"{SPEC_BASE}/server/tools#listing-tools; PRD-F2-12 token budget",
            core_bytes <= CORE_TOOLS_LIST_BYTES_CAP,
            detail={"bytes": core_bytes, "cap": CORE_TOOLS_LIST_BYTES_CAP},
            error=None
            if core_bytes <= CORE_TOOLS_LIST_BYTES_CAP
            else f"{core_bytes} bytes > {CORE_TOOLS_LIST_BYTES_CAP}",
        )
    except Exception as exc:
        results.not_run(
            "stdio.tools.list.default_surface",
            "stdio.tools",
            "stdio",
            "default (read-only) surface lists exactly the three core tools",
            f"{SPEC_BASE}/server/tools#listing-tools",
            detail={"reason": str(exc)},
        )

    full_tools: list[dict] = []
    try:
        stdio_initialize(full, SUPPORTED_VERSION)
        response = full.request("tools/list")
        full_tools = (response.get("result") or {}).get("tools") or []
        names = {tool.get("name") for tool in full_tools}
        errors: list[str] = []
        for tool in full_tools:
            for error in validate_tool(tool):
                errors.append(f"{tool.get('name', '<unnamed>')}: {error}")
        count_ok = names == FULL_TOOLS and len(full_tools) == 20
        results.ok(
            "stdio.tools.list.full_surface",
            "stdio.tools",
            "stdio",
            "full+DDL surface lists exactly 20 tools with valid schemas",
            f"{SPEC_BASE}/server/tools#listing-tools; {PRD_CONF}",
            count_ok and not errors,
            detail={"count": len(full_tools), "schema_errors": errors[:10]},
            error=None
            if count_ok and not errors
            else f"count={len(full_tools)} schema_errors={len(errors)}",
        )
        full_bytes = payload_bytes(response.get("result") or {})
        results.ok(
            "stdio.tools.list.full_bytes",
            "stdio.tools",
            "stdio",
            f"full+DDL tools/list payload is within {FULL_TOOLS_LIST_BYTES_CAP} bytes",
            f"{SPEC_BASE}/server/tools#listing-tools; R1-05 surface cap",
            full_bytes <= FULL_TOOLS_LIST_BYTES_CAP,
            detail={
                "bytes": full_bytes,
                "cap": FULL_TOOLS_LIST_BYTES_CAP,
                "tool_count": len(full_tools),
            },
            error=None
            if full_bytes <= FULL_TOOLS_LIST_BYTES_CAP
            else f"{full_bytes} bytes > {FULL_TOOLS_LIST_BYTES_CAP}",
        )
    except Exception as exc:
        results.not_run(
            "stdio.tools.list.full_surface",
            "stdio.tools",
            "stdio",
            "full+DDL surface lists exactly 20 tools with valid schemas",
            f"{SPEC_BASE}/server/tools#listing-tools; {PRD_CONF}",
            detail={"reason": str(exc)},
        )

    # --- tools/call happy paths -------------------------------------------
    core_calls: list[tuple[str, dict, Callable[[Any], bool]]] = [
        (
            "execute_sql",
            {"sql": "SELECT id, name FROM users ORDER BY id"},
            lambda body: isinstance(body, dict)
            and body.get("ok") is True
            and len(((body.get("data") or {}).get("rows") or [])) == 2,
        ),
        (
            "search_objects",
            {"object_type": "table", "keyword": "users"},
            lambda body: "users" in json.dumps(body),
        ),
        (
            "analyze_query_performance",
            {"sql": "SELECT * FROM users WHERE id = 1"},
            lambda body: isinstance(body, dict) and "engine" in body,
        ),
    ]
    if full_tools:
        for name, arguments, predicate in core_calls:
            try:
                response = call_tool_stdio(full, name, arguments)
                ok, problem = tool_call_ok(response)
                body = result_json(response.get("result"))
                ok = ok and bool(predicate(body))
                results.ok(
                    f"stdio.tools.call.core.{name}",
                    "stdio.tools",
                    "stdio",
                    f"tools/call {name} returns a usable result",
                    f"{SPEC_BASE}/server/tools#calling-tools",
                    ok,
                    detail={"result": body},
                    error=None if ok else problem or "unexpected result shape",
                )
            except Exception as exc:
                results.fail(
                    f"stdio.tools.call.core.{name}",
                    "stdio.tools",
                    "stdio",
                    f"tools/call {name} returns a usable result",
                    f"{SPEC_BASE}/server/tools#calling-tools",
                    error=str(exc),
                )

        try:
            response = call_tool_stdio(
                full,
                "execute_sql",
                {
                    "queries": [
                        "SELECT 1 AS a",
                        "SELECT 2 AS b",
                    ]
                },
            )
            ok, problem = tool_call_ok(response)
            body = result_json(response.get("result"))
            ok = (
                ok
                and isinstance(body, dict)
                and isinstance(body.get("results"), list)
                and len(body["results"]) == 2
                and all(item.get("ok") is True for item in body["results"])
            )
            results.ok(
                "stdio.tools.call.core.batch",
                "stdio.tools",
                "stdio",
                "execute_sql batch preserves input order",
                f"{SPEC_BASE}/server/tools#calling-tools",
                ok,
                detail={"result": body},
                error=None if ok else problem or "batch result shape",
            )
        except Exception as exc:
            results.fail(
                "stdio.tools.call.core.batch",
                "stdio.tools",
                "stdio",
                "execute_sql batch preserves input order",
                f"{SPEC_BASE}/server/tools#calling-tools",
                error=str(exc),
            )

        full_calls = [
            ("list_sources", {}),
            ("list_schemas", {}),
            ("list_tables", {}),
            ("describe_table", {"table": "users"}),
            ("list_indexes", {"table": "orders"}),
            ("list_foreign_keys", {"table": "orders"}),
            ("schema_overview", {}),
            ("get_relationship_graph", {"table": "orders"}),
            ("suggest_joins", {"tables": ["users", "orders"]}),
            ("get_schema_context", {"tables": ["users"]}),
        ]
        for name, arguments in full_calls:
            try:
                response = call_tool_stdio(full, name, arguments)
                ok, problem = tool_call_ok(response)
                results.ok(
                    f"stdio.tools.call.full.{name}",
                    "stdio.tools",
                    "stdio",
                    f"tools/call {name} returns a usable result",
                    f"{SPEC_BASE}/server/tools#calling-tools",
                    ok,
                    detail={"result": result_json(response.get("result"))},
                    error=None if ok else problem,
                )
            except Exception as exc:
                results.fail(
                    f"stdio.tools.call.full.{name}",
                    "stdio.tools",
                    "stdio",
                    f"tools/call {name} returns a usable result",
                    f"{SPEC_BASE}/server/tools#calling-tools",
                    error=str(exc),
                )

        ddl_calls = [
            (
                "create_table",
                {
                    "ddl": "CREATE TABLE conformance_tmp (id INTEGER PRIMARY KEY, label TEXT)"
                },
            ),
            (
                "add_column",
                {
                    "table": "conformance_tmp",
                    "column": {"name": "extra", "type": "TEXT"},
                },
            ),
            ("drop_table", {"table": "conformance_tmp", "confirm": True}),
        ]
        for name, arguments in ddl_calls:
            try:
                response = call_tool_stdio(full, name, arguments)
                ok, problem = tool_call_ok(response)
                body = result_json(response.get("result"))
                ok = ok and isinstance(body, dict) and body.get("ok") is True
                results.ok(
                    f"stdio.tools.call.ddl.{name}",
                    "stdio.tools",
                    "stdio",
                    f"tools/call {name} succeeds on SQLite",
                    f"{SPEC_BASE}/server/tools#calling-tools",
                    ok,
                    detail={"result": body},
                    error=None if ok else problem or "DDL action did not report ok",
                )
            except Exception as exc:
                results.fail(
                    f"stdio.tools.call.ddl.{name}",
                    "stdio.tools",
                    "stdio",
                    f"tools/call {name} succeeds on SQLite",
                    f"{SPEC_BASE}/server/tools#calling-tools",
                    error=str(exc),
                )
    else:
        for name, _, _ in core_calls:
            results.not_run(
                f"stdio.tools.call.core.{name}",
                "stdio.tools",
                "stdio",
                f"tools/call {name} returns a usable result",
                f"{SPEC_BASE}/server/tools#calling-tools",
                detail={"reason": "full tool surface unavailable"},
            )

    # --- error shapes ------------------------------------------------------
    try:
        response = default.request(
            "tools/call", {"name": "definitely_unknown_tool", "arguments": {}}
        )
        error = response.get("error")
        shape_ok = isinstance(error, dict) and isinstance(error.get("code"), int) and bool(
            error.get("message")
        )
        results.ok(
            "stdio.errors.unknown_tool",
            "stdio.errors",
            "stdio",
            "unknown tool returns a JSON-RPC error object",
            f"{SPEC_BASE}/server/tools#error-handling",
            shape_ok,
            detail={"error": error},
            error=None if shape_ok else "missing/invalid error object",
        )
        canonical = isinstance(error, dict) and error.get("code") == -32602
        results.ok(
            "stdio.errors.unknown_tool.canonical_code",
            "stdio.errors",
            "stdio",
            "unknown tool uses the recommended -32602 code",
            f"{SPEC_BASE}/server/tools#error-handling (SHOULD)",
            canonical,
            optional=True,
            detail={"actual_code": error.get("code") if isinstance(error, dict) else None},
            error=None
            if canonical
            else "server uses a typed authorization error code (documented)",
        )
    except Exception as exc:
        results.fail(
            "stdio.errors.unknown_tool",
            "stdio.errors",
            "stdio",
            "unknown tool returns a JSON-RPC error object",
            f"{SPEC_BASE}/server/tools#error-handling",
            error=str(exc),
        )

    try:
        # Well-formed JSON that is not a JSON-RPC message.
        default.send({"jsonrpc": "2.0", "id": 99})
        message = default.read(5)
        code = error_code(message)
        results.ok(
            "stdio.errors.invalid_request",
            "stdio.errors",
            "stdio",
            "invalid JSON-RPC envelope returns -32600",
            f"{SPEC_BASE}/basic/index#messages; JSON-RPC 2.0",
            code == -32600,
            detail={"message": message},
            error=None if code == -32600 else f"code={code!r}",
        )
    except Exception as exc:
        results.fail(
            "stdio.errors.invalid_request",
            "stdio.errors",
            "stdio",
            "invalid JSON-RPC envelope returns -32600",
            f"{SPEC_BASE}/basic/index#messages; JSON-RPC 2.0",
            error=str(exc),
        )

    try:
        # Syntactically invalid JSON: the client is violating the stdio
        # transport contract ("MUST NOT write anything that is not a valid
        # MCP message"), and rmcp deliberately ignores such input.
        default.send_raw("{not json")
        try:
            message = default.read(0.75)
            ok = error_code(message) == -32700
            detail = {"message": message}
        except TimeoutError:
            ok = False
            detail = {"response": None, "note": "ignored by rmcp transport"}
        results.ok(
            "stdio.errors.parse_error",
            "stdio.errors",
            "stdio",
            "invalid JSON bytes produce a parse error",
            "JSON-RPC 2.0 §5 (client MUST NOT send invalid messages)",
            ok,
            optional=True,
            detail=detail,
            error=None if ok else "no -32700 response (upstream rmcp behaviour)",
        )
    except Exception as exc:
        results.fail(
            "stdio.errors.parse_error",
            "stdio.errors",
            "stdio",
            "invalid JSON bytes produce a parse error",
            "JSON-RPC 2.0 §5",
            error=str(exc),
        )

    # --- batch -------------------------------------------------------------
    try:
        default.send(
            [
                {"jsonrpc": "2.0", "id": 101, "method": "ping"},
                {"jsonrpc": "2.0", "id": 102, "method": "ping"},
            ]
        )
        message = default.read(5)
        ok = is_error_message(message) and error_code(message) == -32600
        results.ok(
            "stdio.batch.rejected",
            "stdio.batch",
            "stdio",
            "JSON-RPC batch is rejected under the pinned 2025-06-18 spec",
            f"{SPEC_BASE}/basic/transports#stdio (individual messages); 2025-06-18 removed batching",
            ok,
            detail={"message": message},
            error=None if ok else f"batch not rejected: {message!r}",
        )
    except Exception as exc:
        results.fail(
            "stdio.batch.rejected",
            "stdio.batch",
            "stdio",
            "JSON-RPC batch is rejected under the pinned 2025-06-18 spec",
            f"{SPEC_BASE}/basic/transports#stdio",
            error=str(exc),
        )


def run_http_checks(
    results: Results,
    public: HttpServer,
    token: HttpServer,
    oauth: HttpServer,
    oauth_as: FakeOAuthServer,
) -> None:
    client = HttpClient(public.base)
    token_client = HttpClient(token.base, token="conformance-token")
    oauth_client = HttpClient(oauth.base)

    # --- initialize / ping / tools ----------------------------------------
    try:
        response, message = client.initialize(
            SUPPORTED_VERSION, send_protocol_header=True
        )
        result = message.get("result") if isinstance(message, dict) else None
        session = client.session
        session_ok = (
            isinstance(session, str)
            and bool(session)
            and all(0x21 <= ord(ch) <= 0x7E for ch in session)
        )
        version_ok = isinstance(result, dict) and (
            result.get("protocolVersion") == SUPPORTED_VERSION
        )
        identity_ok = isinstance(result, dict) and (
            (result.get("serverInfo") or {}).get("name") == "strut-stack-sql"
        )
        results.ok(
            "http.initialize",
            "http.lifecycle",
            "http",
            "initialize succeeds over Streamable HTTP and assigns a session",
            f"{SPEC_BASE}/basic/transports#session-management; {PRD_CONF}",
            response.status == 200 and session_ok and version_ok and identity_ok,
            detail={
                "status": response.status,
                "session": session,
                "result": result,
            },
            error=None
            if response.status == 200 and session_ok and version_ok and identity_ok
            else "initialize/session/version mismatch",
        )
        if client.session:
            init_notification = client.initialized()
            results.ok(
                "http.initialized.notification",
                "http.lifecycle",
                "http",
                "notifications/initialized is accepted with 202",
                f"{SPEC_BASE}/basic/lifecycle#initialization",
                init_notification.status == 202,
                detail={"status": init_notification.status},
                error=None
                if init_notification.status == 202
                else f"status {init_notification.status}",
            )
            ping_response, ping = client.rpc("ping")
            results.ok(
                "http.ping",
                "http.lifecycle",
                "http",
                "ping returns an empty result object",
                f"{SPEC_BASE}/basic/utilities/ping",
                ping_response.status == 200
                and isinstance(ping, dict)
                and ping.get("result") == {},
                detail={"status": ping_response.status, "message": ping},
                error=None if ping and ping.get("result") == {} else "bad ping result",
            )
    except Exception as exc:
        results.fail(
            "http.initialize",
            "http.lifecycle",
            "http",
            "initialize succeeds over Streamable HTTP and assigns a session",
            f"{SPEC_BASE}/basic/transports#session-management; {PRD_CONF}",
            error=f"{exc}\n{traceback.format_exc(limit=2)}",
        )
        for check_id, description, clause in (
            (
                "http.initialized.notification",
                "notifications/initialized is accepted with 202",
                f"{SPEC_BASE}/basic/lifecycle#initialization",
            ),
            (
                "http.ping",
                "ping returns an empty result object",
                f"{SPEC_BASE}/basic/utilities/ping",
            ),
        ):
            results.not_run(
                check_id,
                "http.lifecycle",
                "http",
                description,
                clause,
                detail={"reason": "HTTP initialize failed"},
            )

    # Unsupported requested version falls back when no version header is sent.
    try:
        fallback_client = HttpClient(public.base)
        response, message = fallback_client.initialize(
            "2025-03-26", send_protocol_header=False
        )
        result = message.get("result") if isinstance(message, dict) else None
        got = result.get("protocolVersion") if isinstance(result, dict) else None
        results.ok(
            "http.initialize.unsupported_fallback",
            "http.lifecycle",
            "http",
            "unsupported initialize version is answered with the pinned version",
            f"{SPEC_BASE}/basic/lifecycle#version-negotiation",
            response.status == 200 and got == SUPPORTED_VERSION,
            detail={"requested": "2025-03-26", "negotiated": got},
            error=None if got == SUPPORTED_VERSION else f"negotiated {got!r}",
        )
    except Exception as exc:
        results.fail(
            "http.initialize.unsupported_fallback",
            "http.lifecycle",
            "http",
            "unsupported initialize version is answered with the pinned version",
            f"{SPEC_BASE}/basic/lifecycle#version-negotiation",
            error=str(exc),
        )

    try:
        response, message = client.rpc("tools/list", request_id=20)
        tools = (message or {}).get("result", {}).get("tools") or []
        names = {tool.get("name") for tool in tools}
        errors = [
            f"{tool.get('name')}: {error}"
            for tool in tools
            for error in validate_tool(tool)
        ]
        ok = response.status == 200 and names == FULL_TOOLS and not errors
        results.ok(
            "http.tools.list",
            "http.tools",
            "http",
            "tools/list returns all 20 tools with valid schemas",
            f"{SPEC_BASE}/server/tools#listing-tools",
            ok,
            detail={"count": len(tools), "schema_errors": errors[:10]},
            error=None if ok else f"count={len(tools)} errors={len(errors)}",
        )
        http_bytes = payload_bytes((message or {}).get("result") or {})
        results.ok(
            "http.tools.list.full_bytes",
            "http.tools",
            "http",
            f"tools/list payload is within {FULL_TOOLS_LIST_BYTES_CAP} bytes over HTTP",
            f"{SPEC_BASE}/server/tools#listing-tools; R1-05 surface cap",
            http_bytes <= FULL_TOOLS_LIST_BYTES_CAP,
            detail={
                "bytes": http_bytes,
                "cap": FULL_TOOLS_LIST_BYTES_CAP,
                "tool_count": len(tools),
            },
            error=None
            if http_bytes <= FULL_TOOLS_LIST_BYTES_CAP
            else f"{http_bytes} bytes > {FULL_TOOLS_LIST_BYTES_CAP}",
        )
    except Exception as exc:
        results.fail(
            "http.tools.list",
            "http.tools",
            "http",
            "tools/list returns all 20 tools with valid schemas",
            f"{SPEC_BASE}/server/tools#listing-tools",
            error=str(exc),
        )

    try:
        response, message = client.rpc(
            "tools/call",
            {"name": "execute_sql", "arguments": {"sql": "SELECT id, name FROM users ORDER BY id"}},
            request_id=21,
        )
        result = (message or {}).get("result")
        ok = (
            response.status == 200
            and isinstance(result, dict)
            and result.get("isError") is not True
            and isinstance(result_json(result), dict)
            and result_json(result).get("ok") is True
        )
        results.ok(
            "http.tools.call.execute_sql",
            "http.tools",
            "http",
            "tools/call execute_sql returns query data over HTTP",
            f"{SPEC_BASE}/server/tools#calling-tools",
            ok,
            detail={"result": result_json(result)},
            error=None if ok else "execute_sql result shape",
        )
    except Exception as exc:
        results.fail(
            "http.tools.call.execute_sql",
            "http.tools",
            "http",
            "tools/call execute_sql returns query data over HTTP",
            f"{SPEC_BASE}/server/tools#calling-tools",
            error=str(exc),
        )

    try:
        response, message = client.rpc(
            "tools/call",
            {"name": "definitely_unknown_tool", "arguments": {}},
            request_id=22,
        )
        error = (message or {}).get("error")
        shape_ok = isinstance(error, dict) and isinstance(error.get("code"), int) and bool(
            error.get("message")
        )
        results.ok(
            "http.errors.unknown_tool",
            "http.errors",
            "http",
            "unknown tool returns a JSON-RPC error object",
            f"{SPEC_BASE}/server/tools#error-handling",
            shape_ok,
            detail={"error": error},
            error=None if shape_ok else "missing/invalid error object",
        )
    except Exception as exc:
        results.fail(
            "http.errors.unknown_tool",
            "http.errors",
            "http",
            "unknown tool returns a JSON-RPC error object",
            f"{SPEC_BASE}/server/tools#error-handling",
            error=str(exc),
        )

    # --- malformed body / batch -------------------------------------------
    for check_id, body, description in (
        (
            "http.errors.malformed_json",
            b"{not json",
            "malformed JSON body returns an HTTP error status",
        ),
        (
            "http.batch.rejected",
            json.dumps(
                [
                    {"jsonrpc": "2.0", "id": 31, "method": "ping"},
                    {"jsonrpc": "2.0", "id": 32, "method": "ping"},
                ]
            ).encode(),
            "JSON-RPC batch is rejected under the pinned 2025-06-18 spec",
        ),
    ):
        clause = (
            f"{SPEC_BASE}/basic/transports#sending-messages-to-the-server"
            if "batch" in check_id
            else f"{SPEC_BASE}/basic/transports#sending-messages-to-the-server; JSON-RPC 2.0"
        )
        try:
            response = client.post_raw(body)
            messages = response.messages()
            no_result = not any(
                isinstance(message, dict) and "result" in message for message in messages
            )
            ok = 400 <= response.status < 500 and no_result
            results.ok(
                check_id,
                "http.errors" if "errors" in check_id else "http.batch",
                "http",
                description,
                clause,
                ok,
                detail={
                    "status": response.status,
                    "body": response.body.decode("utf-8", "replace")[:300],
                },
                error=None if ok else "expected a 4xx error and no per-item result",
            )
        except Exception as exc:
            results.fail(
                check_id,
                "http.errors" if "errors" in check_id else "http.batch",
                "http",
                description,
                clause,
                error=str(exc),
            )

    # --- session / header behaviour ---------------------------------------
    try:
        response = token_client.post_json(
            {"jsonrpc": "2.0", "id": 41, "method": "ping"},
            session=False,
            protocol_header=None,
            auth=True,
        )
        if response.status == 400:
            results.ok(
                "http.session.missing",
                "http.session",
                "http",
                "request without Mcp-Session-Id is rejected with 400",
                f"{SPEC_BASE}/basic/transports#session-management (SHOULD be 400)",
                True,
                detail={"status": response.status},
            )
        elif 400 <= response.status < 500:
            results.record(
                "http.session.missing",
                "http.session",
                "http",
                "request without Mcp-Session-Id is rejected with 400",
                f"{SPEC_BASE}/basic/transports#session-management (SHOULD be 400)",
                False,
                optional=True,
                detail={"status": response.status},
                error=f"status {response.status}; spec recommends 400",
            )
        else:
            results.fail(
                "http.session.missing",
                "http.session",
                "http",
                "request without Mcp-Session-Id is rejected with 400",
                f"{SPEC_BASE}/basic/transports#session-management",
                detail={"status": response.status},
                error=f"status {response.status}",
            )
    except Exception as exc:
        results.fail(
            "http.session.missing",
            "http.session",
            "http",
            "request without Mcp-Session-Id is rejected",
            f"{SPEC_BASE}/basic/transports#session-management",
            error=str(exc),
        )

    try:
        response = token_client.post_raw(
            json.dumps({"jsonrpc": "2.0", "id": 42, "method": "ping"}).encode(),
            session=False,
            protocol_header=None,
            extra_headers={"mcp-session-id": "unknown-session-id"},
        )
        results.ok(
            "http.session.unknown",
            "http.session",
            "http",
            "unknown session id returns 404",
            f"{SPEC_BASE}/basic/transports#session-management",
            response.status == 404,
            detail={"status": response.status},
            error=None if response.status == 404 else f"status {response.status}",
        )
    except Exception as exc:
        results.fail(
            "http.session.unknown",
            "http.session",
            "http",
            "unknown session id returns 404",
            f"{SPEC_BASE}/basic/transports#session-management",
            error=str(exc),
        )

    try:
        response = token_client.post_raw(
            json.dumps({"jsonrpc": "2.0", "id": 43, "method": "ping"}).encode(),
            extra_headers={"mcp-protocol-version": "2025-03-26"},
        )
        results.ok(
            "http.protocol_header.unsupported",
            "http.protocol",
            "http",
            "unsupported MCP-Protocol-Version header returns 400",
            f"{SPEC_BASE}/basic/transports#protocol-version-header",
            response.status == 400,
            detail={"status": response.status},
            error=None if response.status == 400 else f"status {response.status}",
        )
    except Exception as exc:
        results.fail(
            "http.protocol_header.unsupported",
            "http.protocol",
            "http",
            "unsupported MCP-Protocol-Version header returns 400",
            f"{SPEC_BASE}/basic/transports#protocol-version-header",
            error=str(exc),
        )

    try:
        get_response = client.get("/mcp")
        results.ok(
            "http.get.requires_session",
            "http.session",
            "http",
            "GET /mcp without a session is rejected with 4xx",
            f"{SPEC_BASE}/basic/transports#listening-for-messages-from-the-server; session-management",
            400 <= get_response.status < 500,
            detail={"status": get_response.status},
            error=None
            if 400 <= get_response.status < 500
            else f"status {get_response.status}",
        )
    except Exception as exc:
        results.fail(
            "http.get.requires_session",
            "http.session",
            "http",
            "GET /mcp without a session is rejected with 4xx",
            f"{SPEC_BASE}/basic/transports#listening-for-messages-from-the-server",
            error=str(exc),
        )

    # --- auth (API key) ----------------------------------------------------
    try:
        anonymous = HttpClient(token.base)
        response, _ = anonymous.initialize(SUPPORTED_VERSION, auth=False)
        header = response.header("www-authenticate")
        ok = response.status == 401 and header is not None and "bearer" in header.lower()
        results.ok(
            "http.auth.missing_token",
            "http.auth",
            "http",
            "missing bearer token returns 401 with WWW-Authenticate",
            f"{SPEC_BASE}/basic/authorization; {PRD_CONF}",
            ok,
            detail={"status": response.status, "www_authenticate": header},
            error=None if ok else "missing 401 or WWW-Authenticate",
        )
    except Exception as exc:
        results.fail(
            "http.auth.missing_token",
            "http.auth",
            "http",
            "missing bearer token returns 401 with WWW-Authenticate",
            f"{SPEC_BASE}/basic/authorization; {PRD_CONF}",
            error=str(exc),
        )

    try:
        invalid = HttpClient(token.base, token="wrong-token")
        response, _ = invalid.initialize(SUPPORTED_VERSION)
        header = response.header("www-authenticate")
        ok = response.status == 401 and header is not None and "bearer" in header.lower()
        results.ok(
            "http.auth.invalid_token",
            "http.auth",
            "http",
            "invalid bearer token returns 401 with WWW-Authenticate",
            f"{SPEC_BASE}/basic/authorization",
            ok,
            detail={"status": response.status, "www_authenticate": header},
            error=None if ok else "missing 401 or WWW-Authenticate",
        )
    except Exception as exc:
        results.fail(
            "http.auth.invalid_token",
            "http.auth",
            "http",
            "invalid bearer token returns 401 with WWW-Authenticate",
            f"{SPEC_BASE}/basic/authorization",
            error=str(exc),
        )

    # --- OAuth metadata ----------------------------------------------------
    try:
        metadata = oauth_client.get("/.well-known/oauth-protected-resource")
        body = json.loads(metadata.body) if metadata.body else None
        ok = (
            metadata.status == 200
            and isinstance(body, dict)
            and body.get("resource") == f"{oauth.base}/mcp"
            and body.get("authorization_servers") == [oauth_as.issuer]
            and "header" in (body.get("bearer_methods_supported") or [])
        )
        results.ok(
            "http.oauth.protected_resource",
            "http.oauth",
            "http",
            "protected-resource metadata matches RFC 9728 shape",
            "RFC 9728 §2; PRD-F1-07",
            ok,
            detail={"status": metadata.status, "body": body},
            error=None if ok else "metadata shape mismatch",
        )
        path_metadata = oauth_client.get("/.well-known/oauth-protected-resource/mcp")
        results.ok(
            "http.oauth.protected_resource.path",
            "http.oauth",
            "http",
            "resource-specific well-known path returns the same metadata",
            "RFC 9728 §3.1",
            path_metadata.status == 200 and path_metadata.body == metadata.body,
            detail={"status": path_metadata.status},
            error=None
            if path_metadata.status == 200
            else f"status {path_metadata.status}",
        )
    except Exception as exc:
        results.fail(
            "http.oauth.protected_resource",
            "http.oauth",
            "http",
            "protected-resource metadata matches RFC 9728 shape",
            "RFC 9728 §2; PRD-F1-07",
            error=str(exc),
        )

    try:
        response = HttpClient(public.base).get("/.well-known/oauth-protected-resource")
        results.ok(
            "http.oauth.metadata_absent",
            "http.oauth",
            "http",
            "well-known metadata is 404 when OAuth is not configured",
            "RFC 9728; PRD-F1-07",
            response.status == 404,
            detail={"status": response.status},
            error=None if response.status == 404 else f"status {response.status}",
        )
    except Exception as exc:
        results.fail(
            "http.oauth.metadata_absent",
            "http.oauth",
            "http",
            "well-known metadata is 404 when OAuth is not configured",
            "RFC 9728; PRD-F1-07",
            error=str(exc),
        )

    try:
        anonymous = HttpClient(oauth.base)
        response, _ = anonymous.initialize(SUPPORTED_VERSION, auth=False)
        header = response.header("www-authenticate") or ""
        ok = response.status == 401 and "resource_metadata=" in header
        results.ok(
            "http.oauth.challenge",
            "http.oauth",
            "http",
            "OAuth 401 challenge advertises resource_metadata",
            "RFC 9728 §5.1; MCP authorization",
            ok,
            detail={"status": response.status, "www_authenticate": header},
            error=None if ok else "missing resource_metadata in challenge",
        )
        invalid = HttpClient(oauth.base, token="not-a-jwt")
        response, _ = invalid.initialize(SUPPORTED_VERSION)
        header = response.header("www-authenticate") or ""
        ok = (
            response.status == 401
            and 'error="invalid_token"' in header
            and "resource_metadata=" in header
        )
        results.ok(
            "http.oauth.invalid_token",
            "http.oauth",
            "http",
            "invalid OAuth token returns 401 invalid_token challenge",
            "RFC 6750 §3; MCP authorization",
            ok,
            detail={"status": response.status, "www_authenticate": header},
            error=None if ok else "challenge shape mismatch",
        )
    except Exception as exc:
        results.fail(
            "http.oauth.challenge",
            "http.oauth",
            "http",
            "OAuth 401 challenge advertises resource_metadata",
            "RFC 9728 §5.1",
            error=str(exc),
        )

    # --- DNS rebinding -----------------------------------------------------
    try:
        evil = HttpClient(public.base)
        response = evil.post_json(
            {
                "jsonrpc": "2.0",
                "id": 60,
                "method": "initialize",
                "params": {
                    "protocolVersion": SUPPORTED_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "rebinding-test", "version": "1"},
                },
            },
            session=False,
            protocol_header=None,
            extra_headers={
                "Host": "evil.example.com",
                "Origin": "http://evil.example.com",
            },
        )
        results.ok(
            "http.origin.rebinding_rejected",
            "http.origin",
            "http",
            "non-loopback Host/Origin is rejected (DNS rebinding)",
            f"{SPEC_BASE}/basic/transports#security-warning",
            400 <= response.status < 500,
            detail={"status": response.status},
            error=None
            if 400 <= response.status < 500
            else f"status {response.status}",
        )
        valid = HttpClient(public.base)
        response = valid.post_json(
            {
                "jsonrpc": "2.0",
                "id": 61,
                "method": "initialize",
                "params": {
                    "protocolVersion": SUPPORTED_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "rebinding-test", "version": "1"},
                },
            },
            session=False,
            protocol_header=None,
            extra_headers={
                "Host": f"127.0.0.1:{public.port}",
                "Origin": f"http://127.0.0.1:{public.port}",
            },
        )
        results.ok(
            "http.origin.loopback_accepted",
            "http.origin",
            "http",
            "loopback Host/Origin is accepted",
            f"{SPEC_BASE}/basic/transports#security-warning",
            200 <= response.status < 300,
            detail={"status": response.status},
            error=None
            if 200 <= response.status < 300
            else f"status {response.status}",
        )
    except Exception as exc:
        results.fail(
            "http.origin.rebinding_rejected",
            "http.origin",
            "http",
            "non-loopback Host/Origin is rejected (DNS rebinding)",
            f"{SPEC_BASE}/basic/transports#security-warning",
            error=str(exc),
        )

    # --- DELETE session ----------------------------------------------------
    try:
        if client.session:
            deleted = client.delete()
            terminated = 200 <= deleted.status < 300
            results.ok(
                "http.session.delete",
                "http.session",
                "http",
                "DELETE terminates the session",
                f"{SPEC_BASE}/basic/transports#session-management",
                terminated or deleted.status == 405,
                detail={"status": deleted.status},
                error=None if terminated or deleted.status == 405 else "unexpected status",
            )
            if terminated:
                after = client.post_json(
                    {"jsonrpc": "2.0", "id": 70, "method": "ping"},
                )
                results.ok(
                    "http.session.after_delete",
                    "http.session",
                    "http",
                    "requests after DELETE return 404",
                    f"{SPEC_BASE}/basic/transports#session-management",
                    after.status == 404,
                    detail={"status": after.status},
                    error=None if after.status == 404 else f"status {after.status}",
                )
        else:
            results.not_run(
                "http.session.delete",
                "http.session",
                "http",
                "DELETE terminates the session",
                f"{SPEC_BASE}/basic/transports#session-management",
                detail={"reason": "no session established"},
            )
    except Exception as exc:
        results.fail(
            "http.session.delete",
            "http.session",
            "http",
            "DELETE terminates the session",
            f"{SPEC_BASE}/basic/transports#session-management",
            error=str(exc),
        )


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def human_summary(report: dict) -> str:
    lines: list[str] = []
    counts = report["counts"]
    lines.append(
        "MCP conformance: "
        f"{counts['pass']} pass, {counts['fail']} fail, "
        f"{counts['optional']} optional, {counts['not_run']} not_run"
    )
    lines.append(
        f"binary: {report['binary']} ({report['binary_version']})  "
        f"spec: {report['supported_protocol_version']}  "
        f"finished: {report['finished_at']}"
    )
    for check in report["checks"]:
        marker = {
            "pass": "PASS",
            "fail": "FAIL",
            "optional": "OPT ",
            "not_run": "SKIP",
        }[check["status"]]
        lines.append(
            f"[{marker}] {check['group']:<18} {check['id']} — {check['description']}"
        )
        if check["status"] in ("fail", "optional") and (check["error"] or check["detail"]):
            detail = check["error"] or json.dumps(check["detail"], default=str)
            lines.append(f"       {detail.splitlines()[0][:300]}")
        lines.append(f"       clause: {check['clause']}")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        default="target/release/strut-stack-sql",
        help="release binary to exercise",
    )
    parser.add_argument(
        "--json-out",
        default="target/conformance/report.json",
        help="machine-readable report path",
    )
    parser.add_argument("--keep-temp", action="store_true", help="keep fixtures")
    parser.add_argument("--quiet", action="store_true", help="suppress human summary")
    args = parser.parse_args(argv)

    repo_root = Path(__file__).resolve().parents[2]
    binary = Path(args.binary)
    if not binary.is_absolute():
        binary = repo_root / binary
    if not binary.is_file() or not os.access(binary, os.X_OK):
        print(
            f"error: release binary not found/executable: {binary}\n"
            "build it with: cargo build --release --workspace",
            file=sys.stderr,
        )
        return 2

    try:
        version_output = subprocess.check_output(
            [str(binary), "--version"], text=True, timeout=10
        ).strip()
    except Exception as exc:
        print(f"error: cannot run {binary}: {exc}", file=sys.stderr)
        return 2

    started = datetime.now(timezone.utc)
    results = Results()
    run_checks(results, str(binary), args.keep_temp)
    finished = datetime.now(timezone.utc)

    counts = results.counts()
    report = {
        "runner": "strut-stack-sql self-contained conformance runner",
        "started_at": started.isoformat(),
        "finished_at": finished.isoformat(),
        "binary": str(binary),
        "binary_version": version_output,
        "supported_protocol_version": SUPPORTED_VERSION,
        "clauses": {
            "spec": f"{SPEC_BASE}",
            "prd": PRD_CONF,
        },
        "counts": counts,
        "checks": [asdict(check) for check in results.checks],
        "not_run": [
            {
                "id": check.id,
                "description": check.description,
                "reason": check.detail.get("reason", ""),
            }
            for check in results.checks
            if check.status == "not_run"
        ],
    }

    json_out = Path(args.json_out)
    if not json_out.is_absolute():
        json_out = repo_root / json_out
    json_out.parent.mkdir(parents=True, exist_ok=True)
    json_out.write_text(json.dumps(report, indent=2) + "\n")

    if not args.quiet:
        print(human_summary(report))
        print(f"JSON report: {json_out}")

    return 1 if results.failed_required() else 0


if __name__ == "__main__":
    sys.exit(main())
