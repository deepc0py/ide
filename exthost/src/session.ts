// Per-workspace shared state: the main-side RPC protocol, open-document mirror,
// language-feature registrations, registered commands, and the outbound sink to
// the connected LSP client (buffered until a client attaches).
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

export class Session {
	readonly documents = new Map<string, OpenDoc>();
	readonly features = new Map<number, FeatureRegistration>();
	readonly editorToUri = new Map<string, string>();
	readonly commands = new Set<string>();
	readonly activatedExtensions = new Set<string>();
	readonly contextKeys = new Map<string, unknown>();

	private _notify: OutboundNotify | null = null;
	private _request: OutboundRequest | null = null;
	private readonly _pending: Array<[string, unknown]> = [];

	constructor(
		readonly rpc: RPCProtocol,
		readonly workspaceId: string,
		readonly folders: string[],
		readonly log: (msg: string) => void,
	) { }

	proxy<T>(id: ProxyIdentifier<T>): Proxied<T> {
		return this.rpc.getProxy(id);
	}

	attachClient(notify: OutboundNotify, request: OutboundRequest): void {
		this._notify = notify;
		this._request = request;
		for (const [method, params] of this._pending.splice(0)) {
			notify(method, params);
		}
	}

	detachClient(): void {
		this._notify = null;
		this._request = null;
	}

	notify(method: string, params: unknown): void {
		if (this._notify) {
			this._notify(method, params);
		} else {
			this._pending.push([method, params]);
		}
	}

	request(method: string, params: unknown): Promise<unknown> {
		if (!this._request) {
			return Promise.reject(new Error(`[ide-exthost] no LSP client attached for request ${method}`));
		}
		return this._request(method, params);
	}
}
