#!/usr/bin/env node
/**
 * Minimal MCP stdio client for benchmarks (newline-delimited JSON).
 * Usage: node mcp-client.mjs --command strut-stack-sql --args '[]' --tool execute_sql --input '{"sql":"SELECT 1"}'
 */
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { readFileSync } from "node:fs";

function parseArgs(argv) {
  const out = { command: null, args: [], tool: null, input: {}, iterations: 1, warmup: 1 };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--command") out.command = argv[++i];
    else if (a === "--args") out.args = JSON.parse(argv[++i]);
    else if (a === "--env") out.env = JSON.parse(argv[++i]);
    else if (a === "--tool") out.tool = argv[++i];
    else if (a === "--input") out.input = JSON.parse(argv[++i]);
    else if (a === "--iterations") out.iterations = Number(argv[++i]);
    else if (a === "--warmup") out.warmup = Number(argv[++i]);
  }
  if (!out.command || !out.tool) {
    console.error("Required: --command and --tool");
    process.exit(2);
  }
  return out;
}

class McpClient {
  constructor(proc) {
    this.proc = proc;
    this.pending = new Map();
    this.nextId = 1;
    const rl = createInterface({ input: proc.stdout });
    rl.on("line", (line) => {
      if (!line.trim()) return;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return;
      }
      if (msg.id != null && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(JSON.stringify(msg.error)));
        else resolve(msg.result);
      }
    });
    proc.stderr.on("data", (chunk) => process.stderr.write(chunk));
  }

  send(payload) {
    this.proc.stdin.write(JSON.stringify(payload) + "\n");
  }

  request(method, params) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.send({ jsonrpc: "2.0", id, method, params });
    });
  }

  notify(method, params) {
    this.send({ jsonrpc: "2.0", method, params });
  }

  close() {
    this.proc.stdin.end();
    this.proc.kill("SIGTERM");
  }
}

function rssKb(pid) {
  try {
    const status = readFileSync(`/proc/${pid}/status`, "utf8");
    const m = status.match(/^VmRSS:\s+(\d+)/m);
    return m ? Number(m[1]) : null;
  } catch {
    return null;
  }
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

async function main() {
  const opts = parseArgs(process.argv);
  const env = { ...process.env, ...(opts.env || {}) };
  const t0 = performance.now();
  const proc = spawn(opts.command, opts.args, { env, stdio: ["pipe", "pipe", "pipe"] });
  const client = new McpClient(proc);

  await client.request("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "benchmark", version: "1.0.0" },
  });
  const coldStartMs = performance.now() - t0;
  client.notify("notifications/initialized", {});

  const toolsList = await client.request("tools/list", {});
  const toolsListBytes = Buffer.byteLength(JSON.stringify(toolsList), "utf8");

  await sleep(2000);
  const idleRssKb = rssKb(proc.pid);

  const latencies = [];
  const totalCalls = opts.warmup + opts.iterations;
  for (let i = 0; i < totalCalls; i++) {
    const start = performance.now();
    await client.request("tools/call", {
      name: opts.tool,
      arguments: opts.input,
    });
    const elapsed = performance.now() - start;
    if (i >= opts.warmup) latencies.push(elapsed);
  }

  const loadRssKb = rssKb(proc.pid);
  latencies.sort((a, b) => a - b);
  const p50 = latencies[Math.floor(latencies.length / 2)] ?? null;

  const result = {
    command: opts.command,
    pid: proc.pid,
    cold_start_ms: Math.round(coldStartMs * 100) / 100,
    idle_rss_mb: idleRssKb != null ? Math.round((idleRssKb / 1024) * 100) / 100 : null,
    load_rss_mb: loadRssKb != null ? Math.round((loadRssKb / 1024) * 100) / 100 : null,
    p50_tool_ms: p50 != null ? Math.round(p50 * 100) / 100 : null,
    tools_list_bytes: toolsListBytes,
    iterations: opts.iterations,
  };

  console.log(JSON.stringify(result, null, 2));
  client.close();
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
