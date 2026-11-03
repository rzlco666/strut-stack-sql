#!/usr/bin/env node
/**
 * Official Cursor MCP launcher for strut-stack-sql.
 *
 * Loads workspace .env when present, warns (does not exit) if DB is missing
 * or unreachable, then always spawns the binary so MCP initialize succeeds.
 * DB connect stays lazy inside the server.
 *
 * ~/.cursor/mcp.json:
 *   "command": "node",
 *   "args": ["/path/to/packaging/cursor-mcp-launcher.mjs", "${workspaceFolder}"]
 */
import { spawn } from 'child_process';
import { createConnection } from 'net';
import { existsSync, readFileSync } from 'fs';
import { dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';

const PREFLIGHT_TIMEOUT_MS = Number(process.env.MCP_SQL_PREFLIGHT_MS || 500);
const EXTRA_ENV_DIRS = (
  process.env.MCP_SQL_ENV_SUBDIRS ||
  'apps/api,apps/backend,backend,server,api,db'
)
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean);

const DEFAULT_ARGS = (process.env.MCP_SQL_ARGS || '--full-tools')
  .split(/\s+/)
  .filter(Boolean);

function candidateBins() {
  const envBin =
    process.env.STRUT_STACK_SQL_BIN ||
    process.env.MCP_SQL_BIN;
  const home = process.env.HOME || '';
  return [
    envBin,
    'strut-stack-sql',
    home && join(home, '.local/bin/strut-stack-sql'),
    home && join(home, '.cargo/bin/strut-stack-sql'),
  ].filter(Boolean);
}

function resolveBin() {
  for (const cand of candidateBins()) {
    if (!cand.includes('/') && !cand.includes('\\')) {
      return cand; // PATH lookup by spawn
    }
    if (existsSync(cand)) return cand;
  }
  return 'strut-stack-sql';
}

function parseDotenv(text) {
  const vars = {};
  for (const rawLine of text.split('\n')) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#')) continue;
    const eq = line.indexOf('=');
    if (eq <= 0) continue;
    const key = line.slice(0, eq).trim();
    let value = line.slice(eq + 1).trim();
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }
    vars[key] = value;
  }
  return vars;
}

function hasDbCredentials(vars) {
  if (vars.DATABASE_URL) return true;
  if (vars.POSTGRES_URL || vars.MYSQL_URL || vars.SQLITE_URL) return true;
  if (vars.MYSQL_HOST && vars.MYSQL_USER && (vars.MYSQL_DATABASE || vars.MYSQL_DB)) {
    return true;
  }
  if (vars.POSTGRES_HOST && vars.POSTGRES_USER && vars.POSTGRES_DB) return true;
  return false;
}

function resolveDatabaseUrl(vars) {
  if (vars.DATABASE_URL) return vars.DATABASE_URL;
  if (vars.MYSQL_URL) return vars.MYSQL_URL;
  if (vars.POSTGRES_URL) return vars.POSTGRES_URL;
  if (vars.SQLITE_URL) return vars.SQLITE_URL;
  if (vars.MYSQL_HOST && vars.MYSQL_USER) {
    const host = vars.MYSQL_HOST;
    const user = vars.MYSQL_USER;
    const password = vars.MYSQL_PASSWORD || '';
    const db = vars.MYSQL_DATABASE || vars.MYSQL_DB || '';
    const port = vars.MYSQL_PORT || '3306';
    return `mysql://${user}:${password}@${host}:${port}/${db}`;
  }
  if (vars.POSTGRES_HOST && vars.POSTGRES_USER && vars.POSTGRES_DB) {
    const host = vars.POSTGRES_HOST;
    const user = vars.POSTGRES_USER;
    const password = vars.POSTGRES_PASSWORD || '';
    const db = vars.POSTGRES_DB;
    const port = vars.POSTGRES_PORT || '5432';
    return `postgresql://${user}:${password}@${host}:${port}/${db}`;
  }
  return null;
}

function loadEnvFile(envPath) {
  if (!existsSync(envPath)) return null;
  const vars = parseDotenv(readFileSync(envPath, 'utf8'));
  if (!hasDbCredentials(vars)) return null;
  return { envPath, vars, dir: dirname(envPath) };
}

function findDotenv(workspace) {
  const root = resolve(workspace);
  // Prefer nested app env (e.g. apps/api) over root when both exist.
  const preferred = EXTRA_ENV_DIRS.map((sub) => join(root, sub, '.env'));
  const rootEnv = join(root, '.env');
  for (const envPath of [...preferred, rootEnv]) {
    const found = loadEnvFile(envPath);
    if (found) return found;
  }
  let dir = root;
  while (true) {
    const found = loadEnvFile(join(dir, '.env'));
    if (found) return found;
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return null;
}

function parseHostPort(url) {
  if (!url) return null;
  const lower = url.toLowerCase();
  let rest = url;
  if (lower.startsWith('mysql://')) rest = url.slice('mysql://'.length);
  else if (lower.startsWith('postgresql://')) rest = url.slice('postgresql://'.length);
  else if (lower.startsWith('postgres://')) rest = url.slice('postgres://'.length);
  else if (lower.startsWith('sqlite:')) return null;
  else return null;

  const at = rest.lastIndexOf('@');
  const hostPart = at >= 0 ? rest.slice(at + 1) : rest;
  const slash = hostPart.indexOf('/');
  const hostPort = slash >= 0 ? hostPart.slice(0, slash) : hostPart;
  const [host, port] = hostPort.includes(':')
    ? hostPort.split(':')
    : [hostPort, lower.startsWith('mysql') ? '3306' : '5432'];
  if (!host) return null;
  return { host, port: Number(port) };
}

function tcpPreflight(host, port) {
  return new Promise((resolvePromise) => {
    const socket = createConnection({ host, port });
    const timer = setTimeout(() => {
      socket.destroy();
      resolvePromise(false);
    }, PREFLIGHT_TIMEOUT_MS);
    socket.on('connect', () => {
      clearTimeout(timer);
      socket.end();
      resolvePromise(true);
    });
    socket.on('error', () => {
      clearTimeout(timer);
      resolvePromise(false);
    });
  });
}

const workspaceArg = process.argv[2];
const workspace =
  workspaceArg && workspaceArg !== '${workspaceFolder}'
    ? resolve(workspaceArg)
    : resolve(process.cwd());

const found = findDotenv(workspace);
const childEnv = { ...process.env };
let cwd = workspace;
let rustArgs = [...DEFAULT_ARGS, '--workspace', workspace];

if (!found) {
  console.error(
    `strut-stack-sql launcher: no DATABASE_URL under ${workspace} (checked: ${EXTRA_ENV_DIRS.join(', ')}, .env). Starting anyway — server uses lazy connect / memory fallback.`,
  );
} else {
  Object.assign(childEnv, found.vars);
  cwd = found.dir;
  rustArgs = [...DEFAULT_ARGS, '--workspace', found.dir];
  const dbUrl = resolveDatabaseUrl(found.vars);
  const endpoint = parseHostPort(dbUrl);
  if (endpoint && process.env.MCP_SQL_SKIP_PREFLIGHT !== '1') {
    const ok = await tcpPreflight(endpoint.host, endpoint.port);
    if (!ok) {
      console.error(
        `strut-stack-sql launcher: cannot reach ${endpoint.host}:${endpoint.port} (${found.envPath}) — starting anyway (lazy connect).`,
      );
    }
  }
}

const MCP_BIN = resolveBin();
if (MCP_BIN.includes('/') && !existsSync(MCP_BIN)) {
  console.error(`strut-stack-sql launcher: binary not found: ${MCP_BIN}`);
  process.exit(1);
}

const child = spawn(MCP_BIN, rustArgs, {
  stdio: 'inherit',
  env: childEnv,
  cwd,
});

child.on('error', (err) => {
  console.error(`strut-stack-sql launcher: failed to spawn ${MCP_BIN}: ${err.message}`);
  process.exit(1);
});

child.on('exit', (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  process.exit(code ?? 0);
});
