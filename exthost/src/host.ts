// Control process for the shared extension host. Owns the control unix socket
// (LSP-style Content-Length framed JSON-RPC 2.0) and ONE shared worker_thread (the
// extension-host isolate) that hosts every open window as a single multi-root
// workspace. Each `host/openWorkspace` still returns a per-window LSP socket, so
// the Rust client's control/LSP contract is unchanged; internally the windows
// share one extension host (so each extension + language server loads once).
import * as net from 'node:net';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Worker } from 'node:worker_threads';
import minimist from 'minimist';

const argv = minimist(process.argv.slice(2), { string: ['control-socket', 'extensions-dir', 'data-dir', 'home', 'parent-pid'] });
const controlSocket: string = argv['control-socket'];
const extensionsDir: string = argv['extensions-dir'];
const dataDir: string = argv['data-dir'] ?? process.env.IDE_DATA_DIR ?? '/tmp/ide-exthost-data';
const home: string = argv['home'] ?? process.env.HOME ?? dataDir;
const parentPid: number = Number.parseInt(String(argv['parent-pid'] ?? '0'), 10) || 0;

if (!controlSocket || !extensionsDir) {
	console.error('usage: node host.js --control-socket <path> --extensions-dir <dir> --data-dir <dir> [--home <dir>]');
	process.exit(2);
}

const here = path.dirname(fileURLToPath(import.meta.url));
const workerPath = path.join(here, 'worker.js');
const appRoot = path.resolve(here, '..');
const version = readVersion();

function readVersion(): string {
	try { return JSON.parse(fs.readFileSync(path.join(appRoot, '.vscode-src', 'package.json'), 'utf8')).version ?? '1.0.0'; }
	catch { return '1.0.0'; }
}

// Every immediate child of extensionsDir that has a package.json is an extension.
function discoverExtensionDirs(): string[] {
	const out: string[] = [];
	let entries: fs.Dirent[] = [];
	try { entries = fs.readdirSync(extensionsDir, { withFileTypes: true }); } catch { return out; }
	for (const ent of entries) {
		if (!ent.isDirectory()) { continue; }
		const dir = path.join(extensionsDir, ent.name);
		if (fs.existsSync(path.join(dir, 'package.json'))) { out.push(dir); }
	}
	return out;
}

// ---- the single shared extension-host worker --------------------------------

interface WorkspaceHandle { id: string; lspSocket: string; folders: string[]; }

const workspaces = new Map<string, WorkspaceHandle>();
const readyWaiters = new Map<string, () => void>();
const statsWaiters: Array<(heapUsed: number) => void> = [];
let worker: Worker | null = null;
let booted: Promise<void> | null = null;
let counter = 0;

function ensureWorker(): Promise<void> {
	if (booted) { return booted; }
	const extensionDirs = discoverExtensionDirs();
	const w = new Worker(workerPath, {
		workerData: { dataDir, logsDir: path.join(dataDir, 'logs'), home, appRoot, version, extensionDirs },
	});
	worker = w;
	const { promise, resolve, reject } = Promise.withResolvers<void>();
	booted = promise;
	w.on('message', (msg: { type: string; id?: string; message?: string; heapUsed?: number }) => {
		if (msg.type === 'booted') { resolve(); }
		else if (msg.type === 'log') { console.error(msg.message); }
		else if (msg.type === 'workspaceReady') { readyWaiters.get(msg.id ?? '')?.(); readyWaiters.delete(msg.id ?? ''); }
		else if (msg.type === 'stats') { statsWaiters.splice(0).forEach((fn) => fn(msg.heapUsed ?? 0)); }
	});
	w.on('error', (err) => { console.error('[host] isolate worker error:', err); reject(err); });
	w.on('exit', () => { worker = null; booted = null; });
	return promise;
}

async function openWorkspace(folders: string[]): Promise<{ workspaceId: string; lspSocket: string }> {
	await ensureWorker();
	const id = `ws-${++counter}`;
	const lspSocket = path.join(dataDir, `lsp-${id}.sock`);
	workspaces.set(id, { id, lspSocket, folders });
	const { promise, resolve } = Promise.withResolvers<void>();
	readyWaiters.set(id, resolve);
	worker!.postMessage({ type: 'openWorkspace', id, folders, lspSocket });
	await promise;
	return { workspaceId: id, lspSocket };
}

async function closeWorkspace(workspaceId: string): Promise<void> {
	const handle = workspaces.get(workspaceId);
	if (!handle) { return; }
	worker?.postMessage({ type: 'closeWorkspace', id: workspaceId });
	workspaces.delete(workspaceId);
	try { fs.unlinkSync(handle.lspSocket); } catch { /* already gone */ }
}

async function stats(): Promise<{ workers: { workspaceId: string; heapUsed: number }[]; rss: number }> {
	let heapUsed = 0;
	if (worker) {
		const { promise, resolve } = Promise.withResolvers<number>();
		statsWaiters.push(resolve);
		worker.postMessage({ type: 'stats' });
		heapUsed = await promise;
	}
	// One shared isolate: every window reports the same (shared) V8 heap. `rss` is the
	// whole host process (the isolate runs as a thread inside it).
	const workers = [...workspaces.keys()].map((workspaceId) => ({ workspaceId, heapUsed }));
	return { workers, rss: process.memoryUsage().rss };
}

// ---- control socket (JSON-RPC 2.0, Content-Length framed) -------------------

interface RpcMessage { id?: number | string; method?: string; params?: unknown; result?: unknown; error?: unknown; }

function handleConnection(socket: net.Socket): void {
	let buffer = Buffer.alloc(0);
	const write = (msg: RpcMessage): void => {
		const payload = Buffer.from(JSON.stringify({ jsonrpc: '2.0', ...msg }), 'utf8');
		socket.write(`Content-Length: ${payload.length}\r\n\r\n`);
		socket.write(payload);
	};
	socket.on('data', (chunk) => {
		buffer = Buffer.concat([buffer, chunk]);
		for (;;) {
			const headerEnd = buffer.indexOf('\r\n\r\n');
			if (headerEnd === -1) { return; }
			const match = /Content-Length:\s*(\d+)/i.exec(buffer.subarray(0, headerEnd).toString('utf8'));
			const bodyStart = headerEnd + 4;
			if (!match) { buffer = buffer.subarray(bodyStart); continue; }
			const length = Number(match[1]);
			if (buffer.length < bodyStart + length) { return; }
			const body = buffer.subarray(bodyStart, bodyStart + length).toString('utf8');
			buffer = buffer.subarray(bodyStart + length);
			let msg: RpcMessage;
			try { msg = JSON.parse(body); } catch { continue; }
			if (msg.method && msg.id !== undefined) {
				dispatch(msg.method, msg.params).then(
					(result) => write({ id: msg.id, result }),
					(err) => write({ id: msg.id, error: { code: -32603, message: String(err) } }),
				);
			}
		}
	});
}

async function dispatch(method: string, params: unknown): Promise<unknown> {
	const p = (params ?? {}) as { folders?: string[]; workspaceId?: string };
	switch (method) {
		case 'host/openWorkspace': return openWorkspace(p.folders ?? []);
		case 'host/closeWorkspace': await closeWorkspace(p.workspaceId ?? ''); return {};
		case 'host/stats': return stats();
		default: throw new Error(`unknown control method: ${method}`);
	}
}

fs.mkdirSync(dataDir, { recursive: true });
try { fs.unlinkSync(controlSocket); } catch { /* fresh */ }
const server = net.createServer(handleConnection);
server.listen(controlSocket, () => console.error(`[host] control socket listening at ${controlSocket}`));

let shuttingDown = false;
function shutdown(code: number): void {
	if (shuttingDown) { return; }
	shuttingDown = true;
	try { worker?.postMessage({ type: 'close' }); } catch { /* ignore */ }
	try { server.close(); } catch { /* ignore */ }
	process.exit(code);
}

process.on('SIGTERM', () => shutdown(0));
process.on('SIGINT', () => shutdown(0));

// When launched by the IDE/proxy we are told its pid. If that process dies
// (even via SIGKILL, so it never gets to kill us), take the whole process group
// down: otherwise an orphaned host keeps an inherited stdout pipe open and hangs
// the parent (e.g. `cargo test`). We were started as our own group leader, so a
// negative pid signals the group (host + language servers).
if (parentPid > 0) {
	const watchdog = setInterval(() => {
		let alive = true;
		try { process.kill(parentPid, 0); } catch { alive = false; }
		if (!alive) {
			try { worker?.postMessage({ type: 'close' }); } catch { /* ignore */ }
			try { process.kill(-process.pid, 'SIGKILL'); } catch { /* ignore */ }
			process.exit(0);
		}
	}, 1000);
	watchdog.unref();
}
