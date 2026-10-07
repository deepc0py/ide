// Reproducible memory measurement for the shared extension host.
//
//   node scripts/measure.mjs [--workspaces N] [--worktrees DIR,DIR,...] [--settle S]
//
// Boots dist/host.js, opens N workspaces over the control socket, opens one
// representative document per workspace to trigger language activation, lets the
// tree settle, then sums the RSS of the host process AND ALL its descendants
// (child language servers: eslint server, tsserver, rust-analyzer, jedi, git,
// ...), exactly like bench/membench.py. Prints a per-process breakdown plus the
// host's own `host/stats` (RSS + per-worker heapUsed).
//
// Exit 0 if tree RSS <= --limit-mb (default 1500), else 1.
import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, rmSync, writeFileSync, chmodSync, readdirSync, readFileSync } from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { ensureExtensions } from '../test/extensions.mjs';
import { connect, sleep } from '../test/client.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const HOST = path.join(here, '..', 'dist', 'host.js');
// Captured before we point the host's HOME at /tmp — rust-analyzer's cargo/rustc
// are rustup shims that resolve the toolchain via $RUSTUP_HOME (default ~/.rustup).
const REAL_HOME = process.env.HOME || '';

function parseArgs() {
  const a = { workspaces: 8, settle: 25, limitMb: 1500, openDocs: true };
  const argv = process.argv.slice(2);
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    if (k === '--workspaces') { a.workspaces = Number(argv[++i]); }
    else if (k === '--settle') { a.settle = Number(argv[++i]); }
    else if (k === '--limit-mb') { a.limitMb = Number(argv[++i]); }
    else if (k === '--worktrees') { a.worktrees = argv[++i].split(','); }
    else if (k === '--no-docs') { a.openDocs = false; }
    else if (k === '--sonarlint') { a.sonarlint = true; }
  }
  return a;
}

// RSS (KB) of `pid` and every descendant, via one ps snapshot.
function treeRssKb(rootPid) {
  const out = execFileSync('ps', ['-axo', 'pid=,ppid=,rss=,command='], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
  const procs = new Map();
  const children = new Map();
  for (const line of out.split('\n')) {
    const m = line.match(/^\s*(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/);
    if (!m) { continue; }
    const pid = +m[1], ppid = +m[2], rss = +m[3], cmd = m[4];
    procs.set(pid, { ppid, rss, cmd });
    if (!children.has(ppid)) { children.set(ppid, []); }
    children.get(ppid).push(pid);
  }
  const seen = new Set();
  const stack = [rootPid];
  while (stack.length) {
    const pid = stack.pop();
    if (seen.has(pid) || !procs.has(pid)) { continue; }
    seen.add(pid);
    for (const c of (children.get(pid) ?? [])) { stack.push(c); }
  }
  const rows = [...seen].map((p) => ({ pid: p, rss: procs.get(p).rss, cmd: procs.get(p).cmd }))
    .sort((x, y) => y.rss - x.rss);
  return { totalKb: rows.reduce((s, r) => s + r.rss, 0), rows };
}

// Pick one representative source file in a worktree to open (triggers onLanguage).
function pickDoc(dir) {
  // Prefer a deterministic TypeScript file (representative of a vscode worktree;
  // avoids spuriously activating python/rust-analyzer). Fall back to a shallow walk.
  for (const rel of ['src/vs/base/common/uri.ts', 'src/main.ts', 'src/index.ts']) {
    const p = path.join(dir, rel);
    if (existsSync(p)) { return p; }
  }
  const byExt = { ts: null, js: null, py: null, rs: null };
  const walk = (d, depth) => {
    if (depth > 4 || byExt.ts) { return; }
    let ents;
    try { ents = readdirSync(d, { withFileTypes: true }); } catch { return; }
    for (const e of ents) {
      if (byExt.ts) { return; }
      if (e.name === 'node_modules' || e.name === '.git' || e.name.startsWith('.')) { continue; }
      const full = path.join(d, e.name);
      const m = e.isFile() && /\.(ts|js|py|rs)$/.exec(e.name);
      if (m && !byExt[m[1]]) { byExt[m[1]] = full; }
      else if (e.isDirectory()) { walk(full, depth + 1); }
    }
  };
  walk(dir, 0);
  return byExt.ts ?? byExt.js ?? byExt.py ?? byExt.rs ?? undefined;
}

const langOf = (f) => (f.endsWith('.ts') ? 'typescript' : f.endsWith('.js') ? 'javascript' : f.endsWith('.py') ? 'python' : f.endsWith('.rs') ? 'rust' : 'plaintext');

// The SonarLint language server runs as a bundled-JRE Java process inside the
// host tree. There must be exactly ONE for all windows (shared isolate), and —
// unlike the external SonarQube server, which is a separate container excluded
// from the IDE budget — this JVM IS part of the host tree and counted here.
function summarizeSonarLint(rows) {
  const jvms = rows.filter((r) => /java/i.test(r.cmd) && /sonarlint-ls\.jar|sonarlint/i.test(r.cmd));
  return { count: jvms.length, totalKb: jvms.reduce((s, r) => s + r.rss, 0), rows: jvms };
}

// Generate N trivial single-file TS worktrees (each with an obvious code smell)
// used to exercise the SonarLint language server across many windows.
function makeSonarWorktrees(n) {
  const dirs = [];
  for (let i = 1; i <= n; i++) {
    const dir = `/tmp/ide-sonarlint-wt-${i}`;
    rmSync(dir, { recursive: true, force: true });
    mkdirSync(dir, { recursive: true });
    writeFileSync(path.join(dir, 'index.ts'), 'export function f(x: number) {\n  if (x == x) { return 1; }\n  let unused = 42;\n  return 0;\n}\n');
    dirs.push(dir);
  }
  return dirs;
}

async function main() {
  const args = parseArgs();
  const worktrees = args.worktrees ?? (args.sonarlint ? makeSonarWorktrees(args.workspaces) : Array.from({ length: args.workspaces }, (_, i) => `/tmp/wt/vscode-${i + 1}`));
  const N = worktrees.length;

  const DATA = '/tmp/ide-exthost-measure-data';
  const HOME = '/tmp/ide-exthost-measure-home';
  const CONTROL = path.join(DATA, 'control.sock');

  const extMap = await ensureExtensions();
  for (const dir of Object.values(extMap)) {
    const ra = path.join(dir, 'server', 'rust-analyzer');
    if (existsSync(ra)) { try { chmodSync(ra, 0o755); } catch { /* ignore */ } }
  }
  const extensionsDir = path.dirname(Object.values(extMap)[0]);

  rmSync(DATA, { recursive: true, force: true });
  mkdirSync(path.join(DATA, 'User'), { recursive: true });
  mkdirSync(HOME, { recursive: true });
  // Shipped product defaults only — NO memory-saving overrides here. The
  // low-memory levers (rust-analyzer.cachePriming/checkOnSave/buildScripts/
  // procMacro off, python Jedi) now live in the host's default configuration
  // layer (exthost/src/config.ts PRODUCT_DEFAULT_SETTINGS), so this measurement
  // reflects exactly what users get and agrees with bench/membench.py.
  writeFileSync(path.join(DATA, 'User', 'settings.json'), JSON.stringify({
    'telemetry.telemetryLevel': 'off',
  }, null, 2));

  const env = {
    ...process.env, HOME, IDE_DATA_DIR: DATA,
    CARGO_HOME: '/tmp/ide-exthost-measure-cargo',
    RUSTUP_HOME: process.env.RUSTUP_HOME || path.join(REAL_HOME, '.rustup'),
  };
  const proc = spawn('node', [HOST, '--control-socket', CONTROL, '--extensions-dir', extensionsDir, '--data-dir', DATA, '--home', HOME], {
    stdio: ['ignore', 'ignore', 'inherit'], env,
  });
  process.on('exit', () => { try { proc.kill('SIGKILL'); } catch { /* ignore */ } });
  await sleep(1500);

  const control = connect(CONTROL);
  await control.ready();

  const lsps = [];
  for (let i = 0; i < N; i++) {
    const folder = worktrees[i];
    const open = await control.request('host/openWorkspace', { folders: [folder] });
    const lsp = connect(open.lspSocket);
    await lsp.ready();
    lsp.onServerRequest('window/showMessageRequest', () => null);
    lsp.onServerRequest('workspace/applyEdit', () => ({ applied: true }));
    await lsp.request('initialize', {
      processId: process.pid, rootUri: pathToFileURL(folder).href,
      workspaceFolders: [{ uri: pathToFileURL(folder).href, name: path.basename(folder) }], capabilities: {},
    });
    lsp.notify('initialized', {});
    if (args.openDocs) {
      const doc = pickDoc(folder);
      if (doc) {
        try { lsp.notify('textDocument/didOpen', { textDocument: { uri: pathToFileURL(doc).href, languageId: langOf(doc), version: 1, text: readFileSync(doc, 'utf8') } }); } catch { /* ignore */ }
      }
    }
    lsps.push({ lsp, open });
    process.stderr.write(`[measure] opened workspace ${i + 1}/${N}: ${folder}\n`);
    await sleep(500);
  }

  process.stderr.write(`[measure] settling ${args.settle}s ...\n`);
  // Sample a few times; keep the peak.
  let peak = { totalKb: 0, rows: [] };
  const samples = 5;
  const per = Math.max(1, Math.floor((args.settle * 1000) / samples));
  for (let s = 0; s < samples; s++) {
    await sleep(per);
    const snap = treeRssKb(proc.pid);
    if (snap.totalKb > peak.totalKb) { peak = snap; }
  }

  let stats = null;
  try { stats = await control.request('host/stats', {}); } catch { /* ignore */ }

  console.log('\n=== host process tree (peak of ' + samples + ' samples) ===');
  for (const r of peak.rows.slice(0, 40)) {
    console.log(`${(r.rss / 1024).toFixed(1).padStart(9)} MB  ${String(r.pid).padStart(7)}  ${r.cmd.slice(0, 110)}`);
  }
  const mb = peak.totalKb / 1024;
  console.log(`\nPROCESSES ${peak.rows.length}  HOST_TREE_RSS_MB ${mb.toFixed(1)}  LIMIT_MB ${args.limitMb}`);
  if (stats) {
    console.log(`host/stats: rss=${(stats.rss / 1048576).toFixed(1)} MB  workers=${stats.workers.length}  heapUsed=[${stats.workers.map((w) => (w.heapUsed / 1048576).toFixed(0)).join(', ')}] MB`);
  }

  const sl = summarizeSonarLint(peak.rows);
  console.log(`SONARLINT_JVM processes=${sl.count}  rss=${(sl.totalKb / 1024).toFixed(1)} MB  (expected: one shared JVM for all ${N} windows)`);
  for (const r of sl.rows) {
    console.log(`  jvm ${String(r.pid).padStart(7)}  ${(r.rss / 1024).toFixed(1)} MB  ${r.cmd.slice(0, 90)}`);
  }

  for (const { lsp } of lsps) { lsp.close(); }
  control.close();
  proc.kill('SIGTERM');
  await sleep(500);
  process.exit(mb <= args.limitMb ? 0 : 1);
}

main().catch((err) => { console.error(err); process.exit(2); });
