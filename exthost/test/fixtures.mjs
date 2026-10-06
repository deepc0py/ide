// Build a deterministic multi-folder sample project tree used by the exthost
// test harness to exercise ESLint, Prettier, rust-analyzer, Pylance and GitLens.
//
//   import { makeFixtures } from './fixtures.mjs';
//   const { root, files } = await makeFixtures();   // wipes + recreates root
//
// All positions recorded in `files` are 0-based (LSP convention).

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { execFileSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';

const uri = (p) => pathToFileURL(p).href;

async function writeFile(p, content) {
  await fs.mkdir(path.dirname(p), { recursive: true });
  await fs.writeFile(p, content, 'utf8');
  return p;
}

// ---- TypeScript project files -------------------------------------------------

const TS_PACKAGE_JSON = JSON.stringify(
  {
    name: 'tsproj',
    version: '0.0.0',
    private: true,
    type: 'module',
    devDependencies: { eslint: '^9.0.0', typescript: '^5.0.0', prettier: '^3.0.0' },
  },
  null,
  2,
) + '\n';

// eslint v9 flat config.
const TS_ESLINT_FLAT = `import js from '@eslint/js';

export default [
  js.configs.recommended,
  {
    files: ['**/*.ts'],
    languageOptions: { ecmaVersion: 2022, sourceType: 'module' },
    rules: {
      'no-unused-vars': 'error',
      'no-debugger': 'error',
    },
  },
];
`;

// Legacy eslintrc so an older ESLint resolution path also finds the same rules.
const TS_ESLINTRC = JSON.stringify(
  {
    root: true,
    parserOptions: { ecmaVersion: 2022, sourceType: 'module' },
    env: { es2022: true, node: true },
    rules: { 'no-unused-vars': 'error', 'no-debugger': 'error' },
  },
  null,
  2,
) + '\n';

// Intentionally contains an unused-var violation, a debugger statement, and bad
// formatting (extra spaces, no final newline) so Prettier produces edits.
// NOTE: no trailing newline on purpose.
const TS_INDEX = `const x = 1;\nconst    y=1 ;\ndebugger;\nexport function run(){return x;}`;

const TS_PRETTIERRC = JSON.stringify({ semi: true, singleQuote: false }, null, 2) + '\n';

const TS_TSCONFIG = JSON.stringify(
  {
    compilerOptions: {
      target: 'ES2022',
      module: 'ESNext',
      moduleResolution: 'Bundler',
      strict: true,
      noEmit: true,
    },
    include: ['src'],
  },
  null,
  2,
) + '\n';

// ---- Rust project -------------------------------------------------------------

const RUST_CARGO = `[package]
name = "rustproj"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "rustproj"
path = "src/main.rs"
`;

// Cross-file (cross-module) go-to-definition fixture. `add` is defined in
// `src/math.rs`; `main.rs` calls `math::add`, so resolving the call site requires
// rust-analyzer to have loaded the crate graph (not just syntax analysis).
const RUST_MATH = `pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
`;
// Line 3 "    let r = math::add(1, 2);" — the `add` token starts at char 18.
const RUST_MAIN = `mod math;

fn main() {
    let r = math::add(1, 2);
    println!("{}", r);
}
`;

// ---- Python project -----------------------------------------------------------

const PY_MAIN = `def greet(name):
    return 'hi ' + name

print(greet('world'))
`;

// ---- Git project --------------------------------------------------------------

const GIT_TRACKED = `first tracked line
second tracked line
third tracked line
`;

/**
 * Wipe + recreate `root` and build the sample project tree.
 * @returns {Promise<{root:string, files:object}>}
 */
export async function makeFixtures(root = '/tmp/ide-exthost-fixtures') {
  root = path.resolve(root);
  await fs.rm(root, { recursive: true, force: true });
  await fs.mkdir(root, { recursive: true });

  // TypeScript project.
  const tsProj = path.join(root, 'tsproj');
  const tsPackageJson = await writeFile(path.join(tsProj, 'package.json'), TS_PACKAGE_JSON);
  const tsEslintFlat = await writeFile(path.join(tsProj, 'eslint.config.mjs'), TS_ESLINT_FLAT);
  const tsEslintrc = await writeFile(path.join(tsProj, '.eslintrc.json'), TS_ESLINTRC);
  const tsPrettierrc = await writeFile(path.join(tsProj, '.prettierrc'), TS_PRETTIERRC);
  const tsconfig = await writeFile(path.join(tsProj, 'tsconfig.json'), TS_TSCONFIG);
  const tsIndex = await writeFile(path.join(tsProj, 'src', 'index.ts'), TS_INDEX);

  // Rust cargo project.
  const rustProj = path.join(root, 'rustproj');
  const rustCargoToml = await writeFile(path.join(rustProj, 'Cargo.toml'), RUST_CARGO);
  const rustMath = await writeFile(path.join(rustProj, 'src', 'math.rs'), RUST_MATH);
  const rustMain = await writeFile(path.join(rustProj, 'src', 'main.rs'), RUST_MAIN);

  // Python project.
  const pyMain = await writeFile(path.join(root, 'pyproj', 'main.py'), PY_MAIN);

  // Git project: committed file with a real author on a fixed date (deterministic).
  const gitDir = path.join(root, 'gitproj');
  const gitTracked = await writeFile(path.join(gitDir, 'tracked.txt'), GIT_TRACKED);
  const git = (args) =>
    execFileSync('git', ['-C', root, ...args], {
      stdio: 'pipe',
      env: {
        ...process.env,
        GIT_AUTHOR_DATE: '2024-01-01T00:00:00Z',
        GIT_COMMITTER_DATE: '2024-01-01T00:00:00Z',
      },
    });
  git(['init', '-q']);
  git(['-c', 'user.email=fixtures@ide.test', '-c', 'user.name=IDE Fixtures', 'add', 'gitproj/tracked.txt']);
  git([
    '-c',
    'user.email=fixtures@ide.test',
    '-c',
    'user.name=IDE Fixtures',
    'commit',
    '-q',
    '-m',
    'initial commit',
  ]);

  const files = {
    // TypeScript
    tsProj,
    tsPackageJson,
    tsEslintFlat,
    tsEslintrc,
    tsPrettierrc,
    tsconfig,
    tsIndex,
    tsIndexUri: uri(tsIndex),
    // Rust
    rustProj,
    rustCargoToml,
    rustMath,
    rustMathUri: uri(rustMath),
    rustMain,
    rustMainUri: uri(rustMain),
    // `add` call token on line 3 ("    let r = math::add(1, 2);") starts at char 18.
    rustCallSite: { uri: uri(rustMain), line: 3, character: 18 },
    // The definition lives in src/math.rs, on line 0 (the `pub fn add` identifier).
    rustExpectedDefinitionUri: uri(rustMath),
    rustExpectedDefinitionLine: 0,
    // Python
    pyMain,
    pyMainUri: uri(pyMain),
    // `greet` call token on line 3 ("print(greet('world'))") starts at char 6.
    pyHoverSite: { uri: uri(pyMain), line: 3, character: 6 },
    // Python `def greet` definition is on line 0.
    pyExpectedDefinitionLine: 0,
    // Git
    gitRoot: root,
    gitDir,
    gitTracked,
    gitTrackedUri: uri(gitTracked),
    // Second committed line (0-based line 1) for a blame lookup.
    gitBlameSite: { uri: uri(gitTracked), line: 1, character: 0 },
  };

  return { root, files };
}

// Allow quick manual inspection.
if (import.meta.url === pathToFileURL(process.argv[1] || '').href) {
  const r = await makeFixtures();
  process.stdout.write(JSON.stringify(r, null, 2) + '\n');
}
