// The shared extension-host isolate: ONE VS Code `ExtensionHostMain` wired
// in-memory to our main-thread shim, hosting EVERY open window as a single
// multi-root workspace. Each window contributes its folder(s) and gets its own
// LSP socket; the hub (`Session`) routes traffic per window. This is what keeps
// memory flat: each extension and each language server (rust-analyzer, eslint, …)
// loads ONCE for all windows instead of once per window.
//
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
import { Session, Workspace } from './session.js';
import { installMainShim } from './mainshim.js';
import { LspConnection } from './lsp.js';

interface WorkerData {
	dataDir: string;
	logsDir: string;
	home: string;
	appRoot: string;
	version: string;
	extensionDirs: string[];
}

const data = workerData as WorkerData;

function log(msg: string): void {
	const line = `[exthost] ${msg}`;
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

	const { a: mainProtocol, b: extHostProtocol } = createInProcProtocolPair();
	const rpc = new RPCProtocol(mainProtocol, null, null);
	const session = new Session(rpc, log);

	const reconfigure = (key: string, value: unknown): void => {
		const settingsFile = path.join(data.dataDir, 'User', 'settings.json');
		let current: Record<string, unknown> = {};
		try { current = JSON.parse(fs.readFileSync(settingsFile, 'utf8')); } catch { /* none */ }
		if (value === undefined) { delete current[key]; } else { current[key] = value; }
		fs.writeFileSync(settingsFile, JSON.stringify(current, null, 2));
		const next = buildConfiguration(extensions, data.dataDir, session.folders);
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

	// Configuration is window-independent (settings are global); initialize once.
	const configuration = buildConfiguration(extensions, data.dataDir, []);
	await driveWhenReady(() => session.proxy(ExtHostContext.ExtHostConfiguration).$initializeConfiguration(configuration) as unknown as Promise<void>, 'ExtHostConfiguration');
	log('configuration initialized; ready for windows');

	let workspaceInitialized = false;

	// Push the current multi-root folder set into the ext host. The first call
	// initializes the workspace (kicking off eager + workspaceContains activation);
	// later calls fire onDidChangeWorkspace({added,removed}), which the ext host uses
	// to run workspaceContains activation for newly added folders (e.g. a window that
	// opens a Cargo project after startup) and to let extensions drop closed folders.
	const syncWorkspace = async (): Promise<void> => {
		const folders = session.folders;
		const workspaceData = {
			id: 'ide-shared',
			name: 'ide',
			folders: folders.map((f, index) => ({ uri: URI.file(f), name: path.basename(f), index })),
			configuration: URI.file(path.join(data.dataDir, 'ide.code-workspace')),
			isUntitled: false,
			transient: false,
		};
		if (!workspaceInitialized) {
			await driveWhenReady(() => session.proxy(ExtHostContext.ExtHostWorkspace).$initializeWorkspace(workspaceData, true) as unknown as Promise<void>, 'ExtHostWorkspace');
			workspaceInitialized = true;
		} else {
			session.proxy(ExtHostContext.ExtHostWorkspace).$acceptWorkspaceData(workspaceData);
		}
	};

	const openWorkspace = async (id: string, folders: string[], lspSocket: string): Promise<void> => {
		const ws = new Workspace(id, folders);
		session.addWorkspace(ws);
		// Bring this window up to date with contributions extensions already made
		// (status bar, commands, views, decorations) for late-joining windows.
		session.replayTo(ws);
		await syncWorkspace();
		try { fs.unlinkSync(lspSocket); } catch { /* fresh */ }
		const server = net.createServer((socket) => {
			log(`LSP client connected (${id})`);
			const connection = new LspConnection(session, ws, socket);
			void connection;
			socket.on('close', () => ws.detachClient());
		});
		ws.server = server;
		await new Promise<void>((resolve) => server.listen(lspSocket, resolve));
		log(`window ${id} open: ${folders.join(', ')}`);
		parentPort?.postMessage({ type: 'workspaceReady', id });
	};

	const closeWorkspace = async (id: string): Promise<void> => {
		const ws = session.workspaces.get(id);
		if (!ws) { return; }
		ws.server?.close();
		session.removeWorkspace(id);
		// Remove this window's documents so they stop resolving / routing.
		for (const [uri] of session.documents) {
			if (session.workspaceForUri(uri) === undefined) { session.documents.delete(uri); }
		}
		await syncWorkspace();
		log(`window ${id} closed`);
	};

	parentPort?.on('message', (msg: { type: string; id?: string; folders?: string[]; lspSocket?: string }) => {
		if (msg.type === 'openWorkspace') {
			openWorkspace(msg.id!, msg.folders ?? [], msg.lspSocket!).catch((err) => log(`openWorkspace ${msg.id} failed: ${String(err)}`));
		} else if (msg.type === 'closeWorkspace') {
			closeWorkspace(msg.id!).catch((err) => log(`closeWorkspace ${msg.id} failed: ${String(err)}`));
		} else if (msg.type === 'stats') {
			parentPort?.postMessage({ type: 'stats', heapUsed: process.memoryUsage().heapUsed });
		} else if (msg.type === 'close') {
			try { extHostMain.terminate('host shutdown'); } catch { /* ignore */ }
			realExit(0);
		}
	});

	parentPort?.postMessage({ type: 'booted' });
}

main().catch((err) => {
	log(`FATAL worker error: ${err instanceof Error ? err.stack ?? err.message : String(err)}`);
	realExit(1);
});
