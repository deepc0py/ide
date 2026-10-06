// Per-workspace LSP server: a Content-Length framed JSON-RPC 2.0 endpoint on a
// unix socket that maps standard LSP requests to the extension host's registered
// providers (via the ExtHost* proxies) and surfaces the custom `ide/*` protocol.
import type { Socket } from 'node:net';
import { CancellationToken, ExtHostContext, URI } from './vs.js';
import type { Session, Workspace, FeatureRegistration } from './session.js';
import type { IDocumentFilterDto } from '../.vscode-src/src/vs/workbench/api/common/extHost.protocol.js';
import type { UriComponents } from './vs.js';
import {
	toInternalPosition, toInternalRange, hoverToLsp, locationLinksToLsp, locationsToLsp,
	textEditsToLsp, suggestItemToLsp, workspaceEditToLsp, documentSymbolToLsp,
} from './convert.js';
import { ISuggestResultDtoField } from './vs.js';
import { globToRegExp } from './glob.js';

interface JsonRpcMessage {
	jsonrpc?: string;
	id?: number | string;
	method?: string;
	params?: unknown;
	result?: unknown;
	error?: { code: number; message: string };
}

const COMPLETION_TRIGGER_CHARS = ['.', ':', '>', '"', "'", '/', '@', '<', ' ', '(', ',', '='];

// Match a DocumentFilter.pattern (glob string or RelativePattern) against a path.
function patternMatches(pattern: NonNullable<IDocumentFilterDto['pattern']>, fsPath: string): boolean {
	let glob: string;
	let base: string | undefined;
	if (typeof pattern === 'string') {
		glob = pattern;
	} else {
		glob = pattern.pattern;
		const dto = pattern as { base?: string; baseUri?: UriComponents };
		base = dto.baseUri ? URI.revive(dto.baseUri).fsPath : dto.base;
	}
	let target = fsPath;
	if (base) {
		const prefix = base.endsWith('/') ? base : `${base}/`;
		if (fsPath !== base && !fsPath.startsWith(prefix)) { return false; }
		target = fsPath.slice(base.length).replace(/^[/\\]+/, '');
	}
	if (globToRegExp(glob).test(target)) { return true; }
	return !glob.startsWith('/') && globToRegExp(`**/${glob}`).test(target);
}

// A provider's selector matches a document only if language/scheme AND any
// `pattern` constraint match. Ignoring `pattern` (as before) made pattern-only
// providers (e.g. Python's `**/*requirement*.txt` hover) match every language.
function filterMatches(filter: IDocumentFilterDto | string, languageId: string, fsPath: string): boolean {
	if (typeof filter === 'string') { return filter === languageId || filter === '*'; }
	if (filter.language && filter.language !== languageId && filter.language !== '*') { return false; }
	if (filter.scheme && filter.scheme !== 'file' && filter.scheme !== '*') { return false; }
	if (filter.pattern && !patternMatches(filter.pattern, fsPath)) { return false; }
	return true;
}

function matchingHandles(session: Session, kind: string, languageId: string, fsPath: string): FeatureRegistration[] {
	const out: FeatureRegistration[] = [];
	for (const reg of session.features.values()) {
		if (reg.kind === kind && reg.selector.some((f) => filterMatches(f, languageId, fsPath))) { out.push(reg); }
	}
	return out;
}

export class LspConnection {
	private buffer = Buffer.alloc(0);
	private readonly langFeatures = this.session.proxy(ExtHostContext.ExtHostLanguageFeatures);

	constructor(private readonly session: Session, private readonly workspace: Workspace, private readonly socket: Socket) {
		socket.on('data', (chunk) => this.onData(chunk));
		socket.on('error', (err) => this.session.log(`lsp socket error: ${String(err)}`));
	}

	// ---- framing ------------------------------------------------------------

	private onData(chunk: Buffer): void {
		this.buffer = Buffer.concat([this.buffer, chunk]);
		for (;;) {
			const headerEnd = this.buffer.indexOf('\r\n\r\n');
			if (headerEnd === -1) { return; }
			const header = this.buffer.subarray(0, headerEnd).toString('utf8');
			const match = /Content-Length:\s*(\d+)/i.exec(header);
			if (!match) { this.buffer = this.buffer.subarray(headerEnd + 4); continue; }
			const length = Number(match[1]);
			const bodyStart = headerEnd + 4;
			if (this.buffer.length < bodyStart + length) { return; }
			const body = this.buffer.subarray(bodyStart, bodyStart + length).toString('utf8');
			this.buffer = this.buffer.subarray(bodyStart + length);
			try { this.dispatch(JSON.parse(body)); } catch (err) { this.session.log(`lsp parse error: ${String(err)}`); }
		}
	}

	private send(message: JsonRpcMessage): void {
		const json = JSON.stringify({ jsonrpc: '2.0', ...message });
		const payload = Buffer.from(json, 'utf8');
		this.socket.write(`Content-Length: ${payload.length}\r\n\r\n`);
		this.socket.write(payload);
	}

	private reply(id: number | string, result: unknown): void { this.send({ id, result }); }
	private replyError(id: number | string, message: string): void { this.send({ id, error: { code: -32603, message } }); }

	// ---- outbound (server -> client) ----------------------------------------

	private pendingRequests = new Map<number, (value: unknown) => void>();
	private nextRequestId = 1;
	private notify(method: string, params: unknown): void { this.send({ method, params }); }

	private request(method: string, params: unknown): Promise<unknown> {
		const id = this.nextRequestId++;
		const { promise, resolve } = Promise.withResolvers<unknown>();
		this.pendingRequests.set(id, resolve);
		this.send({ id: -id, method, params });
		return promise;
	}

	// ---- dispatch -----------------------------------------------------------

	private dispatch(msg: JsonRpcMessage): void {
		// Response to a server->client request (we encode those ids as negative).
		if (msg.id !== undefined && typeof msg.id === 'number' && msg.id < 0 && msg.method === undefined) {
			const resolver = this.pendingRequests.get(-msg.id);
			if (resolver) { this.pendingRequests.delete(-msg.id); resolver(msg.result ?? null); }
			return;
		}
		if (!msg.method) { return; }
		if (msg.id === undefined) { this.onNotification(msg.method, msg.params); return; }
		this.onRequest(msg.id, msg.method, msg.params).then(
			(result) => this.reply(msg.id!, result),
			(err) => this.replyError(msg.id!, String(err)),
		);
	}

	private onNotification(method: string, params: unknown): void {
		switch (method) {
			case 'initialized': return;
			case 'textDocument/didOpen': return this.didOpen(params);
			case 'textDocument/didChange': return this.didChange(params);
			case 'textDocument/didClose': return this.didClose(params);
			case 'exit': this.socket.end(); return;
			case 'ide/webview/onMessage': {
				const p = params as { handle: string; message: unknown };
				this.session.proxy(ExtHostContext.ExtHostWebviews).$onMessage(p.handle, JSON.stringify(p.message), { value: [] } as never);
				return;
			}
			default:
				if (method.startsWith('$/')) { return; }
				this.session.log(`unhandled LSP notification: ${method}`);
		}
	}

	private async onRequest(id: number | string, method: string, params: unknown): Promise<unknown> {
		switch (method) {
			case 'initialize': {
				this.workspace.attachClient((m, p) => this.notify(m, p), (m, p) => this.request(m, p));
				return { capabilities: this.capabilities(), serverInfo: { name: 'ide-exthost', version: '0.1.0' } };
			}
			case 'shutdown': return null;
			case 'textDocument/hover': return this.hover(params);
			case 'textDocument/definition': return this.definition('definition', params);
			case 'textDocument/declaration': return this.definition('declaration', params);
			case 'textDocument/typeDefinition': return this.definition('typeDefinition', params);
			case 'textDocument/implementation': return this.definition('implementation', params);
			case 'textDocument/references': return this.references(params);
			case 'textDocument/documentSymbol': return this.documentSymbol(params);
			case 'textDocument/completion': return this.completion(params);
			case 'textDocument/formatting': return this.formatting(params);
			case 'textDocument/rangeFormatting': return this.rangeFormatting(params);
			case 'textDocument/rename': return this.rename(params);
			case 'textDocument/codeAction': return this.codeAction(params);
			case 'workspace/executeCommand': return this.executeCommand(params);
			case 'ide/commands/list': return { commands: [...this.session.commands].map((c) => ({ id: c, title: c, category: '' })) };
			case 'ide/host/activated': return { activated: [...this.session.activatedExtensions] };
			case 'ide/webview/resolveView': return this.resolveView(params);
			default:
				this.session.log(`unhandled LSP request: ${method}`);
				return null;
		}
	}

	private capabilities(): unknown {
		return {
			textDocumentSync: { openClose: true, change: 1 },
			hoverProvider: true,
			definitionProvider: true,
			declarationProvider: true,
			typeDefinitionProvider: true,
			implementationProvider: true,
			referencesProvider: true,
			documentSymbolProvider: true,
			renameProvider: true,
			documentFormattingProvider: true,
			documentRangeFormattingProvider: true,
			documentHighlightProvider: true,
			codeActionProvider: true,
			completionProvider: { triggerCharacters: COMPLETION_TRIGGER_CHARS, resolveProvider: false },
			executeCommandProvider: { commands: [] },
		};
	}

	// ---- document sync ------------------------------------------------------

	private didOpen(params: unknown): void {
		const p = params as { textDocument: { uri: string; languageId: string; version: number; text: string } };
		const td = p.textDocument;
		const lines = td.text.split(/\r\n|\r|\n/);
		const uriComp = URI.parse(td.uri);
		this.session.documents.set(td.uri, { uri: td.uri, languageId: td.languageId, version: td.version, lines, eol: '\n' });
		const editorId = `editor:${td.uri}`;
		this.session.editorToUri.set(editorId, td.uri);
		this.session.proxy(ExtHostContext.ExtHostDocumentsAndEditors).$acceptDocumentsAndEditorsDelta({
			addedDocuments: [{ uri: uriComp, versionId: td.version, lines, EOL: '\n', languageId: td.languageId, isDirty: false, encoding: 'utf8' }],
			addedEditors: [{
				id: editorId,
				documentUri: uriComp,
				options: { tabSize: 4, indentSize: 4, originalIndentSize: 4, insertSpaces: true, cursorStyle: 1, lineNumbers: 1 },
				selections: [{ selectionStartLineNumber: 1, selectionStartColumn: 1, positionLineNumber: 1, positionColumn: 1 }],
				visibleRanges: [{ startLineNumber: 1, startColumn: 1, endLineNumber: lines.length, endColumn: 1 }],
				editorPosition: 0,
			}],
			newActiveEditor: editorId,
		});
		this.session.proxy(ExtHostContext.ExtHostExtensionService).$activateByEvent(`onLanguage:${td.languageId}`, 0).catch(() => { /* ignore */ });
	}

	private didChange(params: unknown): void {
		const p = params as { textDocument: { uri: string; version: number }; contentChanges: { text: string }[] };
		const doc = this.session.documents.get(p.textDocument.uri);
		if (!doc || p.contentChanges.length === 0) { return; }
		const oldLines = doc.lines;
		const oldText = oldLines.join(doc.eol);
		const newText = p.contentChanges[p.contentChanges.length - 1].text;
		doc.lines = newText.split(/\r\n|\r|\n/);
		doc.version = p.textDocument.version;
		const event = {
			changes: [{
				range: { startLineNumber: 1, startColumn: 1, endLineNumber: oldLines.length, endColumn: oldLines[oldLines.length - 1].length + 1 },
				rangeOffset: 0, rangeLength: oldText.length, text: newText,
			}],
			eol: doc.eol, versionId: doc.version, isUndoing: false, isRedoing: false, isFlush: false, isEolChange: false,
		};
		this.session.proxy(ExtHostContext.ExtHostDocuments).$acceptModelChanged(URI.parse(p.textDocument.uri), event, false);
	}

	private didClose(params: unknown): void {
		const p = params as { textDocument: { uri: string } };
		const uri = p.textDocument.uri;
		if (!this.session.documents.delete(uri)) { return; }
		const editorId = `editor:${uri}`;
		this.session.editorToUri.delete(editorId);
		this.session.proxy(ExtHostContext.ExtHostDocumentsAndEditors).$acceptDocumentsAndEditorsDelta({
			removedDocuments: [URI.parse(uri)], removedEditors: [editorId], newActiveEditor: null,
		});
	}

	// ---- language feature requests ------------------------------------------

	private docParams(params: unknown): { uriComp: UriComponents; fsPath: string; languageId: string; position: unknown } | undefined {
		const p = params as { textDocument: { uri: string }; position?: { line: number; character: number } };
		const doc = this.session.documents.get(p.textDocument.uri);
		if (!doc) { return undefined; }
		const uriComp = URI.parse(p.textDocument.uri);
		return {
			uriComp,
			fsPath: uriComp.fsPath,
			languageId: doc.languageId,
			position: p.position ? toInternalPosition(p.position) : undefined,
		};
	}

	private async hover(params: unknown): Promise<unknown> {
		const ctx = this.docParams(params);
		if (!ctx) { return null; }
		for (const reg of matchingHandles(this.session, 'hover', ctx.languageId, ctx.fsPath)) {
			const result = await this.langFeatures.$provideHover(reg.handle, ctx.uriComp, ctx.position as never, undefined, CancellationToken.None);
			if (result) { return hoverToLsp(result); }
		}
		return null;
	}

	private async definition(kind: string, params: unknown): Promise<unknown> {
		const ctx = this.docParams(params);
		if (!ctx) { return null; }
		const method = kind === 'definition' ? '$provideDefinition'
			: kind === 'declaration' ? '$provideDeclaration'
				: kind === 'typeDefinition' ? '$provideTypeDefinition' : '$provideImplementation';
		const handles = matchingHandles(this.session, kind, ctx.languageId, ctx.fsPath);
		if (process.env.IDE_EXTHOST_DEBUG) { this.session.log(`definition(${kind}) lang=${ctx.languageId} handles=${handles.length}`); }
		const out: unknown[] = [];
		for (const reg of handles) {
			const links = await this.langFeatures[method](reg.handle, ctx.uriComp, ctx.position as never, CancellationToken.None);
			if (process.env.IDE_EXTHOST_DEBUG) { this.session.log(`definition handle=${reg.handle} -> ${JSON.stringify(links)}`); }
			out.push(...locationLinksToLsp(links));
		}
		return out;
	}

	private async references(params: unknown): Promise<unknown> {
		const ctx = this.docParams(params);
		if (!ctx) { return null; }
		const context = (params as { context?: { includeDeclaration?: boolean } }).context ?? { includeDeclaration: true };
		const out: unknown[] = [];
		for (const reg of matchingHandles(this.session, 'references', ctx.languageId, ctx.fsPath)) {
			const locs = await this.langFeatures.$provideReferences(reg.handle, ctx.uriComp, ctx.position as never, context as never, CancellationToken.None);
			out.push(...locationsToLsp(locs));
		}
		return out;
	}

	private async documentSymbol(params: unknown): Promise<unknown> {
		const ctx = this.docParams(params);
		if (!ctx) { return null; }
		for (const reg of matchingHandles(this.session, 'documentSymbol', ctx.languageId, ctx.fsPath)) {
			const symbols = await this.langFeatures.$provideDocumentSymbols(reg.handle, ctx.uriComp, CancellationToken.None);
			if (symbols && symbols.length > 0) { return symbols.map(documentSymbolToLsp); }
		}
		return [];
	}

	private async completion(params: unknown): Promise<unknown> {
		const ctx = this.docParams(params);
		if (!ctx) { return { isIncomplete: false, items: [] }; }
		const context = (params as { context?: { triggerKind?: number; triggerCharacter?: string } }).context ?? { triggerKind: 0 };
		const items: unknown[] = [];
		for (const reg of matchingHandles(this.session, 'completion', ctx.languageId, ctx.fsPath)) {
			const result = await this.langFeatures.$provideCompletionItems(reg.handle, ctx.uriComp, ctx.position as never, context as never, CancellationToken.None);
			if (!result) { continue; }
			const defaults = result[ISuggestResultDtoField.defaultRanges];
			for (const item of result[ISuggestResultDtoField.completions]) {
				items.push(suggestItemToLsp(item, defaults));
			}
		}
		return { isIncomplete: false, items };
	}

	private async formatting(params: unknown): Promise<unknown> {
		const ctx = this.docParams(params);
		if (!ctx) { return null; }
		const options = (params as { options?: { tabSize?: number; insertSpaces?: boolean } }).options ?? {};
		const fmt = { tabSize: options.tabSize ?? 4, insertSpaces: options.insertSpaces ?? true };
		for (const reg of matchingHandles(this.session, 'formatting', ctx.languageId, ctx.fsPath)) {
			const edits = await this.langFeatures.$provideDocumentFormattingEdits(reg.handle, ctx.uriComp, fmt as never, CancellationToken.None);
			if (edits) { return textEditsToLsp(edits); }
		}
		return null;
	}

	private async rangeFormatting(params: unknown): Promise<unknown> {
		const ctx = this.docParams(params);
		if (!ctx) { return null; }
		const p = params as { range: { start: { line: number; character: number }; end: { line: number; character: number } }; options?: { tabSize?: number; insertSpaces?: boolean } };
		const range = toInternalRange(p.range);
		const fmt = { tabSize: p.options?.tabSize ?? 4, insertSpaces: p.options?.insertSpaces ?? true };
		for (const reg of matchingHandles(this.session, 'rangeFormatting', ctx.languageId, ctx.fsPath)) {
			const edits = await this.langFeatures.$provideDocumentRangeFormattingEdits(reg.handle, ctx.uriComp, range as never, fmt as never, CancellationToken.None);
			if (edits) { return textEditsToLsp(edits); }
		}
		return null;
	}

	private async rename(params: unknown): Promise<unknown> {
		const ctx = this.docParams(params);
		if (!ctx) { return null; }
		const newName = (params as { newName: string }).newName;
		for (const reg of matchingHandles(this.session, 'rename', ctx.languageId, ctx.fsPath)) {
			const edit = await this.langFeatures.$provideRenameEdits(reg.handle, ctx.uriComp, ctx.position as never, newName, CancellationToken.None);
			if (edit) { return workspaceEditToLsp(edit); }
		}
		return null;
	}

	private async codeAction(params: unknown): Promise<unknown> {
		const ctx = this.docParams(params);
		if (!ctx) { return []; }
		const p = params as { range: { start: { line: number; character: number }; end: { line: number; character: number } }; context?: { diagnostics?: unknown[]; only?: string[] } };
		const range = toInternalRange(p.range);
		const context = { trigger: 1, only: p.context?.only?.[0], diagnostics: [] };
		const actions: unknown[] = [];
		for (const reg of matchingHandles(this.session, 'codeAction', ctx.languageId, ctx.fsPath)) {
			const list = await this.langFeatures.$provideCodeActions(reg.handle, ctx.uriComp, range as never, context as never, CancellationToken.None);
			if (!list) { continue; }
			for (const action of list.actions) {
				actions.push({
					title: action.title,
					kind: action.kind,
					isPreferred: action.isPreferred,
					edit: action.edit ? workspaceEditToLsp(action.edit) : undefined,
					command: action.command ? { command: action.command.id, title: action.command.title, arguments: action.command.arguments } : undefined,
				});
			}
		}
		return actions;
	}

	private async executeCommand(params: unknown): Promise<unknown> {
		const p = params as { command: string; arguments?: unknown[] };
		return this.session.proxy(ExtHostContext.ExtHostCommands).$executeContributedCommand(p.command, ...(p.arguments ?? []));
	}

	// ---- custom ide/* -------------------------------------------------------

	private async resolveView(params: unknown): Promise<unknown> {
		const viewId = (params as { viewId: string }).viewId;
		const handle = `view:${viewId}:${Date.now()}`;
		await this.session.proxy(ExtHostContext.ExtHostWebviewViews).$resolveWebviewView(handle, viewId, viewId, undefined, CancellationToken.None);
		return { handle };
	}
}
