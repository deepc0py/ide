// Prepare the shared-extension-host fixtures for the Rust integration test
// (lapce-proxy/tests/exthost_integration.rs) and print a JSON manifest to
// stdout. Unlike exthost/test/exthost.test.mjs this does NOT boot the host —
// the Rust proxy spawns it through the same code path the IDE windows use.
//
// Reuses extensions.mjs (download the 6 required extensions into a /tmp cache)
// and fixtures.mjs (build the deterministic sample tree + git repo). Idempotent:
// extensions + eslint install are cached; the fixture tree is rebuilt cheaply.
//
//   node exthost/test/prepare-fixtures.mjs   # prints manifest JSON
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
	existsSync,
	mkdirSync,
	writeFileSync,
	chmodSync,
	rmSync,
} from 'node:fs';
import { execFileSync } from 'node:child_process';
import { ensureExtensions } from './extensions.mjs';
import { makeFixtures } from './fixtures.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const HOST = path.join(here, '..', 'dist', 'host.js');

const ROOT = '/tmp/ide-rust-itest';
const FIXTURES = path.join(ROOT, 'fixtures');
const DATA_DIR = path.join(ROOT, 'exthost-data');
const HOME_DIR = path.join(ROOT, 'home');

function installEslint(tsProj) {
	if (existsSync(path.join(tsProj, 'node_modules', 'eslint'))) {
		return;
	}
	execFileSync(
		'npm',
		['install', '--no-save', '--no-audit', '--no-fund', 'eslint@9', '@eslint/js@9'],
		{ cwd: tsProj, stdio: 'inherit', timeout: 300000 },
	);
}

function chmodRustAnalyzer(dir) {
	for (const rel of ['server/rust-analyzer', 'server/rust-analyzer.exe', 'bin/rust-analyzer']) {
		const p = path.join(dir, rel);
		if (existsSync(p)) {
			try {
				chmodSync(p, 0o755);
			} catch {
				/* ignore */
			}
		}
	}
}

async function main() {
	if (!existsSync(HOST)) {
		throw new Error(`exthost dist missing: ${HOST} (run: cd exthost && npm ci && npm run build)`);
	}

	const extMap = await ensureExtensions();
	for (const dir of Object.values(extMap)) {
		chmodRustAnalyzer(dir);
	}
	const extensionsDir = path.dirname(Object.values(extMap)[0]);

	const { root, files } = await makeFixtures(FIXTURES);
	installEslint(files.tsProj);

	rmSync(DATA_DIR, { recursive: true, force: true });
	mkdirSync(path.join(DATA_DIR, 'User'), { recursive: true });
	mkdirSync(HOME_DIR, { recursive: true });
	writeFileSync(
		path.join(DATA_DIR, 'User', 'settings.json'),
		JSON.stringify(
			{
				'telemetry.telemetryLevel': 'off',
				'python.languageServer': 'Jedi',
				'python.experiments.enabled': false,
				'rust-analyzer.checkOnSave': false,
				'rust-analyzer.cargo.buildScripts.enable': false,
				'rust-analyzer.procMacro.enable': false,
				'gitlens.currentLine.enabled': true,
				'gitlens.statusBar.enabled': true,
				'gitlens.codeLens.enabled': false,
				'eslint.useFlatConfig': true,
				'eslint.run': 'onType',
			},
			null,
			2,
		),
	);

	const manifest = {
		hostJs: HOST,
		extensionsDir,
		dataDir: DATA_DIR,
		homeDir: HOME_DIR,
		// The git repo root — opened as the single window folder.
		root,
		tsIndex: files.tsIndex,
		tsIndexUri: files.tsIndexUri,
		gitTracked: files.gitTracked,
		gitTrackedUri: files.gitTrackedUri,
	};
	process.stdout.write(JSON.stringify(manifest) + '\n');
}

main().catch((err) => {
	console.error(err);
	process.exit(1);
});
