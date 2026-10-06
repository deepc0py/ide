// End-to-end test of the shared extension host: downloads the 6 required
// extensions from Open VSX, builds sample projects, boots dist/host.js, opens a
// multi-root workspace over the control socket, then drives each extension's core
// feature through the per-workspace LSP socket.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, readdirSync, writeFileSync, rmSync } from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ensureExtensions } from './extensions.mjs';
import { makeFixtures } from './fixtures.mjs';
import { connect, sleep } from './client.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const HOST = path.join(here, '..', 'dist', 'host.js');
const DATA_DIR = '/tmp/ide-exthost-test-data';
const HOME_DIR = '/tmp/ide-exthost-test-home';
const CONTROL = path.join(DATA_DIR, 'control.sock');

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
		'gitlens.currentLine.enabled': true,
		'gitlens.statusBar.enabled': true,
		'gitlens.codeLens.enabled': false,
		'eslint.useFlatConfig': true,
		'eslint.run': 'onType',
	}, null, 2));

	const proc = spawn('node', [HOST, '--control-socket', CONTROL, '--extensions-dir', extensionsDir, '--data-dir', DATA_DIR, '--home', HOME_DIR], {
		stdio: ['ignore', 'inherit', 'inherit'],
		env: { ...process.env, HOME: HOME_DIR, IDE_DATA_DIR: DATA_DIR },
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

	ctx = { proc, control, lsp, files, workspaceId: open.workspaceId };
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

test('ESLint reports a diagnostic', { timeout: 60000 }, async () => {
	const note = await ctx.lsp.waitForNotification(
		(n) => n.method === 'textDocument/publishDiagnostics' && n.params.uri === ctx.files.tsIndexUri && n.params.diagnostics.length > 0,
		45000,
	);
	assert.ok(note, 'no ESLint diagnostics published');
	const sources = note.params.diagnostics.map((d) => d.source || '').join(',');
	console.log('ESLINT diag:', JSON.stringify(note.params.diagnostics[0]));
	assert.ok(/eslint/i.test(sources) || note.params.diagnostics.some((d) => /no-unused-vars|no-debugger/.test(String(d.code))), 'diagnostics not from eslint');
});

test('Prettier returns formatting edits', { timeout: 60000 }, async () => {
	let edits = null;
	for (let i = 0; i < 20 && !edits; i++) {
		const r = await ctx.lsp.request('textDocument/formatting', { textDocument: { uri: ctx.files.tsIndexUri }, options: { tabSize: 2, insertSpaces: true } });
		if (r && r.length > 0) { edits = r; break; }
		await sleep(1000);
	}
	console.log('PRETTIER edits:', JSON.stringify(edits));
	assert.ok(edits && edits.length > 0, 'no Prettier edits returned');
});

test('rust-analyzer provides language features (definition or hover)', { timeout: 120000 }, async () => {
	const site = ctx.files.rustCallSite;
	let definition = null;
	let hover = null;
	for (let i = 0; i < 90 && !definition && !hover; i++) {
		const r = await ctx.lsp.request('textDocument/definition', { textDocument: { uri: site.uri }, position: { line: site.line, character: site.character } });
		if (Array.isArray(r) && r.length > 0) { definition = r[0]; break; }
		const h = await ctx.lsp.request('textDocument/hover', { textDocument: { uri: site.uri }, position: { line: site.line, character: site.character } });
		if (h && h.contents && h.contents.value) { hover = h; break; }
		await sleep(1000);
	}
	console.log('RUST definition:', JSON.stringify(definition), 'hover:', JSON.stringify(hover));
	assert.ok(definition || hover, 'rust-analyzer returned neither definition nor hover');
	if (definition) { assert.equal(definition.range.start.line, ctx.files.rustExpectedDefinitionLine); }
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
});

test('GitLens shows blame (decoration or hover) on a committed line', { timeout: 60000 }, async () => {
	const note = await ctx.lsp.waitForNotification((n) => n.method === 'ide/decorations/set' && n.params.uri === ctx.files.gitTrackedUri, 40000);
	let ok = Boolean(note);
	if (!ok) {
		const hover = await ctx.lsp.request('textDocument/hover', { textDocument: { uri: ctx.files.gitTrackedUri }, position: { line: 1, character: 0 } });
		ok = Boolean(hover && hover.contents && hover.contents.value);
		console.log('GITLENS hover:', JSON.stringify(hover));
	} else {
		console.log('GITLENS decoration:', JSON.stringify(note.params.decorations?.[0]));
	}
	assert.ok(ok, 'GitLens produced no blame decoration or hover');
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
