// Control process for the shared extension host. Owns the control unix socket
// (LSP-style Content-Length framed JSON-RPC 2.0) and one worker_thread per open
// workspace. See the architecture contract for the control verbs.
import * as net from 'node:net';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Worker } from 'node:worker_threads';
import minimist from 'minimist';

const argv = minimist(process.argv.slice(2), { string: ['control-socket', 'extensions-dir', 'data-dir', 'home'] });
const controlSocket: string = argv['control-socket'];
const extensionsDir: string = argv['extensions-dir'];
const dataDir: string = argv['data-dir'] ?? process.env.IDE_DATA_DIR ?? '/tmp/ide-exthost-data';
const home: string = argv['home'] ?? process.env.HOME ?? dataDir;

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

interface WorkspaceHandle {
	id: string;
	worker: Worker;
	lspSocket: string;
	statsWaiters: Array<(heapUsed: number) => void>;
}

const workspaces = new Map<string, WorkspaceHandle>();
let counter = 0;

function openWorkspace(folders: string[]): Promise<{ workspaceId: string; lspSocket: string }> {
	const id = `ws-${++counter}`;
	const lspSocket = path.join(dataDir, `lsp-${id}.sock`);
	const extensionDirs = discoverExtensionDirs();
	const { promise, resolve, reject } = Promise.withResolvers<{ workspaceId: string; lspSocket: string }>();
	const worker = new Worker(workerPath, {
		workerData: { workspaceId: id, folders, dataDir, logsDir: path.join(dataDir, 'logs'), home, appRoot, version, extensionDirs, lspSocket },
	});
	const handle: WorkspaceHandle = { id, worker, lspSocket, statsWaiters: [] };
	workspaces.set(id, handle);
	worker.on('message', (msg: { type: string; message?: string; heapUsed?: number }) => {
		if (msg.type === 'ready') { resolve({ workspaceId: id, lspSocket }); }
		else if (msg.type === 'log') { console.error(msg.message); }
		else if (msg.type === 'stats') { handle.statsWaiters.splice(0).forEach((w) => w(msg.heapUsed ?? 0)); }
	});
	worker.on('error', (err) => { console.error(`[host] worker ${id} error:`, err); reject(err); });
	worker.on('exit', () => workspaces.delete(id));
	return promise;
}

async function closeWorkspace(workspaceId: string): Promise<void> {
	const handle = workspaces.get(workspaceId);
	if (!handle) { return; }
	handle.worker.postMessage({ type: 'close' });
	try { fs.unlinkSync(handle.lspSocket); } catch { /* already gone */ }
}

function workerHeap(handle: WorkspaceHandle): Promise<number> {
	const { promise, resolve } = Promise.withResolvers<number>();
	handle.statsWaiters.push(resolve);
	handle.worker.postMessage({ type: 'stats' });
	return promise;
}

async function stats(): Promise<{ workers: { workspaceId: string; heapUsed: number }[]; rss: number }> {
	const workers = await Promise.all([...workspaces.values()].map(async (h) => ({ workspaceId: h.id, heapUsed: await workerHeap(h) })));
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

process.on('SIGTERM', () => { server.close(); process.exit(0); });
process.on('SIGINT', () => { server.close(); process.exit(0); });
