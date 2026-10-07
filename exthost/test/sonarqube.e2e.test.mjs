// REAL end-to-end connected-mode test: a live `sonarqube:community` Docker
// container + the real `SonarSource.sonarlint-vscode` extension running inside
// the shared extension host, driven into connected mode by the built-in
// `ide.sonarqube` assistant, must surface a known Sonar rule issue in a TS file
// as a standard LSP `textDocument/publishDiagnostics` on the window socket.
//
// Unlike sonarqube.test.mjs (mock HTTP server, no Docker, no language server),
// this proves the whole stack: Docker → SonarQube Web API first-run auth →
// assistant connect/bind → SonarLint's bundled JRE + Java language server +
// JS/TS analyzer → host diagnostics routing. It also re-asserts the security
// contract: the user token lands in SecretState (secrets.json) ONLY, never in
// settings.json.
//
// Docker-gated: if `docker info` fails the single test SKIPS with a clear
// message (it never fails merely because Docker is absent).
//
// Run: cd exthost && node --test --test-concurrency=1 test/sonarqube.e2e.test.mjs
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { mkdirSync, rmSync, writeFileSync, readFileSync, existsSync, readdirSync } from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { ensureExtensions } from './extensions.mjs';
import { connect, sleep } from './client.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const HOST = path.join(here, '..', 'dist', 'host.js');

// Hermetic to /tmp + this repo. A fresh data dir per run keeps SecretState,
// settings and the per-folder binding deterministic.
const TMP = '/tmp/ide-sonarqube-e2e';
const DATA_DIR = path.join(TMP, 'data');
const HOME_DIR = path.join(TMP, 'home');
// The host loads extensions from immediate child *directories* of the extensions
// dir; symlinks are skipped, so we point it at the real ensureExtensions cache
// (set in before()) rather than a dedicated symlink tree.
const REPO_DIR = path.join(TMP, 'repo');
const CONTROL = path.join(DATA_DIR, 'control.sock');

const CONTAINER = 'ide-sonarqube';
const IMAGE = 'sonarqube:community';
// The container publishes 127.0.0.1:9000 (IPv4 only); use that explicit host so
// the SonarLint Java LS connects deterministically (avoids a `localhost`→::1 miss).
// The same string is the connection serverUrl AND the SecretStorage key.
const SERVER_URL = 'http://127.0.0.1:9000';
const PROJECT_KEY = 'ide-e2e';
const ADMIN_NEW_PASSWORD = 'ide-e2e-Str0ng-Pass'; // >=8 chars, must differ from 'admin'
// SonarLint's SecretStorage namespace. VS Code keys context.secrets by
// ExtensionIdentifier.toKey(id) = LOWERCASED, and the host's ide.secretState.*
// bridge normalizes to match, so the token lands under the lowercased id — which
// is exactly what SonarLint's LS reads (GetTokenForServer → secrets.get).
const SONARLINT_SECRET_NS = 'sonarsource.sonarlint-vscode';

// Default "Sonar way" JS/TS rules this smell trips: S6679 (self-comparison /
// NaN check on `a == a`), S1854 (dead store) + S1481 (unused local `unused`).
// At least one must surface as a Sonar diagnostic.
const SMELL_TS = 'export function f(a: number) {\n  if (a == a) { return 1; }\n  let unused = 42;\n  return 0;\n}\n';

const UP_TIMEOUT_MS = 4 * 60 * 1000;
const DIAG_TIMEOUT_MS = 4 * 60 * 1000;

let SKIP = false;
let SKIP_REASON = '';
let ctx = null;
let userToken = null;
let firstDiag = null;
let timeToFirstDiagMs = null;
let extensionsDir = null; // real ensureExtensions cache dir (immediate child dirs)
const addedSettings = {};

// ---- shell + HTTP helpers ---------------------------------------------------

function sh(args, opts = {}) {
	return execFileSync('docker', args, { encoding: 'utf8', ...opts });
}

function dockerAvailable() {
	try {
		execFileSync('docker', ['info'], { stdio: ['ignore', 'ignore', 'ignore'] });
		return true;
	} catch {
		return false;
	}
}

// The TEST runs in plain Node (the host stubs undici, the test does not), so the
// global fetch is fine here. Auth is HTTP Basic.
async function api(pathname, { method = 'GET', auth, params } = {}) {
	const url = new URL(SERVER_URL + pathname);
	const headers = {};
	if (auth) { headers.Authorization = 'Basic ' + Buffer.from(auth).toString('base64'); }
	let body;
	if (params) {
		body = new URLSearchParams(params).toString();
		headers['Content-Type'] = 'application/x-www-form-urlencoded';
	}
	const res = await fetch(url, { method, headers, body });
	const text = await res.text();
	let json = null;
	if (text) { try { json = JSON.parse(text); } catch { json = text; } }
	return { status: res.status, json, text };
}

async function waitForServerUp(timeoutMs) {
	const deadline = Date.now() + timeoutMs;
	for (;;) {
		try {
			const r = await api('/api/system/status');
			if (r.json && r.json.status === 'UP') { return; }
		} catch { /* not reachable yet */ }
		if (Date.now() > deadline) { throw new Error('SonarQube never reported status UP within timeout'); }
		await sleep(3000);
	}
}

// Fresh server may need a few seconds after UP before the admin account accepts
// writes; retry the first-run calls briefly.
async function apiRetry(pathname, opts, okStatuses, tries = 10) {
	let last;
	for (let i = 0; i < tries; i++) {
		last = await api(pathname, opts);
		if (okStatuses.includes(last.status)) { return last; }
		await sleep(2000);
	}
	throw new Error(`${pathname} failed: HTTP ${last.status} ${last.text}`);
}

// ---- lifecycle --------------------------------------------------------------

before(async () => {
	if (!dockerAvailable()) {
		SKIP = true;
		SKIP_REASON = 'Docker not available (`docker info` failed); skipping real SonarQube e2e';
		return;
	}

	// (a) Guarantee a FRESH server: drop any prior container + named volumes.
	try { sh(['rm', '-f', CONTAINER], { stdio: 'ignore' }); } catch { /* none */ }
	for (const v of ['sonarqube_data', 'sonarqube_extensions', 'sonarqube_logs']) {
		try { sh(['volume', 'rm', '-f', v], { stdio: 'ignore' }); } catch { /* none */ }
	}
	sh([
		'run', '-d', '--name', CONTAINER,
		'-p', '127.0.0.1:9000:9000',
		'-v', 'sonarqube_data:/opt/sonarqube/data',
		'-v', 'sonarqube_extensions:/opt/sonarqube/extensions',
		'-v', 'sonarqube_logs:/opt/sonarqube/logs',
		'--restart', 'unless-stopped',
		IMAGE,
	], { stdio: 'ignore' });

	// (b) Poll until UP (fresh SonarQube takes 1–3 min).
	await waitForServerUp(UP_TIMEOUT_MS);

	// (c) First-run auth: change admin:admin -> strong password, mint a user
	// token, create the project. 204 = no content (change_password), 200 = ok.
	await apiRetry('/api/users/change_password',
		{ method: 'POST', auth: 'admin:admin', params: { login: 'admin', previousPassword: 'admin', password: ADMIN_NEW_PASSWORD } },
		[200, 204]);
	const tok = await apiRetry('/api/user_tokens/generate',
		{ method: 'POST', auth: `admin:${ADMIN_NEW_PASSWORD}`, params: { name: `ide-e2e-${Date.now()}` } },
		[200]);
	userToken = tok.json && tok.json.token;
	assert.ok(userToken, 'SonarQube did not return a user token: ' + tok.text);
	await apiRetry('/api/projects/create',
		{ method: 'POST', auth: `admin:${ADMIN_NEW_PASSWORD}`, params: { project: PROJECT_KEY, name: PROJECT_KEY } },
		[200]);

	// (d) Extensions: download/extract (idempotent). The host only loads immediate
	// child *directories* of --extensions-dir and skips symlinks, so point it at the
	// real ensureExtensions cache (whose children are real dirs). SonarLint loads
	// from there; the built-in assistant loads from the host itself.
	const extMap = await ensureExtensions();
	const sonarDir = extMap['sonarsource.sonarlint-vscode'];
	assert.ok(sonarDir && existsSync(path.join(sonarDir, 'package.json')), 'SonarLint extension not available');
	extensionsDir = path.dirname(sonarDir);
	// SonarLint's bundled JRE: jre/<entry> (same resolution SonarLint uses). Point
	// sonarlint.ls.javaHome at it explicitly so the Java LS launches deterministically.
	const jreRoot = path.join(sonarDir, 'jre');
	const jreHome = path.join(jreRoot, readdirSync(jreRoot)[0]);
	assert.ok(existsSync(path.join(jreHome, 'bin', 'java')), 'embedded JRE missing bin/java at ' + jreHome);

	for (const d of [DATA_DIR, HOME_DIR, REPO_DIR]) { rmSync(d, { recursive: true, force: true }); }
	mkdirSync(path.join(DATA_DIR, 'User'), { recursive: true });
	mkdirSync(HOME_DIR, { recursive: true });
	mkdirSync(REPO_DIR, { recursive: true });

	// pathToNodeExecutable + ls.javaHome make SonarLint's JS/TS analysis + Java LS
	// hermetic to this run (the host runs under node, so node is also on PATH).
	Object.assign(addedSettings, {
		'telemetry.telemetryLevel': 'off',
		'sonarlint.disableTelemetry': true,
		'sonarlint.pathToNodeExecutable': process.execPath,
		'sonarlint.ls.javaHome': jreHome,
	});
	writeFileSync(path.join(DATA_DIR, 'User', 'settings.json'), JSON.stringify(addedSettings, null, 2));

	// A git repo (bindWorkspace groups folders by repo identity) with the smelly TS.
	const git = (...a) => execFileSync('git', a, { cwd: REPO_DIR, stdio: 'ignore' });
	git('init', '-q');
	git('config', 'user.email', 'e2e@example.com');
	git('config', 'user.name', 'ide-e2e');
	git('remote', 'add', 'origin', `https://example.com/acme/${PROJECT_KEY}.git`);
	writeFileSync(path.join(REPO_DIR, 'index.ts'), SMELL_TS);
	git('add', '-A');
	git('commit', '-q', '-m', 'init');

	// Boot the host; capture stderr so a JRE/LS launch failure surfaces as evidence.
	const proc = spawn('node', [HOST, '--control-socket', CONTROL, '--extensions-dir', extensionsDir, '--data-dir', DATA_DIR, '--home', HOME_DIR], {
		stdio: ['ignore', 'pipe', 'pipe'],
		env: { ...process.env, HOME: HOME_DIR, IDE_DATA_DIR: DATA_DIR },
	});
	let hostLog = '';
	const cap = (d) => { hostLog += d; process.stderr.write(d); };
	proc.stdout.on('data', cap);
	proc.stderr.on('data', cap);
	await sleep(1500);

	const control = connect(CONTROL);
	await control.ready();
	const open = await control.request('host/openWorkspace', { folders: [REPO_DIR] });
	await sleep(2500); // let the worker boot the ext host + run onStartupFinished activation

	const lsp = connect(open.lspSocket);
	await lsp.ready();

	// Script the assistant's interactive prompts (native UI requests in prod).
	lsp.onServerRequest('window/showInputBox', (p) => {
		if (p.password) { return { value: userToken }; }
		if (/server url/i.test(p.prompt || '')) { return { value: SERVER_URL }; }
		if (/project key/i.test(p.prompt || '')) { return { value: PROJECT_KEY }; }
		return null;
	});
	lsp.onServerRequest('window/showQuickPick', (p) => {
		const items = p.items || [];
		// Prefer the pre-created project; else "Create new…"; else first item.
		const pick = items.find((i) => i.label === PROJECT_KEY)
			|| items.find((i) => /create new/i.test(i.label))
			|| items[0];
		return pick ? { handle: pick.handle } : null;
	});
	lsp.onServerRequest('window/showMessageRequest', (p) => {
		// Decline the "Bind now?" offer; Bind is driven explicitly below.
		const later = (p.actions || []).find((a) => /later/i.test(a.title));
		return later ? { title: later.title } : null;
	});

	await lsp.request('initialize', {
		processId: process.pid,
		rootUri: pathToFileURL(REPO_DIR).href,
		workspaceFolders: [{ uri: pathToFileURL(REPO_DIR).href, name: PROJECT_KEY }],
		capabilities: {},
	});
	lsp.notify('initialized', {});

	ctx = {
		proc, control, lsp,
		workspaceId: open.workspaceId,
		indexUri: pathToFileURL(path.join(REPO_DIR, 'index.ts')).href,
		stderr: () => hostLog,
	};
}, { timeout: 10 * 60 * 1000 });

after(async () => {
	if (ctx) {
		try { await ctx.control.request('host/closeWorkspace', { workspaceId: ctx.workspaceId }); } catch { /* ignore */ }
		try { ctx.lsp.close(); } catch { /* ignore */ }
		try { ctx.control.close(); } catch { /* ignore */ }
		try { ctx.proc.kill('SIGTERM'); } catch { /* ignore */ }
		await sleep(500);
	}
	// Stop (not remove) the container so it can be reused / inspected.
	if (!SKIP) { try { sh(['stop', CONTAINER], { stdio: 'ignore' }); } catch { /* ignore */ } }
	if (firstDiag) {
		console.log('\n=== SONARQUBE E2E SUMMARY ===');
		console.log('Sonar diagnostic:', JSON.stringify(firstDiag));
		console.log('time to first diagnostic (ms):', timeToFirstDiagMs);
		console.log('added settings:', JSON.stringify(addedSettings));
		console.log('token location: secrets.json only (asserted) — NOT settings.json');
	}
}, { timeout: 5 * 60 * 1000 });

function readJson(file) { return JSON.parse(readFileSync(file, 'utf8')); }

const isSonar = (d) =>
	/sonar/i.test(String(d.source ?? ''))
	|| /(?:typescript|javascript|ts|js):S\d+/i.test(String(d.code ?? ''))
	|| /^S\d+$/i.test(String(d.code ?? ''));

test('connected mode: a real Sonar diagnostic reaches the window (token in SecretState only)', { timeout: 12 * 60 * 1000 }, async (t) => {
	if (SKIP) { t.skip(SKIP_REASON); return; }

	// The built-in assistant activates onStartupFinished; wait for its commands.
	let cmds = [];
	for (let i = 0; i < 120; i++) {
		const r = await ctx.lsp.request('ide/commands/list', {});
		cmds = r.commands || [];
		if (cmds.some((c) => c.id === 'sonarqube.connectExisting')) { break; }
		await sleep(500);
	}
	assert.ok(cmds.some((c) => c.id === 'sonarqube.connectExisting'),
		'SonarQube assistant never registered its commands. Host stderr tail:\n' + ctx.stderr().slice(-3000));

	// Connect to the real server (URL + token prompts answered by the handlers).
	await ctx.lsp.request('workspace/executeCommand', { command: 'sonarqube.connectExisting', arguments: [] });

	// Security contract: token in SecretState (secrets.json) under SonarLint's
	// (lowercased) SecretStorage namespace, keyed by serverUrl; connection config
	// in settings WITHOUT the token.
	const secrets = readJson(path.join(DATA_DIR, 'User', 'secrets.json'));
	assert.equal(secrets[SONARLINT_SECRET_NS]?.[SERVER_URL], userToken,
		'user token must be stored in SecretState under SonarLint namespace keyed by server URL');
	const settings = readJson(path.join(DATA_DIR, 'User', 'settings.json'));
	assert.ok(!JSON.stringify(settings).includes(userToken), 'token must NOT appear in settings.json');
	const conns = settings['sonarlint.connectedMode.connections.sonarqube'];
	assert.ok(Array.isArray(conns) && conns.some((c) => c.serverUrl === SERVER_URL && c.connectionId === 'ide-local'),
		'connection config not written to settings.json');

	// Bind the workspace folder to the pre-created project `ide-e2e`.
	await ctx.lsp.request('workspace/executeCommand', { command: 'sonarqube.bindWorkspace', arguments: [] });
	const bindings = readJson(path.join(DATA_DIR, 'User', 'sonarqube-bindings.json'));
	assert.deepEqual(bindings[REPO_DIR], { projectKey: PROJECT_KEY, connectionId: 'ide-local' },
		'per-folder connected-mode binding not persisted');
	assert.ok(!JSON.stringify(bindings).includes(userToken), 'token must NOT appear in the bindings file');

	// Give connected-mode sync a moment, then open the smelly TS file.
	await sleep(3000);
	const text = readFileSync(path.join(REPO_DIR, 'index.ts'), 'utf8');
	const openDoc = (version) => ctx.lsp.notify('textDocument/didOpen', {
		textDocument: { uri: ctx.indexUri, languageId: 'typescript', version, text },
	});
	openDoc(1);

	// Wait for a publishDiagnostics on our file carrying a Sonar issue. Nudge once
	// by re-opening if nothing arrives in the first minute (SonarLint may still be
	// untarring its JRE / starting the Java LS / syncing the quality profile).
	const start = Date.now();
	const deadline = start + DIAG_TIMEOUT_MS;
	let note = null;
	let nudged = false;
	while (Date.now() < deadline) {
		note = await ctx.lsp.waitForNotification(
			(n) => n.method === 'textDocument/publishDiagnostics'
				&& n.params.uri === ctx.indexUri
				&& (n.params.diagnostics || []).some(isSonar),
			15000,
		);
		if (note) { break; }
		if (!nudged && Date.now() - start > 60000) {
			nudged = true;
			ctx.lsp.notify('textDocument/didClose', { textDocument: { uri: ctx.indexUri } });
			await sleep(500);
			openDoc(2);
		}
	}
	timeToFirstDiagMs = Date.now() - start;

	if (!note) {
		throw new Error(
			`No Sonar diagnostic for ${ctx.indexUri} within ${Math.round(DIAG_TIMEOUT_MS / 1000)}s.\n`
			+ 'Host stderr tail (SonarLint/JRE/LS evidence):\n' + ctx.stderr().slice(-5000),
		);
	}

	const sonarDiags = note.params.diagnostics.filter(isSonar);
	firstDiag = sonarDiags[0];
	console.log('SONAR diagnostic:', JSON.stringify(firstDiag));
	console.log('all sonar codes on file:', JSON.stringify(sonarDiags.map((d) => d.code)));
	console.log('time to first diagnostic (ms):', timeToFirstDiagMs);

	assert.ok(firstDiag, 'expected at least one Sonar diagnostic');
	assert.ok(isSonar(firstDiag), 'diagnostic is not a Sonar issue');
	assert.ok(firstDiag.range && firstDiag.range.start, 'diagnostic has no range');
});
