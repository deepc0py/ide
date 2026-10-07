// End-to-end test of the shared extension host: downloads the 6 required
// extensions from Open VSX, builds sample projects, boots dist/host.js, opens a
// multi-root workspace over the control socket, then drives each extension's core
// feature through the per-workspace LSP socket.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, readdirSync, writeFileSync, rmSync, symlinkSync } from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { ensureExtensions } from './extensions.mjs';
import { makeFixtures } from './fixtures.mjs';
import { connect, sleep } from './client.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const HOST = path.join(here, '..', 'dist', 'host.js');
const DATA_DIR = '/tmp/ide-exthost-test-data';
const HOME_DIR = '/tmp/ide-exthost-test-home';
const CONTROL = path.join(DATA_DIR, 'control.sock');

// Captured before we point the host's HOME at /tmp: rust-analyzer's cargo/rustc are
// rustup shims that resolve the toolchain via $RUSTUP_HOME (default ~/.rustup). The
// production IDE runs with the real HOME where this works; in tests we isolate HOME
// but still hand rust-analyzer a usable toolchain (read-only) via RUSTUP_HOME.
const REAL_HOME = process.env.HOME || '';
const RUSTUP_HOME = process.env.RUSTUP_HOME || path.join(REAL_HOME, '.rustup');
const CARGO_HOME = '/tmp/ide-exthost-test-cargo';

let ctx;

before(async () => {
	const extMap = await ensureExtensions();
	const { root, files } = await makeFixtures();

	// ESLint resolves `eslint` from the workspace; install it + flat-config preset.
	installEslint(files.tsProj);

	// vsix extraction loses the executable bit on the rust-analyzer server binary.
	for (const dir of Object.values(extMap)) { chmodRustAnalyzer(dir); }

	// A dedicated extensions dir whose immediate children are the 6 extensions.
	const extensionsDir = path.dirname(Object.values(extMap)[0]);

	rmSync(DATA_DIR, { recursive: true, force: true });
	mkdirSync(path.join(DATA_DIR, 'User'), { recursive: true });
	mkdirSync(HOME_DIR, { recursive: true });
	writeFileSync(path.join(DATA_DIR, 'User', 'settings.json'), JSON.stringify({
		'telemetry.telemetryLevel': 'off',
		'python.languageServer': 'Jedi',
		'python.experiments.enabled': false,
		'python.terminal.activateEnvironment': false,
		'rust-analyzer.checkOnSave': false,
		'rust-analyzer.cargo.buildScripts.enable': false,
		'rust-analyzer.procMacro.enable': false,
		'rust-analyzer.cachePriming.enable': false,
		'gitlens.currentLine.enabled': true,
		'gitlens.statusBar.enabled': true,
		'gitlens.codeLens.enabled': false,
		'eslint.useFlatConfig': true,
		'eslint.run': 'onType',
	}, null, 2));

	const proc = spawn('node', [HOST, '--control-socket', CONTROL, '--extensions-dir', extensionsDir, '--data-dir', DATA_DIR, '--home', HOME_DIR], {
		stdio: ['ignore', 'inherit', 'inherit'],
		env: { ...process.env, HOME: HOME_DIR, IDE_DATA_DIR: DATA_DIR, CARGO_HOME, RUSTUP_HOME },
	});
	await sleep(1000);

	const control = connect(CONTROL);
	await control.ready();
	const folders = [files.tsProj, files.rustProj, path.join(root, 'pyproj'), files.gitDir];
	const open = await control.request('host/openWorkspace', { folders });
	await sleep(3000); // let the worker boot the ext host + run startup activation

	const lsp = connect(open.lspSocket);
	await lsp.ready();
	lsp.onServerRequest('window/showMessageRequest', () => null);
	lsp.onServerRequest('workspace/applyEdit', () => ({ applied: true }));
	await lsp.request('initialize', { processId: process.pid, rootUri: null, workspaceFolders: folders.map((f) => ({ uri: `file://${f}`, name: path.basename(f) })), capabilities: {} });
	lsp.notify('initialized', {});

	ctx = { proc, control, lsp, files, root, extensionsDir, workspaceId: open.workspaceId };
});

after(async () => {
	if (!ctx) { return; }
	try { await ctx.control.request('host/closeWorkspace', { workspaceId: ctx.workspaceId }); } catch { /* ignore */ }
	ctx.lsp.close();
	ctx.control.close();
	ctx.proc.kill('SIGTERM');
	await sleep(300);
});

function installEslint(tsProj) {
	if (existsSync(path.join(tsProj, 'node_modules', 'eslint'))) { return; }
	execFileSync('npm', ['install', '--no-save', '--no-audit', '--no-fund', 'eslint@9', '@eslint/js@9'], { cwd: tsProj, stdio: 'inherit', timeout: 180000 });
}

function chmodRustAnalyzer(dir) {
	for (const rel of ['server/rust-analyzer', 'server/rust-analyzer.exe', 'bin/rust-analyzer']) {
		const p = path.join(dir, rel);
		if (existsSync(p)) { try { chmodSync(p, 0o755); } catch { /* ignore */ } }
	}
}

function openDoc(uri, languageId, text) {
	ctx.lsp.notify('textDocument/didOpen', { textDocument: { uri, languageId, version: 1, text } });
}

import { readFileSync } from 'node:fs';
function fileText(p) { return readFileSync(p, 'utf8'); }

// Apply LSP TextEdits to `text` (non-overlapping; apply end-to-start) so Prettier's
// output can be asserted for correct content, not merely non-empty.
function applyTextEdits(text, edits) {
	const lines = text.split(/\r\n|\r|\n/);
	const offsetAt = (pos) => {
		let o = 0;
		for (let i = 0; i < pos.line; i++) { o += lines[i].length + 1; }
		return o + pos.character;
	};
	const sorted = [...edits].sort((a, b) => offsetAt(b.range.start) - offsetAt(a.range.start));
	let out = text;
	for (const e of sorted) { out = out.slice(0, offsetAt(e.range.start)) + e.newText + out.slice(offsetAt(e.range.end)); }
	return out;
}

test('all six extensions activate', { timeout: 120000 }, async () => {
	// Open a document per language to trigger onLanguage activations.
	openDoc(ctx.files.tsIndexUri, 'typescript', fileText(ctx.files.tsIndex));
	openDoc(ctx.files.rustMainUri, 'rust', fileText(ctx.files.rustMain));
	openDoc(ctx.files.pyMainUri, 'python', fileText(ctx.files.pyMain));
	openDoc(ctx.files.gitTrackedUri, 'plaintext', fileText(ctx.files.gitTracked));

	const wanted = ['dbaeumer.vscode-eslint', 'esbenp.prettier-vscode', 'eamodio.gitlens', 'ms-python.python', 'rust-lang.rust-analyzer', 'anthropic.claude-code'];
	let activated = [];
	for (let i = 0; i < 60; i++) {
		const res = await ctx.lsp.request('ide/host/activated', {});
		activated = res.activated.map((s) => s.toLowerCase());
		if (wanted.every((w) => activated.includes(w))) { break; }
		await sleep(1000);
	}
	console.log('ACTIVATED:', activated.join(', '));
	for (const w of wanted) { assert.ok(activated.includes(w), `extension did not activate: ${w}`); }
});

test('ESLint reports the expected diagnostics', { timeout: 60000 }, async () => {
	const note = await ctx.lsp.waitForNotification(
		(n) => n.method === 'textDocument/publishDiagnostics' && n.params.uri === ctx.files.tsIndexUri && n.params.diagnostics.length > 0,
		45000,
	);
	assert.ok(note, 'no ESLint diagnostics published');
	const diags = note.params.diagnostics;
	console.log('ESLINT diag:', JSON.stringify(diags[0]));
	// Correct content: ESLint (source "eslint") flags `debugger;` on line 2 and the unused `y`.
	assert.ok(diags.some((d) => /eslint/i.test(String(d.source ?? ''))), 'no diagnostic with source "eslint"');
	const debuggerDiag = diags.find((d) => /no-debugger/.test(String(d.code)));
	assert.ok(debuggerDiag, 'expected a no-debugger diagnostic');
	assert.equal(debuggerDiag.range.start.line, 2, 'no-debugger should flag line 2 (the `debugger;`)');
	assert.ok(diags.some((d) => /no-unused-vars/.test(String(d.code))), 'expected a no-unused-vars diagnostic');
});

test('Prettier formats the document correctly', { timeout: 60000 }, async () => {
	let edits = null;
	for (let i = 0; i < 20 && !edits; i++) {
		const r = await ctx.lsp.request('textDocument/formatting', { textDocument: { uri: ctx.files.tsIndexUri }, options: { tabSize: 2, insertSpaces: true } });
		if (r && r.length > 0) { edits = r; break; }
		await sleep(1000);
	}
	assert.ok(edits && edits.length > 0, 'no Prettier edits returned');
	const formatted = applyTextEdits(fileText(ctx.files.tsIndex), edits);
	console.log('PRETTIER formatted:\n' + formatted);
	// Correct content: Prettier normalizes the messy declaration + brace spacing and adds a final newline.
	assert.match(formatted, /const y = 1;/, 'Prettier did not normalize `const    y=1 ;`');
	assert.match(formatted, /function run\(\) \{/, 'Prettier did not normalize the function brace spacing');
	assert.match(formatted, /\n$/, 'Prettier did not add a trailing newline');
});

test('rust-analyzer resolves a cross-file definition', { timeout: 120000 }, async () => {
	const site = ctx.files.rustCallSite;
	let def = null;
	for (let i = 0; i < 110 && !def; i++) {
		const r = await ctx.lsp.request('textDocument/definition', { textDocument: { uri: site.uri }, position: { line: site.line, character: site.character } });
		if (Array.isArray(r) && r.length > 0) { def = r[0]; break; }
		await sleep(1000);
	}
	console.log('RUST definition:', JSON.stringify(def));
	assert.ok(def, 'rust-analyzer returned no definition for math::add (project did not load?)');
	assert.equal(def.uri, ctx.files.rustExpectedDefinitionUri, 'definition should point into src/math.rs');
	assert.equal(def.range.start.line, ctx.files.rustExpectedDefinitionLine, 'definition should be on the `pub fn add` line');
});

test('Python provides hover or definition', { timeout: 90000 }, async () => {
	const site = ctx.files.pyHoverSite;
	let result = null;
	for (let i = 0; i < 60 && !result; i++) {
		const hover = await ctx.lsp.request('textDocument/hover', { textDocument: { uri: site.uri }, position: { line: site.line, character: site.character } });
		if (hover && hover.contents && hover.contents.value) { result = { kind: 'hover', hover }; break; }
		const def = await ctx.lsp.request('textDocument/definition', { textDocument: { uri: site.uri }, position: { line: site.line, character: site.character } });
		if (Array.isArray(def) && def.length > 0) { result = { kind: 'definition', def }; break; }
		await sleep(1500);
	}
	console.log('PYTHON result:', JSON.stringify(result));
	assert.ok(result, 'Python provided neither hover nor definition (see DEFERRED.md if Jedi unavailable)');
	if (result.kind === 'hover') { assert.match(result.hover.contents.value, /greet/, 'hover should describe the `greet` function'); }
	else { assert.equal(result.def[0].range.start.line, ctx.files.pyExpectedDefinitionLine, 'definition should point to `def greet`'); }
});

test('GitLens shows a blame annotation on a committed line', { timeout: 60000 }, async () => {
	const note = await ctx.lsp.waitForNotification((n) => n.method === 'ide/decorations/set' && n.params.uri === ctx.files.gitTrackedUri, 40000);
	if (note) {
		const dec = note.params.decorations?.[0];
		console.log('GITLENS decoration:', JSON.stringify(dec));
		const annotation = dec?.after?.contentText ?? '';
		const hoverMsg = typeof dec?.hoverMessage === 'string' ? dec.hoverMessage : JSON.stringify(dec?.hoverMessage ?? '');
		assert.ok(annotation.length > 0 || /IDE Fixtures|initial commit|commit/i.test(hoverMsg), 'blame decoration carried no annotation');
	} else {
		const hover = await ctx.lsp.request('textDocument/hover', { textDocument: { uri: ctx.files.gitTrackedUri }, position: { line: 1, character: 0 } });
		console.log('GITLENS hover:', JSON.stringify(hover));
		assert.ok(hover && hover.contents && hover.contents.value && /IDE Fixtures|initial commit|commit/i.test(hover.contents.value), 'GitLens produced no blame annotation');
	}
});

test('Claude Code registers its webview view', { timeout: 60000 }, async () => {
	const note = await ctx.lsp.waitForNotification((n) => n.method === 'ide/views/register' && (n.params.kind === 'webview' || /claude/i.test(n.params.id)), 45000);
	console.log('CLAUDE view:', JSON.stringify(note?.params));
	assert.ok(note, 'no ide/views/register from Claude Code');
	const resolved = await ctx.lsp.request('ide/webview/resolveView', { viewId: note.params.id });
	console.log('CLAUDE resolved handle:', JSON.stringify(resolved));
	assert.ok(resolved && resolved.handle, 'resolveView returned no handle');
	const created = await ctx.lsp.waitForNotification((n) => (n.method === 'ide/webview/create' || n.method === 'ide/webview/setHtml') && n.params.html && n.params.html.length > 0, 30000);
	console.log('CLAUDE webview html length:', created?.params?.html?.length);
	assert.ok(created, 'Claude webview produced no HTML');
});

test('two windows: diagnostics route to the owning window only (isolation)', { timeout: 90000 }, async () => {
	// Two independent TS projects, each reusing the already-installed ESLint via a
	// node_modules symlink, opened as two SEPARATE windows (two openWorkspace calls,
	// two LSP sockets) that share the one extension host.
	const mkProject = (name) => {
		const dir = path.join(ctx.root, name);
		mkdirSync(path.join(dir, 'src'), { recursive: true });
		writeFileSync(path.join(dir, 'package.json'), fileText(ctx.files.tsPackageJson));
		writeFileSync(path.join(dir, 'eslint.config.mjs'), fileText(ctx.files.tsEslintFlat));
		writeFileSync(path.join(dir, '.eslintrc.json'), fileText(ctx.files.tsEslintrc));
		try { symlinkSync(path.join(ctx.files.tsProj, 'node_modules'), path.join(dir, 'node_modules'), 'dir'); } catch { /* already linked */ }
		const idx = path.join(dir, 'src', 'index.ts');
		writeFileSync(idx, 'const dead = 1;\ndebugger;\nexport const ok = 2;\n');
		return { dir, idx, idxUri: pathToFileURL(idx).href };
	};
	const A = mkProject('iso-a');
	const B = mkProject('iso-b');
	const openA = await ctx.control.request('host/openWorkspace', { folders: [A.dir] });
	const openB = await ctx.control.request('host/openWorkspace', { folders: [B.dir] });
	const lspA = connect(openA.lspSocket); await lspA.ready();
	const lspB = connect(openB.lspSocket); await lspB.ready();
	for (const l of [lspA, lspB]) {
		l.onServerRequest('window/showMessageRequest', () => null);
		l.onServerRequest('workspace/applyEdit', () => ({ applied: true }));
	}
	const wf = (f) => [{ uri: pathToFileURL(f).href, name: path.basename(f) }];
	await lspA.request('initialize', { processId: process.pid, rootUri: pathToFileURL(A.dir).href, workspaceFolders: wf(A.dir), capabilities: {} });
	lspA.notify('initialized', {});
	await lspB.request('initialize', { processId: process.pid, rootUri: pathToFileURL(B.dir).href, workspaceFolders: wf(B.dir), capabilities: {} });
	lspB.notify('initialized', {});
	await sleep(500);
	lspA.notify('textDocument/didOpen', { textDocument: { uri: A.idxUri, languageId: 'typescript', version: 1, text: fileText(A.idx) } });
	lspB.notify('textDocument/didOpen', { textDocument: { uri: B.idxUri, languageId: 'typescript', version: 1, text: fileText(B.idx) } });

	const diagFor = (uri) => (n) => n.method === 'textDocument/publishDiagnostics' && n.params.uri === uri && n.params.diagnostics.length > 0;
	const noteA = await lspA.waitForNotification(diagFor(A.idxUri), 60000);
	const noteB = await lspB.waitForNotification(diagFor(B.idxUri), 60000);
	// Each window must receive diagnostics for its OWN file on its OWN socket — this
	// only happens if per-window URI routing works (broken routing -> they'd arrive on
	// the primary window and these waits would time out).
	assert.ok(noteA, 'window A received no diagnostics for its own file');
	assert.ok(noteB, 'window B received no diagnostics for its own file');
	assert.ok(noteA.params.diagnostics.some((d) => /no-debugger/.test(String(d.code))), 'window A diagnostics not from ESLint');
	assert.ok(noteB.params.diagnostics.some((d) => /no-debugger/.test(String(d.code))), 'window B diagnostics not from ESLint');
	await sleep(1500); // allow any mis-routed notifications to arrive before asserting isolation
	assert.equal(lspA.takeNotifications((n) => n.method === 'textDocument/publishDiagnostics' && n.params.uri === B.idxUri).length, 0, 'window A leaked window B diagnostics');
	assert.equal(lspB.takeNotifications((n) => n.method === 'textDocument/publishDiagnostics' && n.params.uri === A.idxUri).length, 0, 'window B leaked window A diagnostics');

	lspA.close(); lspB.close();
	await ctx.control.request('host/closeWorkspace', { workspaceId: openA.workspaceId });
	await ctx.control.request('host/closeWorkspace', { workspaceId: openB.workspaceId });
});

test('late-joining window replays contributions and resolves its own webview', { timeout: 90000 }, async () => {
	// By now the primary window (window A = ctx.lsp) has activated every extension,
	// so the host already holds live contributions: status-bar items, a non-empty
	// command list and Claude's registered webview view. A window that opens *now*
	// must be brought up to date (the late-join replay) rather than seeing nothing.
	const dirB = path.join(ctx.root, 'late-join-b');
	mkdirSync(path.join(dirB, 'src'), { recursive: true });
	writeFileSync(path.join(dirB, 'index.ts'), 'export const x = 1;\n');
	const openB = await ctx.control.request('host/openWorkspace', { folders: [dirB] });
	const lspB = connect(openB.lspSocket);
	await lspB.ready();
	lspB.onServerRequest('window/showMessageRequest', () => null);
	lspB.onServerRequest('workspace/applyEdit', () => ({ applied: true }));
	await lspB.request('initialize', { processId: process.pid, rootUri: pathToFileURL(dirB).href, workspaceFolders: [{ uri: pathToFileURL(dirB).href, name: 'late-join-b' }], capabilities: {} });
	lspB.notify('initialized', {});

	// Replayed contributions are buffered on the window until its LSP client
	// attaches, so they flush right after initialize — no re-activation needed.
	const status = await lspB.waitForNotification((n) => n.method === 'ide/statusBar/set', 15000);
	assert.ok(status, 'late window B received no replayed status-bar item');
	const viewNote = await lspB.waitForNotification((n) => n.method === 'ide/views/register' && (n.params.kind === 'webview' || /claude/i.test(n.params.id)), 15000);
	assert.ok(viewNote, 'late window B received no replayed view registration');
	const cmds = await lspB.request('ide/commands/list', {});
	assert.ok(cmds.commands.length > 0, 'late window B sees an empty command list');
	console.log('LATE-JOIN B: status', JSON.stringify(status.params.id), 'view', JSON.stringify(viewNote.params.id), 'commands', cmds.commands.length);

	// Window B resolves its OWN Claude webview instance: a handle distinct from
	// window A's, with the create delivered only on B's socket (per-window view).
	const resolvedA = await ctx.lsp.request('ide/webview/resolveView', { viewId: viewNote.params.id });
	const resolvedB = await lspB.request('ide/webview/resolveView', { viewId: viewNote.params.id });
	assert.ok(resolvedA.handle && resolvedB.handle, 'resolveView returned no handle');
	assert.notEqual(resolvedB.handle, resolvedA.handle, 'late window B must resolve its own distinct webview handle');
	const createdB = await lspB.waitForNotification((n) => (n.method === 'ide/webview/create' || n.method === 'ide/webview/setHtml') && n.params.handle === resolvedB.handle && n.params.html && n.params.html.length > 0, 30000);
	assert.ok(createdB, 'late window B webview produced no HTML');
	console.log('LATE-JOIN B: own webview handle', JSON.stringify(resolvedB.handle), 'html', createdB.params.html.length, 'bytes');

	// B's resolved webview must not leak onto window A's socket.
	await sleep(500);
	assert.equal(ctx.lsp.takeNotifications((n) => (n.method === 'ide/webview/create' || n.method === 'ide/webview/setHtml') && n.params.handle === resolvedB.handle).length, 0, 'window A leaked window B webview');

	lspB.close();
	await ctx.control.request('host/closeWorkspace', { workspaceId: openB.workspaceId });
});
