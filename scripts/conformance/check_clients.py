#!/usr/bin/env python3
"""Machine-check docs/CLIENTS.md.

* Extracts every fenced JSON/TOML config block (and the marked shell blocks).
* Parses each block with the matching parser (json / tomllib).
* Validates the client-specific container shape and the strut-stack-sql entry.
* Smoke-runs the exact command line from the marked generic stdio snippet:
  initialize -> notifications/initialized -> tools/list.

Python 3 standard library only. Exits non-zero on any failure.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import Any

MARKER = re.compile(r"<!--\s*conformance:\s*(.*?)\s*-->")
ATTR = re.compile(r"(\w+)=([^\s]+)")
FENCE = "```"

REQUIRED_CLIENTS = {
    "generic",
    "cursor",
    "claude-code",
    "vscode",
    "codex",
    "windsurf",
    "opencode",
}
SERVER_NAME = "strut-stack-sql"


class Failure(Exception):
    pass


def parse_attrs(raw: str) -> dict[str, str]:
    return {key: value for key, value in ATTR.findall(raw)}


def scan_blocks(text: str) -> tuple[list[dict[str, Any]], list[str]]:
    lines = text.splitlines()
    blocks: list[dict[str, Any]] = []
    problems: list[str] = []
    pending: dict[str, str] | None = None
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        marker = MARKER.fullmatch(stripped)
        if marker:
            pending = parse_attrs(marker.group(1))
            index += 1
            continue
        if stripped.startswith(FENCE):
            language = stripped[3:].strip()
            start = index + 1
            index += 1
            body: list[str] = []
            while index < len(lines) and not lines[index].strip().startswith(FENCE):
                body.append(lines[index])
                index += 1
            index += 1
            if language in ("json", "toml", "shell", "bash"):
                if pending is None:
                    problems.append(
                        f"unmarked {language} block near line {start}: "
                        f"{body[0][:60] if body else ''}"
                    )
                else:
                    blocks.append(
                        {
                            "language": language,
                            "body": "\n".join(body),
                            "attrs": pending,
                            "line": start,
                        }
                    )
            pending = None
            continue
        if stripped:
            pending = None
        index += 1
    return blocks, problems


def require(condition: bool, message: str) -> None:
    if not condition:
        raise Failure(message)


def is_http_url(value: Any) -> bool:
    return isinstance(value, str) and value.startswith(("http://", "https://"))


def check_stdio_entry(client: str, entry: dict[str, Any]) -> None:
    require(isinstance(entry, dict), f"{client}: server entry must be an object")
    if client == "opencode":
        require(entry.get("type") == "local", "opencode stdio: type must be local")
        command = entry.get("command")
        require(
            isinstance(command, list) and command and command[0] == SERVER_NAME,
            "opencode stdio: command must be an array starting with strut-stack-sql",
        )
        require("--full-tools" in command, "opencode stdio: --full-tools missing")
        require(
            "--url-env" in command or "--url" in command or "--config" in command,
            "opencode stdio: no connection flag",
        )
        environment = entry.get("environment")
        require(
            environment is None or isinstance(environment, dict),
            "opencode stdio: environment must be an object",
        )
        return
    if client == "vscode":
        require(entry.get("type") == "stdio", "vscode stdio: type must be stdio")
    require(entry.get("command") == SERVER_NAME, f"{client} stdio: command must be {SERVER_NAME}")
    args = entry.get("args")
    require(isinstance(args, list), f"{client} stdio: args must be an array")
    require("--full-tools" in args, f"{client} stdio: --full-tools missing")
    require(
        "--url-env" in args or "--url" in args or "--config" in args,
        f"{client} stdio: no connection flag",
    )
    env = entry.get("env")
    require(env is None or isinstance(env, dict), f"{client} stdio: env must be an object")


def check_http_entry(client: str, entry: dict[str, Any]) -> None:
    require(isinstance(entry, dict), f"{client}: server entry must be an object")
    if client == "opencode":
        require(entry.get("type") == "remote", "opencode http: type must be remote")
        require(is_http_url(entry.get("url")), "opencode http: url missing")
        require(entry.get("oauth") is False, "opencode http: set oauth=false for bearer auth")
        headers = entry.get("headers")
        require(isinstance(headers, dict), "opencode http: headers missing")
        require(
            "Bearer" in str(headers.get("Authorization", "")),
            "opencode http: Authorization bearer header missing",
        )
        return
    if client == "windsurf":
        url = entry.get("serverUrl") or entry.get("url")
        require(is_http_url(url), "windsurf http: serverUrl/url missing")
    elif client == "cursor":
        require(is_http_url(entry.get("url")), "cursor http: url missing")
    else:
        require(is_http_url(entry.get("url")), f"{client} http: url missing")
    if client in ("claude-code", "vscode"):
        require(entry.get("type") == "http", f"{client} http: type must be http")
    headers = entry.get("headers")
    require(isinstance(headers, dict), f"{client} http: headers missing")
    require(
        "Bearer" in str(headers.get("Authorization", "")),
        f"{client} http: Authorization bearer header missing",
    )


def config_entry(client: str, parsed: dict[str, Any]) -> dict[str, Any]:
    container = {
        "cursor": "mcpServers",
        "claude-code": "mcpServers",
        "windsurf": "mcpServers",
        "generic": "mcpServers",
        "vscode": "servers",
        "opencode": "mcp",
    }[client]
    servers = parsed.get(container)
    require(isinstance(servers, dict), f"{client}: top-level {container} object missing")
    entry = servers.get(SERVER_NAME)
    require(isinstance(entry, dict), f"{client}: {container}.{SERVER_NAME} missing")
    return entry


def check_json_block(block: dict[str, Any]) -> dict[str, Any]:
    attrs = block["attrs"]
    client = attrs.get("client")
    transport = attrs.get("transport")
    require(client in REQUIRED_CLIENTS, f"line {block['line']}: unknown client {client!r}")
    require(transport in ("stdio", "http"), f"line {block['line']}: unknown transport")
    try:
        parsed = json.loads(block["body"])
    except json.JSONDecodeError as exc:
        raise Failure(f"line {block['line']}: invalid JSON: {exc}") from exc
    require(isinstance(parsed, dict), f"line {block['line']}: JSON root must be an object")
    entry = config_entry(client, parsed)
    if transport == "stdio":
        check_stdio_entry(client, entry)
    else:
        check_http_entry(client, entry)
    return {"client": client, "transport": transport, "entry": entry}


def check_toml_block(block: dict[str, Any]) -> dict[str, Any]:
    attrs = block["attrs"]
    client = attrs.get("client")
    transport = attrs.get("transport")
    require(client == "codex", f"line {block['line']}: TOML is only used for codex")
    require(transport in ("stdio", "http"), f"line {block['line']}: unknown transport")
    try:
        parsed = tomllib.loads(block["body"])
    except tomllib.TOMLDecodeError as exc:
        raise Failure(f"line {block['line']}: invalid TOML: {exc}") from exc
    servers = parsed.get("mcp_servers")
    require(isinstance(servers, dict), "codex: [mcp_servers] missing")
    entry = servers.get(SERVER_NAME)
    require(isinstance(entry, dict), "codex: mcp_servers.strut-stack-sql missing")
    if transport == "stdio":
        require(entry.get("command") == SERVER_NAME, "codex stdio: command must be strut-stack-sql")
        args = entry.get("args")
        require(isinstance(args, list), "codex stdio: args must be an array")
        require("--full-tools" in args, "codex stdio: --full-tools missing")
        require(
            "--url-env" in args or "--url" in args or "--config" in args,
            "codex stdio: no connection flag",
        )
    else:
        require(is_http_url(entry.get("url")), "codex http: url missing")
        require(
            entry.get("bearer_token_env_var") is not None
            or isinstance(entry.get("http_headers"), dict)
            or isinstance(entry.get("env_http_headers"), dict),
            "codex http: no bearer/header configuration",
        )
    return {"client": client, "transport": transport, "entry": entry}


def check_shell_block(block: dict[str, Any]) -> dict[str, Any]:
    attrs = block["attrs"]
    text = block["body"].strip()
    require(text, f"line {block['line']}: empty shell block")
    client = attrs.get("client")
    transport = attrs.get("transport")
    if client == "claude-code":
        require("claude mcp add" in text, "claude-code shell: not a 'claude mcp add' command")
        require(f"--transport {transport}" in text, "claude-code shell: --transport missing")
        if transport == "stdio":
            require(" -- " in text, "claude-code stdio shell: '--' separator missing")
        else:
            require("--header" in text, "claude-code http shell: --header missing")
    else:
        require("--http" in text and "--http-token" in text, "server launch: flags missing")
    return {"client": client, "transport": transport}


def smoke_run(binary: str, entry: dict[str, Any]) -> dict[str, Any]:
    command = entry.get("command")
    args = entry.get("args")
    env = entry.get("env") or {}
    if isinstance(command, list):  # opencode shape, accepted defensively
        args = command[1:]
        command = command[0]
    require(command == SERVER_NAME, f"smoke: command is {command!r}")
    require(isinstance(args, list), "smoke: args must be an array")
    require(isinstance(env, dict), "smoke: env must be an object")

    process_env = dict(os.environ)
    process_env.update({str(key): str(value) for key, value in env.items()})
    process = subprocess.Popen(
        [binary, *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=process_env,
    )
    try:
        assert process.stdin is not None and process.stdout is not None

        def send(payload: Any) -> None:
            process.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
            process.stdin.flush()

        def read_response(request_id: int) -> dict[str, Any]:
            while True:
                line = process.stdout.readline()
                if not line:
                    stderr = process.stderr.read() if process.stderr else ""
                    raise Failure(f"smoke: server closed stdout; stderr={stderr[-800:]}")
                try:
                    message = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(message, dict) and message.get("id") == request_id:
                    return message

        send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "docs-smoke", "version": "1.0.0"},
                },
            }
        )
        init = read_response(1)
        result = init.get("result") or {}
        require(
            result.get("protocolVersion") == "2025-06-18",
            "smoke: initialize did not negotiate 2025-06-18",
        )
        require(
            (result.get("serverInfo") or {}).get("name") == SERVER_NAME,
            "smoke: serverInfo.name mismatch",
        )
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        tools_response = read_response(2)
        tools = (tools_response.get("result") or {}).get("tools") or []
        names = {tool.get("name") for tool in tools}
        require(len(tools) >= 3, f"smoke: expected >=3 tools, got {len(tools)}")
        require(
            {"search_objects", "execute_sql", "analyze_query_performance"} <= names,
            f"smoke: core tools missing from {sorted(names)}",
        )
        return {"tool_count": len(tools), "command": [binary, *args]}
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docs", default="docs/CLIENTS.md")
    parser.add_argument("--binary", default="target/release/strut-stack-sql")
    parser.add_argument("--json-out", default="")
    args = parser.parse_args(argv)

    root = Path(__file__).resolve().parents[2]
    docs = Path(args.docs)
    if not docs.is_absolute():
        docs = root / docs
    binary = Path(args.binary)
    if not binary.is_absolute():
        binary = root / binary

    failures: list[str] = []
    checked: list[dict[str, Any]] = []
    smoke: dict[str, Any] | None = None
    try:
        text = docs.read_text()
        blocks, problems = scan_blocks(text)
        failures.extend(problems)
        for block in blocks:
            try:
                if block["language"] == "json":
                    parsed = check_json_block(block)
                    if block["attrs"].get("smoke") == "true":
                        if smoke is not None:
                            raise Failure("more than one smoke snippet")
                        smoke = parsed
                elif block["language"] == "toml":
                    parsed = check_toml_block(block)
                else:
                    parsed = check_shell_block(block)
                checked.append(
                    {
                        "client": parsed["client"],
                        "transport": parsed["transport"],
                        "format": block["language"],
                        "line": block["line"],
                    }
                )
            except Failure as exc:
                failures.append(str(exc))

        clients = {item["client"] for item in checked}
        for client in REQUIRED_CLIENTS:
            if client not in clients:
                failures.append(f"missing configuration snippet for {client}")
        for client in REQUIRED_CLIENTS - {"generic"}:
            transports = {
                item["transport"] for item in checked if item["client"] == client
            }
            for transport in ("stdio", "http"):
                if transport not in transports:
                    failures.append(f"{client}: missing {transport} configuration")

        if smoke is None:
            failures.append("no smoke=true stdio snippet found")
        else:
            if not binary.is_file() or not os.access(binary, os.X_OK):
                failures.append(f"smoke: binary not executable: {binary}")
            else:
                try:
                    smoke_result = smoke_run(str(binary), smoke["entry"])
                except Failure as exc:
                    failures.append(str(exc))
                else:
                    print(
                        "smoke: OK — "
                        f"{smoke_result['tool_count']} tools from "
                        f"{' '.join(smoke_result['command'][1:])}"
                    )
    except OSError as exc:
        failures.append(f"cannot read {docs}: {exc}")

    print(f"checked {len(checked)} marked snippets in {docs}")
    for failure in failures:
        print(f"FAIL: {failure}", file=sys.stderr)
    if args.json_out:
        output = Path(args.json_out)
        if not output.is_absolute():
            output = root / output
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(
            json.dumps(
                {
                    "docs": str(docs),
                    "binary": str(binary),
                    "snippets": checked,
                    "failures": failures,
                    "ok": not failures,
                },
                indent=2,
            )
            + "\n"
        )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
