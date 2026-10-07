// Integration test for the built-in SonarQube setup assistant + the host
// plumbing it relies on (MainThreadQuickOpen showInputBox/showQuickPick, modal
// showMessage, contributed command title/category, the `ide.secretState.*` and
// `ide.sonarqube.setBindings` bridge commands, and folder-scoped binding config).
//
// It boots dist/host.js with an EMPTY user-extensions dir, so only the built-in
// `ide.sonarqube` extension (loaded from exthost/builtin) activates — no heavy
// language servers. A mock SonarQube HTTP server stands in for a real container,
// so this test needs neither Docker nor network. The real-container end-to-end
// test lives in sonarqube.e2e.test.mjs.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import http from 'node:http';
import { mkdirSync, rmSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { connect, sleep } from './client.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const HOST = path.join(here, '..', 'dist', 'host.js');
const DATA_DIR = '/tmp/ide-sonarqube-test-data';
const HOME_DIR = '/tmp/ide-sonarqube-test-home';
const EXT_DIR = '/tmp/ide-sonarqube-test-ext'; // intentionally empty
const REPO_DIR = '/tmp/ide-sonarqube-test-repo';
const CONTROL = path.join(DATA_DIR, 'control.sock');

const TOKEN = 'squ_mocktoken_abcdef0123456789';
const PROJECT_KEY = 'myproj';

let ctx;
let mock;

// A mock SonarQube Web API. Records the Authorization header seen on
// authenticated endpoints so the test can prove the token was actually sent.
function startMockSonar() {
	const seen = { validateAuth: null, createAuth: null, created: [] };
	const projects = [];
	const server = http.createServer((req, res) => {
		const url = new URL(req.url, 'http://localhost');
		let body = '';
		req.on('data', (c) => { body += c; });
		req.on('end', () => {
			const params = new URLSearchParams(body || url.search.slice(1));
			const auth = req.headers.authorization || '';
			const json = (code, obj) => { res.writeHead(code, { 'content-type': 'application/json' }); res.end(JSON.stringify(obj)); };
			if (url.pathname === '/api/system/status') { return json(200, { id: 'mock', version: '25.1', status: 'UP' }); }
			if (url.pathname === '/api/authentication/validate') { seen.validateAuth = auth; return json(200, { valid: true }); }
			if (url.pathname === '/api/projects/search') {
				return json(200, { paging: { pageIndex: 1, pageSize: 100, total: projects.length }, components: projects });
			}
			if (url.pathname === '/api/projects/create') {
				seen.createAuth = auth;
				const key = params.get('project');
				const name = params.get('name') || key;
				projects.push({ key, name });
				seen.created.push(key);
				return json(200, { project: { key, name } });
			}
			return json(404, { errors: [{ msg: `unexpected ${url.pathname}` }] });
		});
	});
	return new Promise((resolve) => {
		server.listen(0, '127.0.0.1', () => {
			const { port } = server.address();
			resolve({ server, port, url: `http://127.0.0.1:${port}`, seen });
		});
	});
}

before(async () => {
	mock = await startMockSonar();

	for (const d of [DATA_DIR, HOME_DIR, EXT_DIR, REPO_DIR]) { rmSync(d, { recursive: true, force: true }); }
	mkdirSync(path.join(DATA_DIR, 'User'), { recursive: true });
	mkdirSync(HOME_DIR, { recursive: true });
	mkdirSync(EXT_DIR, { recursive: true });
	mkdirSync(REPO_DIR, { recursive: true });
	writeFileSync(path.join(DATA_DIR, 'User', 'settings.json'), JSON.stringify({ 'telemetry.telemetryLevel': 'off' }, null, 2));

	// A git repo with a remote so the assistant derives a stable project key.
	const git = (...a) => execFileSync('git', a, { cwd: REPO_DIR, stdio: 'ignore' });
	git('init', '-q');
	git('remote', 'add', 'origin', 'https://example.com/acme/myproj.git');
	writeFileSync(path.join(REPO_DIR, 'index.ts'), 'export const x = 1;\n');

	const proc = spawn('node', [HOST, '--control-socket', CONTROL, '--extensions-dir', EXT_DIR, '--data-dir', DATA_DIR, '--home', HOME_DIR], {
		stdio: ['ignore', 'inherit', 'inherit'],
		env: { ...process.env, HOME: HOME_DIR, IDE_DATA_DIR: DATA_DIR },
	});
	await sleep(1000);

	const control = connect(CONTROL);
	await control.ready();
	const open = await control.request('host/openWorkspace', { folders: [REPO_DIR] });
	await sleep(1500);

	const lsp = connect(open.lspSocket);
	await lsp.ready();

	// Script the interactive prompts the assistant raises (these are the native
	// UI requests the host sends; in the real IDE they render as dialogs).
	lsp.onServerRequest('window/showInputBox', (p) => {
		if (p.password) { return { value: TOKEN }; }
		if (/server url/i.test(p.prompt || '')) { return { value: mock.url }; }
		if (/project key/i.test(p.prompt || '')) { return { value: PROJECT_KEY }; }
		return null;
	});
	lsp.onServerRequest('window/showQuickPick', (p) => {
		// Choose the "Create new project…" entry (first item).
		const create = p.items.find((i) => /create new/i.test(i.label)) || p.items[0];
		return create ? { handle: create.handle } : null;
	});
	lsp.onServerRequest('window/showMessageRequest', (p) => {
		// Decline the "Bind now?" offer so Connect stays isolated from Bind.
		const later = (p.actions || []).find((a) => /later/i.test(a.title));
		return later ? { title: later.title } : null;
	});

	await lsp.request('initialize', { processId: process.pid, rootUri: null, workspaceFolders: [{ uri: `file://${REPO_DIR}`, name: 'myproj' }], capabilities: {} });
	lsp.notify('initialized', {});

	ctx = { proc, control, lsp, workspaceId: open.workspaceId };
});

after(async () => {
	if (ctx) {
		try { await ctx.control.request('host/closeWorkspace', { workspaceId: ctx.workspaceId }); } catch { /* ignore */ }
		ctx.lsp.close();
		ctx.control.close();
		ctx.proc.kill('SIGTERM');
		await sleep(300);
	}
	if (mock) { mock.server.close(); }
});

function readJson(file) { return JSON.parse(readFileSync(file, 'utf8')); }

test('the SonarQube assistant activates and contributes titled, categorized commands', { timeout: 60000 }, async () => {
	let cmds = [];
	for (let i = 0; i < 40; i++) {
		const res = await ctx.lsp.request('ide/commands/list', {});
		cmds = res.commands || [];
		if (cmds.some((c) => c.id === 'sonarqube.connectExisting')) { break; }
		await sleep(500);
	}
	const setup = cmds.find((c) => c.id === 'sonarqube.setupLocalServer');
	assert.ok(setup, 'sonarqube.setupLocalServer not registered');
	assert.equal(setup.title, 'Set Up Local Server', 'command title should come from contributes.commands');
	assert.equal(setup.category, 'SonarQube', 'command category should be SonarQube');
});

test('Connect to Existing Server: validates, writes config, stores token in SecretState only', { timeout: 60000 }, async () => {
	await ctx.lsp.request('workspace/executeCommand', { command: 'sonarqube.connectExisting', arguments: [] });

	// The token was actually presented to the server on validate.
	assert.ok(mock.seen.validateAuth, 'server never received a validate call');
	const decoded = mock.seen.validateAuth.startsWith('Bearer ')
		? mock.seen.validateAuth.slice('Bearer '.length)
		: Buffer.from(mock.seen.validateAuth.replace(/^Basic /, ''), 'base64').toString('utf8').replace(/:$/, '');
	assert.equal(decoded, TOKEN, 'validate must carry the user token');

	// Connection config written WITHOUT the token.
	const settings = readJson(path.join(DATA_DIR, 'User', 'settings.json'));
	const conns = settings['sonarlint.connectedMode.connections.sonarqube'];
	assert.ok(Array.isArray(conns) && conns.length === 1, 'connection not written to settings');
	assert.equal(conns[0].serverUrl, mock.url);
	assert.equal(conns[0].connectionId, 'ide-local');
	const settingsText = JSON.stringify(settings);
	assert.ok(!settingsText.includes(TOKEN), 'token must NOT appear in settings.json');

	// Token stored in SonarLint's SecretStorage namespace, keyed by server URL.
	const secrets = readJson(path.join(DATA_DIR, 'User', 'secrets.json'));
	// SecretStorage namespaces by ExtensionIdentifier.toKey (LOWERCASED), matching
	// how SonarLint's context.secrets reads it.
	assert.equal(secrets['sonarsource.sonarlint-vscode']?.[mock.url], TOKEN, 'token must be stored in SecretState under SonarLint id (lowercased) keyed by server URL');
});

test('Connected status bar item appears', { timeout: 60000 }, async () => {
	const note = await ctx.lsp.waitForNotification(
		(n) => n.method === 'ide/statusBar/set' && /SonarQube: connected/.test(String(n.params?.text ?? '')),
		20000,
	);
	assert.ok(note, 'no "SonarQube: connected" status bar item was set');
});

test('Bind Workspace: creates a project and writes a folder-scoped binding', { timeout: 60000 }, async () => {
	await ctx.lsp.request('workspace/executeCommand', { command: 'sonarqube.bindWorkspace', arguments: [] });

	assert.deepEqual(mock.seen.created, [PROJECT_KEY], 'the project should have been created on the server');

	const bindings = readJson(path.join(DATA_DIR, 'User', 'sonarqube-bindings.json'));
	assert.deepEqual(bindings[REPO_DIR], { projectKey: PROJECT_KEY, connectionId: 'ide-local' }, 'per-folder binding not persisted');
	// No token leaked into the bindings file either.
	assert.ok(!JSON.stringify(bindings).includes(TOKEN));
});
