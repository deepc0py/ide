// Bundles the ide shared extension host from VS Code (MIT) source in ./.vscode-src.
//  - dist/host.js    : control-socket server + worker manager (entry point)
//  - dist/worker.js  : per-workspace worker (VS Code ExtensionHostMain + main-thread shim + LSP bridge)
import { build } from 'esbuild';
import { existsSync, cpSync, rmSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(fileURLToPath(import.meta.url));
const vscodeSrc = resolve(root, '.vscode-src');
if (!existsSync(vscodeSrc)) {
	console.error('Missing .vscode-src; run scripts/fetch-vscode.sh first.');
	process.exit(1);
}

const STUB = resolve(root, 'src/stubs/native.cjs');

// Native / Electron-only / telemetry / copyleft modules that the extension host
// never needs for the LSP-bridge feature set. They are redirected to a loud stub
// so that (a) nothing must be resolved from node_modules at runtime and (b) any
// accidental use throws a clear error instead of hanging.
//  - jschardet is LGPL-2.1+ (copyleft) => stubbed, not shipped (encoding falls back to utf8).
//  - @microsoft/1ds-*, tas-client-umd are telemetry => dropped.
const NATIVE = [
	'electron', '@vscode/native-watchdog', '@vscode/spdlog', '@vscode/deviceid',
	'@vscode/windows-process-tree', 'native-is-elevated', '@vscode/windows-registry',
	'node-pty', '@vscode/policy-watcher', '@vscode/windows-mutex', '@parcel/watcher',
	'@vscode/vscode-languagedetection', 'kerberos', '@vscode/sqlite3', 'vsda',
	'@vscode/ripgrep-universal', '@vscode/tree-sitter-wasm', 'undici', '@vscode/windows-ca-certs',
	'jschardet', 'tas-client-umd', '@microsoft/1ds-core-js', '@microsoft/1ds-post-js',
];

const resolver = {
	name: 'ide-resolver',
	setup(b) {
		const native = new Set(NATIVE);
		// Redirect native/unused bare modules to the stub.
		b.onResolve({ filter: /^[^.]/ }, (args) => {
			if (native.has(args.path)) { return { path: STUB }; }
			return undefined;
		});
		// VS Code imports use explicit `.js` suffixes but files on disk are `.ts`.
		b.onResolve({ filter: /\.js$/ }, (args) => {
			if (args.kind === 'entry-point') { return undefined; }
			if (!args.path.startsWith('.')) { return undefined; }
			const abs = resolve(args.resolveDir, args.path);
			const ts = abs.replace(/\.js$/, '.ts');
			if (existsSync(ts)) { return { path: ts }; }
			if (existsSync(abs)) { return { path: abs }; }
			return undefined;
		});
		// Stub `.css` asset imports pulled in by a few shared files.
		b.onResolve({ filter: /\.css$/ }, () => ({ path: 'ide-empty-css', namespace: 'ide-stub' }));
		b.onLoad({ filter: /.*/, namespace: 'ide-stub' }, () => ({ contents: '', loader: 'js' }));
	},
};

const common = {
	bundle: true,
	// VS Code registers DI singletons via top-level side-effect calls; esbuild's
	// tree-shaker drops them, so disable it (correctness over bundle size).
	treeShaking: false,
	platform: 'node',
	format: 'esm',
	target: 'node22',
	sourcemap: true,
	logLevel: 'warning',
	logLimit: 0,
	legalComments: 'none',
	// VS Code DI relies on TS experimental (legacy) decorators.
	tsconfigRaw: {
		compilerOptions: {
			experimentalDecorators: true,
			useDefineForClassFields: false,
		},
	},
	define: {
		'process.env.VSCODE_HANDLES_UNCAUGHT_ERRORS': '"true"',
	},
	banner: {
		js: "import { createRequire as __ideCreateRequire } from 'node:module'; const require = __ideCreateRequire(import.meta.url); import { fileURLToPath as __ideFileURLToPath } from 'node:url'; import { dirname as __ideDirname } from 'node:path'; const __filename = __ideFileURLToPath(import.meta.url); const __dirname = __ideDirname(__filename);",
	},
	plugins: [resolver],
};

const targets = [
	{ in: resolve(root, 'src/worker.ts'), out: resolve(root, 'dist/worker.js') },
	{ in: resolve(root, 'src/host.ts'), out: resolve(root, 'dist/host.js') },
];

for (const t of targets) {
	await build({ ...common, entryPoints: [t.in], outfile: t.out });
}

// Ship the built-in extensions (OUR MIT code, e.g. the SonarQube assistant)
// next to host.js so a packaged host (host.js not under the repo) still finds
// them via `<host.js dir>/builtin/*`. In the repo they are also found via
// `<appRoot>/builtin`.
const builtinSrc = resolve(root, 'builtin');
if (existsSync(builtinSrc)) {
	const builtinDest = resolve(root, 'dist/builtin');
	rmSync(builtinDest, { recursive: true, force: true });
	cpSync(builtinSrc, builtinDest, { recursive: true });
}
console.log('build complete');
