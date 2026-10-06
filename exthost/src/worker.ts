// Per-workspace worker thread: boots the real VS Code ExtensionHostMain wired
// in-memory to our main-thread shim, then serves one LSP socket for the window.
// These two imports register all ExtHost* singletons (ILogService, extension
// service, etc.) via side effects and MUST run before ExtensionHostMain.
import '../.vscode-src/src/vs/workbench/api/common/extHost.common.services.js';
import '../.vscode-src/src/vs/workbench/api/node/extHost.node.services.js';
import * as net from 'node:net';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { parentPort, workerData } from 'node:worker_threads';
import { RPCProtocol, ExtHostContext, ExtensionHostMain, URI } from './vs.js';
import type { IHostUtils, IExtensionHostInitData } from './vs.js';
import { createInProcProtocolPair } from './inproc.js';
import { scanExtensions } from './scanner.js';
import { buildInitData } from './initdata.js';
import { buildConfiguration } from './config.js';
import { Session } from './session.js';
import { installMainShim } from './mainshim.js';
import { LspConnection } from './lsp.js';

interface WorkerData {
	workspaceId: string;
	folders: string[];
	dataDir: string;
	logsDir: string;
	home: string;
	appRoot: string;
	version: string;
	extensionDirs: string[];
	lspSocket: string;
}

const data = workerData as WorkerData;

function log(msg: string): void {
	const line = `[exthost ${data.workspaceId}] ${msg}`;
	if (parentPort) { parentPort.postMessage({ type: 'log', message: line }); }
	else { console.error(line); }
}

// A single extension throwing must not take down the shared host window. Log and
// keep serving (VS Code's real ext host likewise swallows extension errors).
process.on('uncaughtException', (err) => log(`uncaughtException: ${err instanceof Error ? err.stack ?? err.message : String(err)}`));
process.on('unhandledRejection', (reason) => log(`unhandledRejection: ${reason instanceof Error ? reason.stack ?? reason.message : String(reason)}`));

// An extension calling process.exit() must never tear down the shared host
// process (which hosts every window). Keep a private reference for our own
// controlled shutdown and neuter the public one.
const realExit = process.exit.bind(process);
process.exit = ((code?: number) => { log(`extension called process.exit(${code ?? 0}) — prevented`); }) as typeof process.exit;

function delay(ms: number): Promise<void> {
	const { promise, resolve } = Promise.withResolvers<void>();
	setTimeout(resolve, ms);
	return promise;
}

async function driveWhenReady<T>(call: () => Promise<T>, what: string): Promise<void> {
	for (let attempt = 0; attempt < 400; attempt++) {
		try { await call(); return; } catch { await delay(25); }
	}
	log(`timed out waiting for ext-host actor: ${what}`);
}

async function main(): Promise<void> {
	process.env.HOME = data.home;
	fs.mkdirSync(path.join(data.dataDir, 'User'), { recursive: true });
	fs.mkdirSync(data.logsDir, { recursive: true });

	const extensions = scanExtensions(data.extensionDirs);
	log(`scanned ${extensions.length} extensions`);
	const initData: IExtensionHostInitData = buildInitData({
		dataDir: data.dataDir, logsDir: data.logsDir, appRoot: data.appRoot, version: data.version, extensions,
	});
	const configuration = buildConfiguration(extensions, data.dataDir, data.folders);

	const { a: mainProtocol, b: extHostProtocol } = createInProcProtocolPair();
	const rpc = new RPCProtocol(mainProtocol, null, null);
	const session = new Session(rpc, data.workspaceId, data.folders, log);

	const reconfigure = (key: string, value: unknown): void => {
		const settingsFile = path.join(data.dataDir, 'User', 'settings.json');
		let current: Record<string, unknown> = {};
		try { current = JSON.parse(fs.readFileSync(settingsFile, 'utf8')); } catch { /* none */ }
		if (value === undefined) { delete current[key]; } else { current[key] = value; }
		fs.writeFileSync(settingsFile, JSON.stringify(current, null, 2));
		const next = buildConfiguration(extensions, data.dataDir, data.folders);
		session.proxy(ExtHostContext.ExtHostConfiguration).$acceptConfigurationChanged(next, { keys: [key], overrides: [] }).catch(() => { /* ignore */ });
	};

	installMainShim(session, { extensions, dataDir: data.dataDir, home: data.home, reconfigure });

	const hostUtils: IHostUtils = {
		_serviceBrand: undefined,
		pid: process.pid,
		exit() { /* never kill the shared host process from a single workspace */ },
		async fsExists(p: string) { return fs.promises.access(p).then(() => true, () => false); },
		async fsRealpath(p: string) { return fs.promises.realpath(p); },
	};

	const extHostMain = new ExtensionHostMain(extHostProtocol, initData, hostUtils, null);
	void extHostMain;

	// The ExtHost* actors register asynchronously during initialize(); retry until present.
	await driveWhenReady(() => session.proxy(ExtHostContext.ExtHostConfiguration).$initializeConfiguration(configuration) as unknown as Promise<void>, 'ExtHostConfiguration');
	const workspaceData = {
		id: data.workspaceId,
		name: path.basename(data.folders[0] ?? 'workspace'),
		folders: data.folders.map((f, index) => ({ uri: URI.file(f), name: path.basename(f), index })),
		configuration: data.folders.length > 1 ? URI.file(path.join(data.folders[0], 'ide.code-workspace')) : null,
		isUntitled: false,
		transient: false,
	};
	await driveWhenReady(() => session.proxy(ExtHostContext.ExtHostWorkspace).$initializeWorkspace(workspaceData, true) as unknown as Promise<void>, 'ExtHostWorkspace');
	log('workspace initialized; extension host starting');

	// Serve the LSP socket.
	try { fs.unlinkSync(data.lspSocket); } catch { /* fresh */ }
	const server = net.createServer((socket) => {
		log('LSP client connected');
		const connection = new LspConnection(session, socket);
		void connection;
		socket.on('close', () => session.detachClient());
	});
	server.listen(data.lspSocket, () => {
		log(`LSP socket listening at ${data.lspSocket}`);
		parentPort?.postMessage({ type: 'ready' });
	});

	parentPort?.on('message', (msg: { type: string }) => {
		if (msg.type === 'stats') {
			parentPort?.postMessage({ type: 'stats', heapUsed: process.memoryUsage().heapUsed, workspaceId: data.workspaceId });
		} else if (msg.type === 'close') {
			try { extHostMain.terminate('workspace closed'); } catch { /* ignore */ }
			server.close();
			realExit(0);
		}
	});
}

main().catch((err) => {
	log(`FATAL worker error: ${err instanceof Error ? err.stack ?? err.message : String(err)}`);
	realExit(1);
});
