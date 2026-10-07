// SPDX-License-Identifier: MIT
// This file is OUR code (MIT-licensed), part of the IDE's built-in extension set.
// It does NOT bundle, patch, or redistribute SonarQube or the SonarLint extension;
// it only drives the Docker CLI and SonarQube's HTTP Web API, and configures the
// already-installed `SonarSource.sonarlint-vscode` extension via public settings
// plus the host's pinned command contracts.
//
// SonarQube in-IDE setup assistant.
//   - Runs/connects ONE singleton SonarQube Community server shared by all windows.
//   - Puts SonarLint into connected mode, bound per worktree (grouped by repo).
//
// Plain CommonJS; depends only on the `vscode` API and Node built-ins. It executes
// inside the shared extension host's Node isolate, so child_process + fetch work.
'use strict';

const vscode = require('vscode');
const { execFile } = require('node:child_process');
const path = require('node:path');
const http = require('node:http');
const https = require('node:https');

// ---------------------------------------------------------------------------
// Constants (pinned)
// ---------------------------------------------------------------------------

const CONTAINER_NAME = 'ide-sonarqube';
const SONARQUBE_IMAGE = 'sonarqube:community';
const DEFAULT_SERVER_URL = 'http://localhost:9000';
const CONNECTION_ID = 'ide-local';
// SonarLint's SecretStorage namespace (the language server fetches the token by
// key = the exact serverUrl string). Pinned host contract.
const SONARLINT_EXT_ID = 'SonarSource.sonarlint-vscode';
// This extension's own SecretStorage keys (never settings, never logs).
const SECRET_ADMIN_PASSWORD = 'admin-password';
const SECRET_USER_TOKEN = 'user-token';

const STATUS_POLL_TIMEOUT_MS = 5 * 60 * 1000; // ~5 min server boot budget
const STATUS_POLL_INTERVAL_MS = 3000;

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/** @enum {string} Explicit setup state enum driving the status-bar item. */
const State = Object.freeze({
	NotConfigured: 'not configured',
	Starting: 'starting',
	Connected: 'connected',
	Stopped: 'stopped',
	Error: 'error',
});

/** @type {vscode.StatusBarItem} */
let statusBarItem;
/** @type {string} */
let currentState = State.NotConfigured;
/** @type {vscode.ExtensionContext} */
let ctx;
/** @type {vscode.OutputChannel} */
let out;

function log(message) {
	// NEVER log tokens or passwords; callers must pass only non-secret text.
	if (out) {
		out.appendLine(`[sonarqube] ${message}`);
	}
}

function setState(next) {
	currentState = next;
	if (!statusBarItem) {
		return;
	}
	switch (next) {
		case State.Starting:
			statusBarItem.text = '$(sync~spin) SonarQube: starting…';
			break;
		case State.Connected:
			statusBarItem.text = '$(check) SonarQube: connected';
			break;
		case State.Error:
			statusBarItem.text = '$(error) SonarQube: error';
			break;
		case State.Stopped:
			statusBarItem.text = '$(circle-slash) SonarQube: stopped';
			break;
		case State.NotConfigured:
		default:
			statusBarItem.text = '$(circle-outline) SonarQube: not configured';
			break;
	}
	statusBarItem.show();
}

// ---------------------------------------------------------------------------
// Docker helpers
// ---------------------------------------------------------------------------

/**
 * Run a binary and capture stdout/stderr. Rejects with a sanitized Error on a
 * non-zero exit. We only ever pass non-secret args to `docker`.
 * @returns {Promise<{stdout: string, stderr: string}>}
 */
function run(cmd, args, options) {
	return new Promise((resolve, reject) => {
		execFile(cmd, args, { encoding: 'utf8', ...(options || {}) }, (err, stdout, stderr) => {
			if (err) {
				const e = new Error(`${cmd} ${args.join(' ')} failed: ${(stderr || err.message || '').trim()}`);
				// @ts-ignore
				e.code = err.code;
				reject(e);
				return;
			}
			resolve({ stdout: stdout || '', stderr: stderr || '' });
		});
	});
}

/** @returns {Promise<boolean>} whether a usable Docker engine is reachable. */
async function dockerAvailable() {
	try {
		await run('docker', ['info']);
		return true;
	} catch {
		return false;
	}
}

/**
 * @returns {Promise<{exists: boolean, running: boolean, state: string}>}
 * Inspect the singleton container's presence/state.
 */
async function containerStatus() {
	try {
		const { stdout } = await run('docker', [
			'ps', '-a',
			'--filter', `name=^/${CONTAINER_NAME}$`,
			'--format', '{{.Names}} {{.State}}',
		]);
		const line = stdout.trim();
		if (!line) {
			return { exists: false, running: false, state: '' };
		}
		const state = line.split(/\s+/)[1] || '';
		return { exists: true, running: state === 'running', state };
	} catch {
		return { exists: false, running: false, state: '' };
	}
}

async function startExistingContainer() {
	await run('docker', ['start', CONTAINER_NAME]);
}

async function createContainer() {
	await run('docker', [
		'run', '-d',
		'--name', CONTAINER_NAME,
		'--restart', 'unless-stopped',
		'-p', '127.0.0.1:9000:9000',
		'-v', 'sonarqube_data:/opt/sonarqube/data',
		'-v', 'sonarqube_extensions:/opt/sonarqube/extensions',
		'-v', 'sonarqube_logs:/opt/sonarqube/logs',
		SONARQUBE_IMAGE,
	]);
}

async function stopContainer() {
	await run('docker', ['stop', CONTAINER_NAME]);
}

// ---------------------------------------------------------------------------
// HTTP helper (centralized; sets Authorization; parses JSON; throws on non-2xx)
// ---------------------------------------------------------------------------

/**
 * @typedef {Object} Auth
 * @property {{user: string, pass: string}} [basic]  Basic user:pass.
 * @property {string} [bearer]                        Bearer token.
 * @property {string} [basicToken]                    Basic <token>: (token as username).
 */

/**
 * Perform an HTTP request against the SonarQube Web API.
 * @param {string} serverUrl
 * @param {string} apiPath e.g. '/api/system/status'
 * @param {{ method?: string, auth?: Auth, params?: Record<string,string>, timeoutMs?: number }} [opts]
 * @returns {Promise<any>} parsed JSON (or null for empty bodies)
 */
function api(serverUrl, apiPath, opts) {
	const o = opts || {};
	const method = o.method || 'GET';
	const headers = { Accept: 'application/json' };

	if (o.auth) {
		if (o.auth.bearer) {
			headers.Authorization = `Bearer ${o.auth.bearer}`;
		} else if (o.auth.basicToken) {
			headers.Authorization = `Basic ${Buffer.from(`${o.auth.basicToken}:`).toString('base64')}`;
		} else if (o.auth.basic) {
			headers.Authorization = `Basic ${Buffer.from(`${o.auth.basic.user}:${o.auth.basic.pass}`).toString('base64')}`;
		}
	}

	const target = new URL(`${serverUrl.replace(/\/+$/, '')}${apiPath}`);
	let bodyStr;
	if (o.params) {
		if (method === 'GET' || method === 'HEAD') {
			for (const [k, v] of Object.entries(o.params)) { target.searchParams.set(k, v); }
		} else {
			// Send params as a form body so secrets never land in a URL/log.
			bodyStr = new URLSearchParams(o.params).toString();
			headers['Content-Type'] = 'application/x-www-form-urlencoded';
			headers['Content-Length'] = Buffer.byteLength(bodyStr);
		}
	}

	// Use Node's http/https directly (NOT global fetch): the shared host's bundle
	// stubs `undici` and layers VS Code proxy handling over fetch, which hangs.
	const lib = target.protocol === 'https:' ? https : http;
	return new Promise((resolve, reject) => {
		const req = lib.request(target, { method, headers, timeout: o.timeoutMs || 15000 }, (res) => {
			let data = '';
			res.setEncoding('utf8');
			res.on('data', (c) => { data += c; });
			res.on('end', () => {
				let body = null;
				if (data) { try { body = JSON.parse(data); } catch { body = data; } }
				const code = res.statusCode || 0;
				if (code >= 200 && code < 300) { resolve(body); return; }
				// SonarQube error bodies are {errors:[{msg}]}; those carry no secrets.
				let detail = '';
				if (body && typeof body === 'object' && Array.isArray(body.errors)) {
					detail = body.errors.map((x) => x && x.msg).filter(Boolean).join('; ');
				}
				const e = new Error(`HTTP ${code} ${apiPath}${detail ? `: ${detail}` : ''}`);
				e.status = code;
				reject(e);
			});
		});
		req.on('error', reject);
		req.on('timeout', () => { req.destroy(new Error(`request to ${apiPath} timed out`)); });
		if (bodyStr) { req.write(bodyStr); }
		req.end();
	});
}

/** @returns {Promise<string|null>} status string or null if unreachable. */
async function systemStatus(serverUrl) {
	try {
		const body = await api(serverUrl, '/api/system/status', { timeoutMs: 5000 });
		return body && body.status ? String(body.status) : null;
	} catch {
		return null;
	}
}

// ---------------------------------------------------------------------------
// SonarQube Web API wrappers
// ---------------------------------------------------------------------------

/** @returns {Promise<boolean>} whether the supplied auth is valid. */
async function validateAuth(serverUrl, auth) {
	const body = await api(serverUrl, '/api/authentication/validate', { method: 'POST', auth });
	return !!(body && body.valid === true);
}

async function changeAdminPassword(serverUrl, previousPassword, newPassword) {
	await api(serverUrl, '/api/users/change_password', {
		method: 'POST',
		auth: { basic: { user: 'admin', pass: previousPassword } },
		params: { login: 'admin', previousPassword, password: newPassword },
	});
}

/** @returns {Promise<string>} the generated user token. */
async function generateUserToken(serverUrl, adminPassword) {
	const name = `ide-${Date.now()}`;
	const body = await api(serverUrl, '/api/user_tokens/generate', {
		method: 'POST',
		auth: { basic: { user: 'admin', pass: adminPassword } },
		params: { name },
	});
	if (!body || !body.token) {
		throw new Error('token generation returned no token');
	}
	return String(body.token);
}

/** @returns {Promise<Array<{key: string, name: string}>>} */
async function searchProjects(serverUrl, auth) {
	const results = [];
	let page = 1;
	for (;;) {
		const body = await api(serverUrl, '/api/projects/search', {
			auth,
			params: { ps: '100', p: String(page) },
		});
		const comps = (body && body.components) || [];
		for (const c of comps) {
			results.push({ key: c.key, name: c.name || c.key });
		}
		const total = body && body.paging ? body.paging.total : comps.length;
		if (results.length >= total || comps.length === 0) {
			break;
		}
		page += 1;
	}
	return results;
}

async function createProject(serverUrl, auth, projectKey, projectName) {
	await api(serverUrl, '/api/projects/create', {
		method: 'POST',
		auth,
		params: { project: projectKey, name: projectName || projectKey },
	});
}

// ---------------------------------------------------------------------------
// Secrets + connection config (pinned host contracts)
// ---------------------------------------------------------------------------

async function storeSonarLintToken(serverUrl, token) {
	// Written into SonarLint's own SecretStorage namespace via the host command.
	await vscode.commands.executeCommand('ide.secretState.store', SONARLINT_EXT_ID, serverUrl, token);
}

async function deleteSonarLintToken(serverUrl) {
	await vscode.commands.executeCommand('ide.secretState.delete', SONARLINT_EXT_ID, serverUrl);
}

async function writeConnectionConfig(serverUrl) {
	await vscode.workspace.getConfiguration('sonarlint').update(
		'connectedMode.connections.sonarqube',
		[{ connectionId: CONNECTION_ID, serverUrl, disableNotifications: true }],
		vscode.ConfigurationTarget.Global,
	);
}

function readConnectionConfig() {
	const arr = vscode.workspace.getConfiguration('sonarlint').get('connectedMode.connections.sonarqube');
	if (Array.isArray(arr)) {
		const entry = arr.find((c) => c && c.connectionId === CONNECTION_ID) || arr[0];
		if (entry && entry.serverUrl) {
			return { serverUrl: String(entry.serverUrl) };
		}
	}
	return null;
}

/**
 * Best-effort credentials for admin Web API calls (project search/create):
 * a stored user token (Bearer), else stored admin password (Basic admin:*).
 * @returns {Promise<Auth|null>}
 */
async function resolveApiAuth() {
	const token = await ctx.secrets.get(SECRET_USER_TOKEN);
	if (token) {
		return { bearer: token };
	}
	const adminPassword = await ctx.secrets.get(SECRET_ADMIN_PASSWORD);
	if (adminPassword) {
		return { basic: { user: 'admin', pass: adminPassword } };
	}
	return null;
}

// ---------------------------------------------------------------------------
// Repo identity + per-worktree binding
// ---------------------------------------------------------------------------

/** Normalize a git remote URL into a stable repo identity. */
function normalizeRemote(url) {
	let s = url.trim();
	s = s.replace(/\.git$/i, '');
	// scp-like: git@host:owner/repo -> host/owner/repo
	const scp = s.match(/^[^@]+@([^:]+):(.+)$/);
	if (scp) {
		return `${scp[1]}/${scp[2]}`.toLowerCase();
	}
	// URL form: strip scheme + optional userinfo.
	s = s.replace(/^[a-z]+:\/\//i, '');
	s = s.replace(/^[^@/]+@/, '');
	return s.toLowerCase();
}

/**
 * Compute a stable repo identity + default project key for a workspace folder.
 * Identity from the git remote URL; fallback to the toplevel directory basename.
 * @returns {Promise<{identity: string, defaultKey: string}>}
 */
async function repoIdentity(folderFsPath) {
	try {
		const { stdout } = await run('git', ['remote', 'get-url', 'origin'], { cwd: folderFsPath });
		const remote = stdout.trim();
		if (remote) {
			const identity = normalizeRemote(remote);
			const base = identity.split('/').filter(Boolean).pop() || identity;
			return { identity, defaultKey: sanitizeKey(base) };
		}
	} catch {
		// no remote / not a git repo — fall through
	}
	try {
		const { stdout } = await run('git', ['rev-parse', '--show-toplevel'], { cwd: folderFsPath });
		const top = stdout.trim();
		if (top) {
			const base = path.basename(top);
			return { identity: `local:${top}`, defaultKey: sanitizeKey(base) };
		}
	} catch {
		// not a git repo at all
	}
	const base = path.basename(folderFsPath);
	return { identity: `local:${folderFsPath}`, defaultKey: sanitizeKey(base) };
}

/** SonarQube project keys allow [A-Za-z0-9_.:-]; map anything else to '-'. */
function sanitizeKey(name) {
	const k = String(name).replace(/[^A-Za-z0-9_.:-]/g, '-').replace(/^-+|-+$/g, '');
	return k || 'project';
}

/**
 * Group the current workspace folders by repo identity. Worktrees of the same
 * repo collapse into one group so they share a single binding.
 * @returns {Promise<Array<{identity: string, defaultKey: string, folders: vscode.WorkspaceFolder[]}>>}
 */
async function groupFoldersByRepo() {
	const folders = vscode.workspace.workspaceFolders || [];
	/** @type {Map<string, {identity: string, defaultKey: string, folders: vscode.WorkspaceFolder[]}>} */
	const groups = new Map();
	for (const folder of folders) {
		const { identity, defaultKey } = await repoIdentity(folder.uri.fsPath);
		let g = groups.get(identity);
		if (!g) {
			g = { identity, defaultKey, folders: [] };
			groups.set(identity, g);
		}
		g.folders.push(folder);
	}
	return [...groups.values()];
}

// ---------------------------------------------------------------------------
// Shared flows
// ---------------------------------------------------------------------------

/** Poll the server /api/system/status until UP, inside a progress notification. */
async function waitForServerUp(serverUrl) {
	return vscode.window.withProgress({
		location: vscode.ProgressLocation.Notification,
		title: 'Starting SonarQube (this can take 1–3 minutes)…',
		cancellable: true,
	}, async (progress, cancel) => {
		const deadline = Date.now() + STATUS_POLL_TIMEOUT_MS;
		for (;;) {
			if (cancel.isCancellationRequested) {
				throw new Error('cancelled while waiting for SonarQube to start');
			}
			const status = await systemStatus(serverUrl);
			if (status === 'UP') {
				return;
			}
			progress.report({ message: status ? `status: ${status}` : 'waiting for server…' });
			if (Date.now() > deadline) {
				throw new Error('timed out waiting for SonarQube to report status UP (~5 min)');
			}
			await delay(STATUS_POLL_INTERVAL_MS);
		}
	});
}

function delay(ms) {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * First-run auth: if admin:admin is still valid, force a password change, then
 * generate a user token. Returns the token to store for SonarLint.
 * @returns {Promise<string>}
 */
async function firstRunAuthAndToken(serverUrl) {
	let adminPassword = await ctx.secrets.get(SECRET_ADMIN_PASSWORD);

	// Detect a fresh server (default admin:admin still works).
	let defaultValid = false;
	try {
		defaultValid = await validateAuth(serverUrl, { basic: { user: 'admin', pass: 'admin' } });
	} catch {
		defaultValid = false;
	}

	if (defaultValid) {
		const newPassword = await vscode.window.showInputBox({
			password: true,
			ignoreFocusOut: true,
			prompt: 'New SonarQube admin password',
			placeHolder: 'Choose a strong password for the local admin account',
			validateInput: (v) => (v && v.length >= 8 ? undefined : 'Use at least 8 characters'),
		});
		if (!newPassword) {
			throw new Error('admin password change cancelled');
		}
		if (newPassword === 'admin') {
			throw new Error('new admin password must differ from the default');
		}
		await changeAdminPassword(serverUrl, 'admin', newPassword);
		adminPassword = newPassword;
		await ctx.secrets.store(SECRET_ADMIN_PASSWORD, adminPassword);
		log('admin password changed and stored in SecretStorage');
	} else if (!adminPassword) {
		// Server already secured but we have no stored admin password: ask for it
		// so we can mint a token.
		const existing = await vscode.window.showInputBox({
			password: true,
			ignoreFocusOut: true,
			prompt: 'SonarQube admin password (to generate an access token)',
		});
		if (!existing) {
			throw new Error('admin password required to generate a token');
		}
		const ok = await validateAuth(serverUrl, { basic: { user: 'admin', pass: existing } });
		if (!ok) {
			throw new Error('admin credentials rejected by SonarQube');
		}
		adminPassword = existing;
		await ctx.secrets.store(SECRET_ADMIN_PASSWORD, adminPassword);
	}

	const token = await generateUserToken(serverUrl, adminPassword);
	await ctx.secrets.store(SECRET_USER_TOKEN, token);
	log('user token generated and stored in SecretStorage');
	return token;
}

/** Finalize a connection: persist config + token, flip to connected. */
async function finalizeConnection(serverUrl, token) {
	await writeConnectionConfig(serverUrl);
	await storeSonarLintToken(serverUrl, token);
	setState(State.Connected);
	log('connection configured (connected mode)');
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async function cmdSetupLocalServer() {
	try {
		if (!(await dockerAvailable())) {
			await vscode.window.showErrorMessage(
				'Docker engine not found. Install Docker Desktop (https://www.docker.com/products/docker-desktop) '
				+ 'or Colima (`brew install colima docker && colima start`), then retry.',
				{ modal: true },
			);
			setState(State.Error);
			return;
		}

		setState(State.Starting);
		const cs = await containerStatus();
		if (cs.exists && cs.running) {
			log('reusing running container');
		} else if (cs.exists) {
			log(`starting existing (stopped) container (state=${cs.state})`);
			await startExistingContainer();
		} else {
			const choice = await vscode.window.showInformationMessage(
				`Start a local SonarQube server?\n\nThis runs the "${SONARQUBE_IMAGE}" container `
				+ `"${CONTAINER_NAME}" on 127.0.0.1:9000 with persistent named volumes.`,
				{ modal: true },
				'Start SonarQube', 'Cancel',
			);
			if (choice !== 'Start SonarQube') {
				setState(State.NotConfigured);
				return;
			}
			log('creating new container');
			await createContainer();
		}

		const serverUrl = DEFAULT_SERVER_URL;
		await waitForServerUp(serverUrl);
		const token = await firstRunAuthAndToken(serverUrl);
		await finalizeConnection(serverUrl, token);

		const bind = await vscode.window.showInformationMessage(
			'SonarQube is connected. Bind this workspace to a project now?',
			'Bind Workspace', 'Later',
		);
		if (bind === 'Bind Workspace') {
			await cmdBindWorkspace();
		}
	} catch (err) {
		setState(State.Error);
		await vscode.window.showErrorMessage(`SonarQube setup failed: ${errMessage(err)}`);
	}
}

async function cmdConnectExisting() {
	try {
		const serverUrl = await vscode.window.showInputBox({
			ignoreFocusOut: true,
			prompt: 'SonarQube server URL',
			value: DEFAULT_SERVER_URL,
			validateInput: (v) => (/^https?:\/\//i.test((v || '').trim()) ? undefined : 'Enter an http(s) URL'),
		});
		if (!serverUrl) {
			return;
		}
		const url = serverUrl.trim().replace(/\/+$/, '');

		const token = await vscode.window.showInputBox({
			password: true,
			ignoreFocusOut: true,
			prompt: `SonarQube user token for ${url}`,
		});
		if (!token) {
			return;
		}

		setState(State.Starting);
		const status = await systemStatus(url);
		if (status !== 'UP') {
			throw new Error(status ? `server status is ${status}, expected UP` : 'server not reachable');
		}

		// Token auth: prefer Bearer, fall back to Basic <token>: form.
		let valid = false;
		try {
			valid = await validateAuth(url, { bearer: token });
		} catch {
			valid = false;
		}
		if (!valid) {
			try {
				valid = await validateAuth(url, { basicToken: token });
			} catch {
				valid = false;
			}
		}
		if (!valid) {
			throw new Error('token rejected by SonarQube');
		}

		await ctx.secrets.store(SECRET_USER_TOKEN, token);
		await finalizeConnection(url, token);

		const bind = await vscode.window.showInformationMessage(
			'Connected to SonarQube. Bind this workspace to a project now?',
			'Bind Workspace', 'Later',
		);
		if (bind === 'Bind Workspace') {
			await cmdBindWorkspace();
		}
	} catch (err) {
		setState(State.Error);
		await vscode.window.showErrorMessage(`Connect failed: ${errMessage(err)}`);
	}
}

async function cmdBindWorkspace() {
	try {
		const conn = readConnectionConfig();
		if (!conn) {
			throw new Error('no SonarQube connection configured; run Set Up Local Server or Connect to Existing Server first');
		}
		const serverUrl = conn.serverUrl;
		const groups = await groupFoldersByRepo();
		if (groups.length === 0) {
			throw new Error('no workspace folders open to bind');
		}

		const auth = await resolveApiAuth();
		if (!auth) {
			throw new Error('no stored credentials to query projects; reconnect to SonarQube');
		}

		let projects = [];
		try {
			projects = await searchProjects(serverUrl, auth);
		} catch (e) {
			log(`project search failed: ${errMessage(e)}`);
		}

		/** @type {Record<string, {projectKey: string, connectionId: string}>} */
		const bindings = {};
		const CREATE_NEW = '$(add) Create new project…';

		for (const group of groups) {
			const label = group.folders.map((f) => f.name).join(', ');
			const items = [
				{ label: CREATE_NEW, description: `default key: ${group.defaultKey}` },
				...projects.map((p) => ({ label: p.key, description: p.name })),
			];
			const picked = await vscode.window.showQuickPick(items, {
				ignoreFocusOut: true,
				title: `Bind ${label} to a SonarQube project`,
				placeHolder: 'Select an existing project or create a new one',
			});
			if (!picked) {
				continue; // skip this repo
			}

			let projectKey;
			if (picked.label === CREATE_NEW) {
				const key = await vscode.window.showInputBox({
					ignoreFocusOut: true,
					prompt: `New project key for ${label}`,
					value: group.defaultKey,
					validateInput: (v) => (v && v.trim() ? undefined : 'Project key is required'),
				});
				if (!key) {
					continue;
				}
				projectKey = key.trim();
				await createProject(serverUrl, auth, projectKey, projectKey);
				projects.push({ key: projectKey, name: projectKey });
				log(`created project ${projectKey}`);
			} else {
				projectKey = picked.label;
			}

			// Every folder (worktree) of this repo shares the same binding.
			for (const folder of group.folders) {
				bindings[folder.uri.fsPath] = { projectKey, connectionId: CONNECTION_ID };
			}
		}

		if (Object.keys(bindings).length === 0) {
			await vscode.window.showInformationMessage('No bindings selected.');
			return;
		}

		// Full-replacement map (pinned host contract).
		await vscode.commands.executeCommand('ide.sonarqube.setBindings', bindings);
		setState(State.Connected);
		await vscode.window.showInformationMessage(
			`Bound ${Object.keys(bindings).length} folder(s) across ${groups.length} repo(s) to SonarQube.`,
		);
	} catch (err) {
		await vscode.window.showErrorMessage(`Bind failed: ${errMessage(err)}`);
	}
}

async function cmdOpenDashboard() {
	try {
		const conn = readConnectionConfig();
		const serverUrl = conn ? conn.serverUrl : DEFAULT_SERVER_URL;

		// Prefer a bound project dashboard if this window has a binding.
		let target = serverUrl;
		const folders = vscode.workspace.workspaceFolders || [];
		if (folders.length > 0) {
			const binding = vscode.workspace
				.getConfiguration('sonarlint', folders[0].uri)
				.get('connectedMode.project');
			if (binding && binding.projectKey) {
				target = `${serverUrl.replace(/\/+$/, '')}/dashboard?id=${encodeURIComponent(binding.projectKey)}`;
			}
		}
		await vscode.env.openExternal(vscode.Uri.parse(target));
	} catch (err) {
		await vscode.window.showErrorMessage(`Open dashboard failed: ${errMessage(err)}`);
	}
}

async function cmdStopLocalServer() {
	try {
		const choice = await vscode.window.showInformationMessage(
			`Stop the local SonarQube container "${CONTAINER_NAME}"? Other IDE windows share it.`,
			{ modal: true },
			'Stop SonarQube', 'Cancel',
		);
		if (choice !== 'Stop SonarQube') {
			return;
		}
		if (!(await dockerAvailable())) {
			throw new Error('Docker engine not found');
		}
		await stopContainer();
		setState(State.Stopped);
		await vscode.window.showInformationMessage('SonarQube server stopped.');
	} catch (err) {
		setState(State.Error);
		await vscode.window.showErrorMessage(`Stop failed: ${errMessage(err)}`);
	}
}

async function cmdShowStatus() {
	const hasDocker = await dockerAvailable();
	const cs = hasDocker ? await containerStatus() : { exists: false, running: false, state: 'n/a' };
	const conn = readConnectionConfig();
	const serverUrl = conn ? conn.serverUrl : DEFAULT_SERVER_URL;
	const status = conn ? await systemStatus(serverUrl) : null;

	let bindingCount = 0;
	for (const folder of vscode.workspace.workspaceFolders || []) {
		const b = vscode.workspace.getConfiguration('sonarlint', folder.uri).get('connectedMode.project');
		if (b && b.projectKey) {
			bindingCount += 1;
		}
	}

	const lines = [
		`State: ${currentState}`,
		`Docker engine: ${hasDocker ? 'available' : 'not found'}`,
		`Container "${CONTAINER_NAME}": ${cs.exists ? cs.state || 'present' : 'absent'}`,
		`Server: ${conn ? `${serverUrl} — ${status || 'unreachable'}` : 'no connection configured'}`,
		`Connection configured: ${conn ? 'yes' : 'no'}`,
		`Bound folders in this window: ${bindingCount}`,
	];
	// Never reveal secrets.
	await vscode.window.showInformationMessage(lines.join('\n'), { modal: true });
}

function errMessage(err) {
	if (err && typeof err === 'object' && 'message' in err) {
		return String(err.message);
	}
	return String(err);
}

// ---------------------------------------------------------------------------
// Activation
// ---------------------------------------------------------------------------

async function reconstructState() {
	const conn = readConnectionConfig();
	if (!conn) {
		setState(State.NotConfigured);
		return;
	}
	const status = await systemStatus(conn.serverUrl);
	if (status === 'UP') {
		setState(State.Connected);
	} else if (status) {
		setState(State.Starting);
	} else {
		setState(State.Stopped);
	}
}

function activate(context) {
	ctx = context;
	out = vscode.window.createOutputChannel('SonarQube');
	context.subscriptions.push(out);

	statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
	statusBarItem.command = 'sonarqube.showStatus';
	statusBarItem.tooltip = 'SonarQube setup status';
	context.subscriptions.push(statusBarItem);
	setState(State.NotConfigured);

	const register = (id, fn) => context.subscriptions.push(vscode.commands.registerCommand(id, fn));
	register('sonarqube.setupLocalServer', cmdSetupLocalServer);
	register('sonarqube.connectExisting', cmdConnectExisting);
	register('sonarqube.bindWorkspace', cmdBindWorkspace);
	register('sonarqube.openDashboard', cmdOpenDashboard);
	register('sonarqube.stopLocalServer', cmdStopLocalServer);
	register('sonarqube.showStatus', cmdShowStatus);

	// Reconstruct state from persisted connection config + a status probe.
	reconstructState().catch(() => setState(State.NotConfigured));
}

function deactivate() {
	// Status bar + output channel are disposed via context.subscriptions.
}

exports.activate = activate;
exports.deactivate = deactivate;
