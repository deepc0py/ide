// Shared-isolate state. One `Session` (the hub) owns the single extension host's
// global state — the main-side RPC protocol, open-document mirror, language-feature
// registrations, registered commands — which is shared across every open window,
// because all windows run in ONE extension host with ONE multi-root workspace.
//
// Each window is a `Workspace`: a per-window LSP socket plus the folders it
// contributed to the shared multi-root workspace. The hub routes outbound traffic
// (diagnostics, decorations) to the owning window by URI, broadcasts global events
// (commands, status bar, views), and sends requests to the primary window.
import type { Server } from 'node:net';
import { URI } from './vs.js';
import type { RPCProtocol } from './vs.js';
import type { ProxyIdentifier, Proxied } from '../.vscode-src/src/vs/workbench/services/extensions/common/proxyIdentifier.js';
import type { IDocumentFilterDto } from '../.vscode-src/src/vs/workbench/api/common/extHost.protocol.js';

export interface OpenDoc {
	uri: string;
	languageId: string;
	version: number;
	lines: string[];
	eol: string;
	editorId?: string;
}

export interface FeatureRegistration {
	handle: number;
	kind: string;
	selector: IDocumentFilterDto[];
	triggerCharacters?: string[];
	data?: Record<string, unknown>;
}

export type OutboundNotify = (method: string, params: unknown) => void;
export type OutboundRequest = (method: string, params: unknown) => Promise<unknown>;

// One open window: its LSP socket client plus the folders it contributed to the
// shared multi-root workspace. Outbound traffic is buffered until a client attaches.
export class Workspace {
	server: Server | null = null;

	private _notify: OutboundNotify | null = null;
	private _request: OutboundRequest | null = null;
	private readonly _pending: Array<[string, unknown]> = [];

	constructor(readonly id: string, public folders: string[]) { }

	get connected(): boolean { return this._notify !== null; }

	attachClient(notify: OutboundNotify, request: OutboundRequest): void {
		this._notify = notify;
		this._request = request;
		for (const [method, params] of this._pending.splice(0)) { notify(method, params); }
	}

	detachClient(): void {
		this._notify = null;
		this._request = null;
	}

	notify(method: string, params: unknown): void {
		if (this._notify) { this._notify(method, params); }
		else { this._pending.push([method, params]); }
	}

	request(method: string, params: unknown): Promise<unknown> {
		if (!this._request) { return Promise.reject(new Error(`[ide-exthost] no LSP client attached for request ${method}`)); }
		return this._request(method, params);
	}
}

export class Session {
	readonly documents = new Map<string, OpenDoc>();
	readonly features = new Map<number, FeatureRegistration>();
	readonly editorToUri = new Map<string, string>();
	readonly commands = new Set<string>();
	// Static command metadata (title/category) from every extension's
	// `contributes.commands`, so the command palette shows a human label and the
	// 'SonarQube' category rather than the raw command id.
	readonly commandMeta = new Map<string, { title: string; category: string }>();
	readonly activatedExtensions = new Set<string>();
	readonly contextKeys = new Map<string, unknown>();
	readonly workspaces = new Map<string, Workspace>();

	// Current global contribution state, retained so a window that opens *after*
	// extensions already contributed (late join) can be brought up to date. Keyed
	// for idempotent replace/remove.
	readonly statusBarItems = new Map<string, unknown>();
	readonly views = new Map<string, unknown>();
	readonly decorations = new Map<string, unknown>();
	// Owning window of a per-window resolved webview *view* handle, so its html /
	// options / messages route only to the window that resolved it (each window
	// resolves its own instance) instead of broadcasting to every window.
	readonly webviewOwners = new Map<string, Workspace>();

	// The window whose `workspace/executeCommand` is currently running. UI prompts
	// an extension raises while handling a command (showInputBox / showQuickPick /
	// modal showMessage) are routed back to this window so they render where the
	// user invoked the command, instead of always the primary window.
	activeWorkspace: Workspace | undefined = undefined;

	constructor(
		readonly rpc: RPCProtocol,
		readonly log: (msg: string) => void,
	) { }

	proxy<T>(id: ProxyIdentifier<T>): Proxied<T> {
		return this.rpc.getProxy(id);
	}

	// Every folder across every open window, in insertion order.
	get folders(): string[] {
		const out: string[] = [];
		for (const ws of this.workspaces.values()) { out.push(...ws.folders); }
		return out;
	}

	addWorkspace(ws: Workspace): void { this.workspaces.set(ws.id, ws); }
	removeWorkspace(id: string): void {
		const ws = this.workspaces.get(id);
		if (ws) {
			for (const [handle, owner] of this.webviewOwners) {
				if (owner === ws) { this.webviewOwners.delete(handle); }
			}
		}
		this.workspaces.delete(id);
	}

	// The window that owns a URI: the one whose folder is the longest path prefix.
	workspaceForUri(uri: string): Workspace | undefined {
		let fsPath: string;
		try { fsPath = URI.parse(uri).fsPath; } catch { fsPath = uri; }
		let best: Workspace | undefined;
		let bestLen = -1;
		for (const ws of this.workspaces.values()) {
			for (const folder of ws.folders) {
				const prefix = folder.endsWith('/') ? folder : `${folder}/`;
				if ((fsPath === folder || fsPath.startsWith(prefix)) && folder.length > bestLen) {
					best = ws;
					bestLen = folder.length;
				}
			}
		}
		return best;
	}

	// First connected window (fallback: first registered) — used for requests and
	// for routing URI-less traffic that has no natural owner.
	private primary(): Workspace | undefined {
		for (const ws of this.workspaces.values()) { if (ws.connected) { return ws; } }
		return this.workspaces.values().next().value;
	}

	// Route a notification to the window that owns `uri` (diagnostics, decorations).
	notifyUri(uri: string, method: string, params: unknown): void {
		(this.workspaceForUri(uri) ?? this.primary())?.notify(method, params);
	}

	// Send a global event to every window (commands, status bar, view registration).
	broadcast(method: string, params: unknown): void {
		for (const ws of this.workspaces.values()) { ws.notify(method, params); }
	}

	// Bring a window that opened after extensions already contributed up to date:
	// replay the current commands, status-bar items, registered views and the
	// decorations for documents this window owns. Buffered on the Workspace until
	// its LSP client attaches, so the ordering versus live events is preserved.
	replayTo(ws: Workspace): void {
		if (this.commands.size > 0) {
			ws.notify('ide/commands/changed', {
				commands: [...this.commands].map((c) => ({ id: c, title: c, category: '' })),
			});
		}
		for (const item of this.statusBarItems.values()) { ws.notify('ide/statusBar/set', item); }
		for (const view of this.views.values()) { ws.notify('ide/views/register', view); }
		for (const [uri, params] of this.decorations) {
			if (this.workspaceForUri(uri) === ws) { ws.notify('ide/decorations/set', params); }
		}
	}

	// Send a request to the owning window if known, else the primary window.
	request(method: string, params: unknown): Promise<unknown> {
		const ws = this.primary();
		if (!ws) { return Promise.reject(new Error(`[ide-exthost] no window attached for request ${method}`)); }
		return ws.request(method, params);
	}

	requestForUri(uri: string, method: string, params: unknown): Promise<unknown> {
		const ws = this.workspaceForUri(uri) ?? this.primary();
		if (!ws) { return Promise.reject(new Error(`[ide-exthost] no window attached for request ${method}`)); }
		return ws.request(method, params);
	}

	// Send a request to the window that invoked the current command (if any), else
	// the primary window. Used for interactive UI prompts (input box / quick pick
	// / modal message) that must render in the invoking window.
	requestActive(method: string, params: unknown): Promise<unknown> {
		const ws = this.activeWorkspace ?? this.primary();
		if (!ws) { return Promise.reject(new Error(`[ide-exthost] no window attached for request ${method}`)); }
		return ws.request(method, params);
	}

	// Contributed command entries with human title + category (falls back to the
	// id) for the command palette.
	commandEntries(): { id: string; title: string; category: string }[] {
		return [...this.commands].map((id) => {
			const meta = this.commandMeta.get(id);
			return { id, title: meta?.title ?? id, category: meta?.category ?? '' };
		});
	}
}
