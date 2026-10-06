// The main-thread shim: implementations of the MainThread* actor shapes that the
// real VS Code extension host talks to over RPC. Behavioural actors are wired to
// the LSP bridge / filesystem; everything else gets a loud fallback so that
// unimplemented calls are logged (once) and resolve to `undefined` rather than
// hanging or throwing inside extension activation.
import * as fs from 'node:fs';
import * as fsp from 'node:fs/promises';
import * as path from 'node:path';
import { MainContext, ExtHostContext, URI, VSBuffer, SerializableObjectWithBuffers } from './vs.js';
import type { Session } from './session.js';
import type { ScannedExtension } from './scanner.js';
import { markerToLspDiagnostic, workspaceEditToLsp } from './convert.js';
import { globToRegExp } from './glob.js';
import type { UriComponents } from './vs.js';
import type { IMarkerData, IDocumentFilterDto, IWorkspaceEditDto } from '../.vscode-src/src/vs/workbench/api/common/extHost.protocol.js';

export interface ShimDeps {
	extensions: ScannedExtension[];
	dataDir: string;
	home: string;
	reconfigure(key: string, value: unknown): void;
}

type Actor = Record<string, (...args: never[]) => unknown>;

// ---- diagnostics aggregation (per-owner, merged per-uri) --------------------

class DiagnosticsSink {
	private readonly byUri = new Map<string, Map<string, unknown[]>>();
	constructor(private readonly session: Session) { }

	changeMany(owner: string, entries: [UriComponents, IMarkerData[] | undefined][]): void {
		const touched = new Set<string>();
		for (const [uriComp, markers] of entries) {
			const uri = URI.revive(uriComp).toString();
			touched.add(uri);
			let owners = this.byUri.get(uri);
			if (!owners) { owners = new Map(); this.byUri.set(uri, owners); }
			if (markers && markers.length > 0) {
				owners.set(owner, markers.map(markerToLspDiagnostic));
			} else {
				owners.delete(owner);
			}
		}
		for (const uri of touched) { this.publish(uri); }
	}

	clear(owner: string): void {
		for (const [uri, owners] of this.byUri) {
			if (owners.delete(owner)) { this.publish(uri); }
		}
	}

	private publish(uri: string): void {
		const owners = this.byUri.get(uri);
		const diagnostics: unknown[] = [];
		if (owners) { for (const list of owners.values()) { diagnostics.push(...list); } }
		this.session.notify('textDocument/publishDiagnostics', { uri, diagnostics });
	}
}

// ---- filesystem helpers -----------------------------------------------------

function fileType(stat: { isDirectory(): boolean; isSymbolicLink(): boolean }): number {
	if (stat.isDirectory()) { return 2; }
	if (stat.isSymbolicLink()) { return 64; }
	return 1;
}

// ---- decoration render options ----------------------------------------------

interface DecorationRenderAfter { contentText?: string; color?: unknown; }
interface DecorationTypeOptions { after?: DecorationRenderAfter; before?: DecorationRenderAfter; }
interface DecorationOption {
	range: { startLineNumber: number; startColumn: number; endLineNumber: number; endColumn: number };
	hoverMessage?: unknown;
	renderOptions?: { after?: DecorationRenderAfter; before?: DecorationRenderAfter };
}

export interface MainShim {
	diagnostics: DiagnosticsSink;
}

export function installMainShim(session: Session, deps: ShimDeps): MainShim {
	const log = session.log;
	const diagnostics = new DiagnosticsSink(session);
	const extById = new Map<string, ScannedExtension>();
	for (const e of deps.extensions) { extById.set(e.description.identifier.value.toLowerCase(), e); }

	const activateByEvent = (event: string): void => {
		session.proxy(ExtHostContext.ExtHostExtensionService).$activateByEvent(event, 0).catch((err: unknown) => log(`activateByEvent ${event} failed: ${String(err)}`));
	};

	const toLspRangeFromInternal = (r: DecorationOption['range']): unknown => ({
		start: { line: r.startLineNumber - 1, character: r.startColumn - 1 },
		end: { line: r.endLineNumber - 1, character: r.endColumn - 1 },
	});

	// -- storage / secrets persistence --
	const storageFile = path.join(deps.dataDir, 'User', 'globalStorage', 'ide-storage.json');
	const secretsFile = path.join(deps.dataDir, 'User', 'secrets.json');
	const readJson = (file: string): Record<string, Record<string, string>> => {
		try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch { return {}; }
	};
	const writeJson = (file: string, data: unknown): void => {
		fs.mkdirSync(path.dirname(file), { recursive: true });
		fs.writeFileSync(file, JSON.stringify(data));
	};

	// -- webview html staging --
	const webviewHtml = new Map<string, string>();
	const webviewCreated = new Set<string>();

	const impls: Partial<Record<string, Actor>> = {
		MainThreadExtensionService: {
			$onDidActivateExtension(id: { value: string }) { session.activatedExtensions.add(id.value); log(`activated ${id.value}`); },
			$onWillActivateExtension() { /* noop */ },
			$onExtensionActivationError(id: { value: string }, error: unknown, missing: unknown) {
				log(`ACTIVATION ERROR ${id.value}: ${JSON.stringify(error)}${missing ? ` missingDep=${JSON.stringify(missing)}` : ''}`);
			},
			$onExtensionRuntimeError(id: { value: string }, error: unknown) { log(`runtime error ${id.value}: ${JSON.stringify(error)}`); },
			$setPerformanceMarks() { /* noop */ },
			async $getExtension(extensionId: string) { return extById.get(extensionId.toLowerCase())?.description; },
			async $asBrowserUri(uri: UriComponents) { return uri; },
			async $activateExtension(id: { value: string }, reason: unknown) {
				await session.proxy(ExtHostContext.ExtHostExtensionService).$activate(id as never, reason as never);
			},
		},
		MainThreadConsole: {
			$logExtensionHostMessage(entry: { type?: string; severity?: string; arguments?: string }) {
				log(`[exthost console] ${entry.severity ?? ''} ${entry.arguments ?? ''}`);
			},
		},
		MainThreadErrors: {
			$onUnexpectedError(err: unknown) { log(`[exthost error] ${JSON.stringify(err)}`); },
		},
		MainThreadTelemetry: {
			$publicLog() { /* telemetry dropped */ },
			$publicLog2() { /* telemetry dropped */ },
		},
		MainThreadLogger: {
			$log() { /* noop */ },
			$flush() { /* noop */ },
			async $createLogger() { /* noop */ },
			async $registerLogger() { /* noop */ },
			async $deregisterLogger() { /* noop */ },
			async $setVisibility() { /* noop */ },
		},
		MainThreadCommands: {
			$registerCommand(id: string) {
				session.commands.add(id);
				session.notify('ide/commands/changed', { commands: [...session.commands].map((c) => ({ id: c, title: c, category: '' })) });
			},
			$unregisterCommand(id: string) {
				session.commands.delete(id);
				session.notify('ide/commands/changed', { commands: [...session.commands].map((c) => ({ id: c, title: c, category: '' })) });
			},
			$fireCommandActivationEvent(id: string) { activateByEvent(`onCommand:${id}`); },
			async $executeCommand(id: string, args: unknown[] | SerializableObjectWithBuffers<unknown[]>) {
				const realArgs = args instanceof SerializableObjectWithBuffers ? args.value : args;
				if (session.commands.has(id)) {
					return session.proxy(ExtHostContext.ExtHostCommands).$executeContributedCommand(id, ...realArgs);
				}
				// Built-in commands the workbench would normally provide.
				if (id === 'setContext' || id === '_setContext') { session.contextKeys.set(String(realArgs[0]), realArgs[1]); return undefined; }
				if (id === 'vscode.executeDocumentSymbolProvider') { return []; }
				log(`unimplemented builtin command: ${id}`);
				return undefined;
			},
			async $getCommands() { return [...session.commands]; },
		},
		MainThreadConfiguration: {
			async $updateConfigurationOption(_target: unknown, key: string, value: unknown) { deps.reconfigure(key, value); },
			async $removeConfigurationOption(_target: unknown, key: string) { deps.reconfigure(key, undefined); },
		},
		MainThreadWorkspace: {
			async $startFileSearch(includeFolder: UriComponents | null, options: { includePattern?: { pattern?: string }; filePattern?: string; maxResults?: number }) {
				const roots = includeFolder ? [URI.revive(includeFolder).fsPath] : session.folders;
				const pattern = options.includePattern?.pattern ?? options.filePattern;
				const re = pattern ? globToRegExp(pattern) : undefined;
				const max = options.maxResults ?? 10000;
				const out: UriComponents[] = [];
				const walk = (dir: string, rootDir: string): void => {
					if (out.length >= max) { return; }
					let entries: fs.Dirent[];
					try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
					for (const ent of entries) {
						if (ent.name === 'node_modules' || ent.name === '.git') { continue; }
						const full = path.join(dir, ent.name);
						if (ent.isDirectory()) { walk(full, rootDir); }
						else {
							const rel = path.relative(rootDir, full);
							if (!re || re.test(rel)) { out.push(URI.file(full)); if (out.length >= max) { return; } }
						}
					}
				};
				for (const root of roots) { walk(root, root); }
				return out;
			},
			async $checkExists(folders: UriComponents[], includes: string[], _token: unknown) {
				const regexes = includes.map(globToRegExp);
				const walk = (dir: string, rootDir: string): boolean => {
					let entries: fs.Dirent[];
					try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return false; }
					for (const ent of entries) {
						if (ent.name === 'node_modules' || ent.name === '.git') { continue; }
						const full = path.join(dir, ent.name);
						if (ent.isDirectory()) { if (walk(full, rootDir)) { return true; } }
						else if (regexes.some((re) => re.test(path.relative(rootDir, full)))) { return true; }
					}
					return false;
				};
				for (const folder of folders) {
					const root = URI.revive(folder).fsPath;
					if (walk(root, root)) { return true; }
				}
				return false;
			},
			async $resolveProxy() { return undefined; },
			async $lookupAuthorization() { return undefined; },
			async $lookupKerberosAuthorization() { return undefined; },
			async $loadCertificates() { return []; },
			async $requestWorkspaceTrust() { return true; },
			async $requestResourceTrust() { return true; },
			async $isResourceTrusted() { return true; },
			async $saveAll() { return true; },
			async $resolveDecoding() { return { preferredEncoding: 'utf8', guessEncoding: false, candidateGuessEncodings: [] }; },
			async $resolveEncoding() { return { encoding: 'utf8', addBOM: false }; },
			async $validateDetectedEncoding() { return 'utf8'; },
		},
		MainThreadDocuments: {
			async $tryOpenDocument(uriComp: UriComponents) {
				const uri = URI.revive(uriComp);
				if (!session.documents.has(uri.toString()) && uri.scheme === 'file') {
					const text = await fsp.readFile(uri.fsPath, 'utf8');
					const lines = text.split(/\r\n|\r|\n/);
					session.documents.set(uri.toString(), { uri: uri.toString(), languageId: 'plaintext', version: 1, lines, eol: '\n' });
					session.proxy(ExtHostContext.ExtHostDocumentsAndEditors).$acceptDocumentsAndEditorsDelta({
						addedDocuments: [{ uri: uriComp, versionId: 1, lines, EOL: '\n', languageId: 'plaintext', isDirty: false, encoding: 'utf8' }],
					});
				}
				return uriComp;
			},
			async $tryCreateDocument() { throw new Error('untitled documents are not supported by the shared host'); },
			async $trySaveDocument(uriComp: UriComponents) {
				const uri = URI.revive(uriComp);
				const doc = session.documents.get(uri.toString());
				if (doc && uri.scheme === 'file') { await fsp.writeFile(uri.fsPath, doc.lines.join(doc.eol)); return true; }
				return false;
			},
		},
		MainThreadDiagnostics: {
			$changeMany(owner: string, entries: [UriComponents, IMarkerData[] | undefined][]) { diagnostics.changeMany(owner, entries); },
			$clear(owner: string) { diagnostics.clear(owner); },
		},
		MainThreadLanguageFeatures: makeLanguageFeatures(session),
		MainThreadLanguages: {
			$setLanguageStatus(handle: number, status: { label?: string; detail?: string; command?: { title?: string } }) {
				session.notify('ide/statusBar/set', { id: `lang.${handle}`, text: status.label ?? '', tooltip: status.detail ?? '', alignment: 'right', priority: 0 });
			},
			$removeLanguageStatus(handle: number) { session.notify('ide/statusBar/remove', { id: `lang.${handle}` }); },
			async $changeLanguage(uriComp: UriComponents, languageId: string) {
				const doc = session.documents.get(URI.revive(uriComp).toString());
				if (doc) { doc.languageId = languageId; }
			},
		},
		MainThreadMessageService: {
			async $showMessage(severity: number, message: string, _options: unknown, commands: { title: string; handle: number }[]) {
				const type = severity >= 3 ? 1 : severity === 2 ? 2 : 3;
				if (commands.length > 0) {
					const picked = await session.request('window/showMessageRequest', { type, message, actions: commands.map((c) => ({ title: c.title })) });
					if (picked && typeof picked === 'object' && 'title' in picked) {
						const title = picked.title;
						const match = commands.find((c) => c.title === title);
						return match?.handle;
					}
					return undefined;
				}
				session.notify('window/showMessage', { type, message });
				return undefined;
			},
		},
		MainThreadStatusBar: {
			$setEntry(id: string, _statusId: string, _extId: string | undefined, _name: string, text: string, tooltip: unknown, _hasTip: boolean, command: { id?: string } | undefined, _color: unknown, _bg: unknown, alignLeft: boolean, priority: number | undefined) {
				session.notify('ide/statusBar/set', {
					id, text,
					tooltip: toPlainText(tooltip),
					command: command?.id,
					alignment: alignLeft ? 'left' : 'right',
					priority: priority ?? 0,
				});
			},
			$disposeEntry(id: string) { session.notify('ide/statusBar/remove', { id }); },
		},
		MainThreadOutputService: {
			async $register(label: string) { return `output-${label}`; },
			async $update() { /* output tailing not bridged */ },
			async $reveal() { /* noop */ },
			async $close() { /* noop */ },
			async $dispose() { /* noop */ },
		},
		MainThreadProgress: {
			async $startProgress(handle: number, options: { title?: string }) { log(`progress start [${handle}] ${options.title ?? ''}`); },
			$progressReport() { /* noop */ },
			$progressEnd() { /* noop */ },
		},
		MainThreadStorage: {
			async $initializeExtensionStorage(shared: boolean, extensionId: string) {
                const store = readJson(storageFile);
                return JSON.stringify(store[`${shared ? 'g' : 'w'}:${extensionId}`] ?? {});
			},
			async $setValue(shared: boolean, extensionId: string, value: object) {
				const store = readJson(storageFile);
				store[`${shared ? 'g' : 'w'}:${extensionId}`] = value as Record<string, string>;
				writeJson(storageFile, store);
			},
			$registerExtensionStorageKeysToSync() { /* noop */ },
		},
		MainThreadSecretState: {
			async $getPassword(extensionId: string, key: string) { return readJson(secretsFile)[extensionId]?.[key]; },
			async $setPassword(extensionId: string, key: string, value: string) {
				const store = readJson(secretsFile);
				(store[extensionId] ??= {})[key] = value;
				writeJson(secretsFile, store);
				session.proxy(ExtHostContext.ExtHostSecretState).$onDidChangePassword({ extensionId, key }).catch(() => { /* ignore */ });
			},
			async $deletePassword(extensionId: string, key: string) {
				const store = readJson(secretsFile);
				if (store[extensionId]) { delete store[extensionId][key]; writeJson(secretsFile, store); }
			},
			async $getKeys(extensionId: string) { return Object.keys(readJson(secretsFile)[extensionId] ?? {}); },
		},
		MainThreadAuthentication: {
			async $registerAuthenticationProvider() { /* noop */ },
			async $unregisterAuthenticationProvider() { /* noop */ },
			async $ensureProvider() { /* noop */ },
			async $sendDidChangeSessions() { /* noop */ },
			async $getSession() { return undefined; },
			async $getAccounts() { return []; },
			async $removeSession() { /* noop */ },
		},
		MainThreadWindow: {
			async $getInitialState() { return { isFocused: true, isActive: true }; },
			async $openUri() { return true; },
			async $asExternalUri(uri: UriComponents) { return uri; },
		},
		MainThreadLanguageModelTools: {
			async $getTools() { return []; },
			async $registerTool() { /* noop */ },
			$unregisterTool() { /* noop */ },
			async $invokeTool() { throw new Error('language model tools are not available in the shared host'); },
			async $countTokensForInvocation() { return 0; },
		},
		MainThreadLanguageModels: {
			async $registerLanguageModelProvider() { /* noop */ },
			$unregisterProvider() { /* noop */ },
			async $tryStartChatRequest() { throw new Error('language models are not available in the shared host'); },
			async $selectChatModels() { return []; },
			$handleProgressChunk() { /* noop */ },
			async $countTokens() { return 0; },
		},
		MainThreadFileSystem: {
			async $stat(uriComp: UriComponents) {
				const s = await fsp.stat(URI.revive(uriComp).fsPath);
				return { type: fileType(s), ctime: s.ctimeMs, mtime: s.mtimeMs, size: s.size, permissions: undefined };
			},
			async $readdir(uriComp: UriComponents) {
				const dir = URI.revive(uriComp).fsPath;
				const entries = await fsp.readdir(dir, { withFileTypes: true });
				return entries.map((e) => [e.name, fileType(e)] as [string, number]);
			},
			async $readFile(uriComp: UriComponents) {
				return VSBuffer.wrap(await fsp.readFile(URI.revive(uriComp).fsPath));
			},
			async $registerFileSystemProvider() { /* providers from extensions are recorded ext-side */ },
			$unregisterProvider() { /* noop */ },
			async $ensureActivation(scheme: string) { activateByEvent(`onFileSystem:${scheme}`); },
		},
		MainThreadFileSystemEventService: {
			$watch() { /* file watching not bridged in the shared host */ },
			$unwatch() { /* noop */ },
		},
		MainThreadBulkEdits: {
			async $tryApplyWorkspaceEdit(dto: SerializableObjectWithBuffers<IWorkspaceEditDto> | IWorkspaceEditDto) {
				const edit = dto instanceof SerializableObjectWithBuffers ? dto.value : dto;
				const applied = await session.request('workspace/applyEdit', { edit: workspaceEditToLsp(edit) });
				if (applied && typeof applied === 'object' && 'applied' in applied) { return Boolean(applied.applied); }
				return true;
			},
		},
		MainThreadWebviews: {
			$setHtml(handle: string, value: string) {
				const firstSet = !webviewHtml.has(handle);
				webviewHtml.set(handle, value);
				if (webviewCreated.has(handle)) {
					// Panel already announced via $createWebviewPanel.
					session.notify('ide/webview/setHtml', { handle, html: value });
				} else if (firstSet) {
					// Webview *view* (e.g. Claude sidebar): announce it on first html.
					webviewCreated.add(handle);
					session.notify('ide/webview/create', { handle, viewType: handle, title: '', html: value, options: {}, kind: 'view' });
				} else {
					session.notify('ide/webview/setHtml', { handle, html: value });
				}
			},
			$setOptions() { /* options tracked client-side */ },
			async $postMessage(handle: string, value: string) {
				session.notify('ide/webview/postMessage', { handle, message: safeParse(value) });
				return true;
			},
		},
		MainThreadWebviewPanels: {
			$createWebviewPanel(_ext: unknown, handle: string, viewType: string, initData: { title?: string }, _show: unknown) {
				webviewCreated.add(handle);
				session.notify('ide/webview/create', { handle, viewType, title: initData.title ?? viewType, html: webviewHtml.get(handle) ?? '', options: {}, kind: 'panel' });
			},
			$disposeWebview(handle: string) { session.notify('ide/webview/dispose', { handle }); webviewCreated.delete(handle); },
			$reveal() { /* noop */ },
			$setTitle(handle: string, value: string) { session.notify('ide/webview/setTitle', { handle, title: value }); },
			$setIconPath() { /* noop */ },
			$registerSerializer() { /* noop */ },
			$unregisterSerializer() { /* noop */ },
		},
		MainThreadWebviewViews: {
			$registerWebviewViewProvider(_ext: unknown, viewType: string) {
				session.notify('ide/views/register', { id: viewType, name: viewType, container: '', kind: 'webview' });
			},
			$unregisterWebviewViewProvider() { /* noop */ },
			$setWebviewViewTitle() { /* noop */ },
			$setWebviewViewDescription() { /* noop */ },
			$setWebviewViewBadge() { /* noop */ },
			$show() { /* noop */ },
		},
		MainThreadTreeViews: {
			async $registerTreeViewDataProvider(treeViewId: string) {
				session.notify('ide/views/register', { id: treeViewId, name: treeViewId, container: '', kind: 'tree' });
			},
			async $refresh() { /* noop */ },
			async $reveal() { /* noop */ },
			$setMessage() { /* noop */ },
			$setTitle() { /* noop */ },
			$setBadge() { /* noop */ },
			async $disposeTree() { /* noop */ },
		},
		MainThreadTextEditors: {
			async $tryShowTextDocument() { return undefined; },
			$registerTextEditorDecorationType(_extId: unknown, key: string, options: DecorationTypeOptions) { decorationTypes.set(key, options); },
			$removeTextEditorDecorationType(key: string) { decorationTypes.delete(key); },
			async $tryShowEditor() { /* noop */ },
			async $tryHideEditor() { /* noop */ },
			async $trySetOptions() { /* noop */ },
			async $trySetDecorations(id: string, key: string, ranges: DecorationOption[]) {
				const doc = editorToUri.get(id);
				if (!doc) { return; }
				const typeOpts = decorationTypes.get(key);
				const decorations = ranges.map((r) => ({
					range: toLspRangeFromInternal(r.range),
					after: (r.renderOptions?.after?.contentText ?? typeOpts?.after?.contentText) !== undefined
						? { contentText: r.renderOptions?.after?.contentText ?? typeOpts?.after?.contentText }
						: undefined,
					hoverMessage: hoverText(r.hoverMessage),
				}));
				session.notify('ide/decorations/set', { uri: doc, decorations });
			},
			async $trySetDecorationsFast() { /* no content payload */ },
			async $tryRevealRange() { /* noop */ },
			async $trySetSelections() { /* noop */ },
			async $tryApplyEdits() { return false; },
		},
	};

	const decorationTypes = new Map<string, DecorationTypeOptions>();
	const editorToUri = session.editorToUri;

	registerActors(session, impls, log);
	return { diagnostics };
}

function hoverText(msg: unknown): string | undefined {
	if (typeof msg === 'string') { return msg; }
	if (Array.isArray(msg)) { return msg.map(hoverText).filter(Boolean).join('\n'); }
	if (msg && typeof msg === 'object' && 'value' in msg) { return String(msg.value); }
	return undefined;
}

function toPlainText(value: unknown): string | undefined {
	if (typeof value === 'string') { return value; }
	if (value && typeof value === 'object' && 'value' in value) { return String(value.value); }
	return undefined;
}

function safeParse(value: string): unknown {
	try { return JSON.parse(value); } catch { return value; }
}

// Records every language-feature provider registration so the LSP bridge can
// route requests to a matching provider handle.
function makeLanguageFeatures(session: Session): Actor {
	const reg = (handle: number, kind: string, selector: IDocumentFilterDto[], extra?: Record<string, unknown>): void => {
		session.features.set(handle, { handle, kind, selector, data: extra });
		if (process.env.IDE_EXTHOST_DEBUG) { session.log(`register language feature: ${kind} selector=${JSON.stringify(selector)}`); }
	};
	return {
		$unregister(handle: number) { session.features.delete(handle); },
		$registerHoverProvider(h: number, s: IDocumentFilterDto[]) { reg(h, 'hover', s); },
		$registerDefinitionSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'definition', s); },
		$registerDeclarationSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'declaration', s); },
		$registerImplementationSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'implementation', s); },
		$registerTypeDefinitionSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'typeDefinition', s); },
		$registerReferenceSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'references', s); },
		$registerDocumentSymbolProvider(h: number, s: IDocumentFilterDto[]) { reg(h, 'documentSymbol', s); },
		$registerRenameSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'rename', s); },
		$registerDocumentFormattingSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'formatting', s); },
		$registerRangeFormattingSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'rangeFormatting', s); },
		$registerOnTypeFormattingSupport(h: number, s: IDocumentFilterDto[], chars: string[]) { reg(h, 'onTypeFormatting', s, { triggerCharacters: chars }); },
		$registerCompletionsProvider(h: number, s: IDocumentFilterDto[], triggerCharacters: string[]) { reg(h, 'completion', s, { triggerCharacters }); },
		$registerSignatureHelpProvider(h: number, s: IDocumentFilterDto[], metadata: { triggerCharacters?: string[] }) { reg(h, 'signatureHelp', s, { triggerCharacters: metadata?.triggerCharacters }); },
		$registerCodeActionSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'codeAction', s); },
		$registerDocumentHighlightProvider(h: number, s: IDocumentFilterDto[]) { reg(h, 'documentHighlight', s); },
		$registerCodeLensSupport(h: number, s: IDocumentFilterDto[]) { reg(h, 'codeLens', s); },
		$registerDocumentLinkProvider(h: number, s: IDocumentFilterDto[]) { reg(h, 'documentLink', s); },
		$registerFoldingRangeProvider(h: number, s: IDocumentFilterDto[]) { reg(h, 'foldingRange', s); },
		$registerSelectionRangeProvider(h: number, s: IDocumentFilterDto[]) { reg(h, 'selectionRange', s); },
		$registerInlayHintsProvider(h: number, s: IDocumentFilterDto[]) { reg(h, 'inlayHint', s); },
		$registerDocumentSemanticTokensProvider(h: number, s: IDocumentFilterDto[], legend: unknown) { reg(h, 'semanticTokens', s, { legend }); },
		$registerDocumentRangeSemanticTokensProvider(h: number, s: IDocumentFilterDto[], legend: unknown) { reg(h, 'semanticTokensRange', s, { legend }); },
		$setLanguageConfiguration() { /* indentation rules not bridged */ },
		$setWordDefinitions() { /* noop */ },
	};
}

// Registers each actor under its MainContext identifier. Actors we implement are
// wrapped so unknown method calls are logged once; everything else gets a pure
// loud fallback. This guarantees no "unknown actor" throw and no hangs.
function registerActors(session: Session, impls: Partial<Record<string, Actor>>, log: (m: string) => void): void {
	const warned = new Set<string>();
	for (const [name, identifier] of Object.entries(MainContext)) {
		const impl = impls[name];
		const handler: ProxyHandler<Actor> = {
			get(_t, prop: string | symbol) {
				if (typeof prop !== 'string') { return undefined; }
				if (impl && prop in impl) { return impl[prop].bind(impl); }
				return (...args: unknown[]) => {
					const key = `${name}.${prop}`;
					if (!warned.has(key)) { warned.add(key); log(`UNIMPLEMENTED main-thread call: ${key}(${args.length} args)`); }
					return undefined;
				};
			},
		};
		session.rpc.set(identifier, new Proxy({} as Actor, handler));
	}
}
