// License audit for ide-exthost's installed dependency tree.
//
//   node scripts/license-audit.mjs
//
// Walks node_modules, reads each package's license field, and fails (exit 1) if
// any package's license is not clearly in the permissive allowlist (including
// UNKNOWN/missing or copyleft/source-available licenses).

import * as fs from 'node:fs/promises';
import { existsSync } from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const PKG_ROOT = path.resolve(__dirname, '..');
const NODE_MODULES = path.join(PKG_ROOT, 'node_modules');

// Permissive SPDX identifiers we accept.
const ALLOWLIST = new Set([
  'MIT',
  'Apache-2.0',
  'BSD-2-Clause',
  'BSD-3-Clause',
  'ISC',
  'MPL-2.0',
  'Zlib',
  'Unicode-DFS-2016',
  'Unicode-3.0',
  'CC0-1.0',
  '0BSD',
  'BlueOak-1.0.0',
  'Python-2.0',
  'WTFPL',
]);

// Packages whose published SPDX is nonstandard/missing but are actually
// permissive. Keyed by "name" or "name@version". Fill in only encountered ones.
const KNOWN_OVERRIDES = {
  // e.g. 'some-pkg': 'MIT',
};

// External, runtime-downloaded dependencies that are deliberately OUTSIDE this
// npm audit because they are never bundled into our repo or binaries: we fetch
// them at runtime (Open VSX / Docker Hub) and ship/patch nothing. They are
// recorded here (and in NOTICE / docs/sonarqube.md) so the license posture is
// explicit. Per the licensing decision, SonarQube Community Edition and the
// SonarQube for IDE (SonarLint) extension are allowed ONLY as external,
// unmodified dependencies.
const EXTERNAL_RUNTIME_DEPENDENCIES = [
  {
    name: 'SonarSource.sonarlint-vscode (SonarQube for IDE)',
    license: 'LGPL-3.0',
    source: 'Open VSX (open-vsx.org), downloaded at runtime into the extensions dir',
    note: 'Unmodified VS Code extension, loaded by the shared host like any other extension. Not vendored, bundled, or patched.',
  },
  {
    name: 'sonarqube:community (SonarQube Community Edition server)',
    license: 'LGPL-3.0',
    source: 'Docker Hub, pulled at runtime; runs as an external container (ide-sonarqube)',
    note: 'External server process, never linked into or shipped with ide.',
  },
];

// Normalize a package.json license/licenses field into a string label.
function readLicense(pkg) {
  if (typeof pkg.license === 'string') return pkg.license;
  if (pkg.license && typeof pkg.license === 'object' && pkg.license.type) return pkg.license.type;
  if (Array.isArray(pkg.licenses)) {
    const types = pkg.licenses.map((l) => (typeof l === 'string' ? l : l && l.type)).filter(Boolean);
    if (types.length) return types.join(' OR ');
  }
  if (pkg.licenses && typeof pkg.licenses === 'object' && pkg.licenses.type) return pkg.licenses.type;
  return 'UNKNOWN';
}

// Strip surrounding parentheses and split an SPDX expression into atoms.
function spdxAtoms(expr) {
  return expr
    .replace(/[()]/g, ' ')
    .split(/\s+(?:OR|AND|WITH)\s+/i)
    .map((s) => s.trim())
    .filter(Boolean);
}

// An expression is permissive if it is a disjunction (OR) with at least one
// permissive atom, or a pure conjunction whose every atom is permissive.
function isPermissive(expr) {
  if (!expr || expr === 'UNKNOWN') return false;
  const atoms = spdxAtoms(expr);
  if (!atoms.length) return false;
  const hasOr = /\bOR\b/i.test(expr);
  const allowed = atoms.map((a) => ALLOWLIST.has(a));
  return hasOr ? allowed.some(Boolean) : allowed.every(Boolean);
}

// Recursively collect every package.json under a node_modules dir, including
// scoped packages (@scope/name) and nested node_modules.
async function collect(nmDir, out) {
  if (!existsSync(nmDir)) return;
  let entries;
  try {
    entries = await fs.readdir(nmDir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const ent of entries) {
    if (ent.name === '.bin' || ent.name === '.cache' || ent.name.startsWith('.')) continue;
    const full = path.join(nmDir, ent.name);
    if (!ent.isDirectory() && !ent.isSymbolicLink()) continue;
    if (ent.name.startsWith('@')) {
      // scope dir -> recurse one level into the scoped packages
      let scoped;
      try {
        scoped = await fs.readdir(full, { withFileTypes: true });
      } catch {
        continue;
      }
      for (const s of scoped) {
        if (!s.isDirectory() && !s.isSymbolicLink()) continue;
        await recordPkg(path.join(full, s.name), out);
      }
    } else {
      await recordPkg(full, out);
    }
  }
}

async function recordPkg(pkgDir, out) {
  const pjPath = path.join(pkgDir, 'package.json');
  if (existsSync(pjPath)) {
    try {
      const pkg = JSON.parse(await fs.readFile(pjPath, 'utf8'));
      if (pkg.name && pkg.version) {
        const key = `${pkg.name}@${pkg.version}`;
        if (!out.has(key)) {
          let license = readLicense(pkg);
          const override = KNOWN_OVERRIDES[pkg.name] || KNOWN_OVERRIDES[key];
          if ((license === 'UNKNOWN' || !isPermissive(license)) && override) license = override;
          out.set(key, { name: pkg.name, version: pkg.version, license });
        }
      }
    } catch {
      /* ignore unreadable package.json */
    }
  }
  // Recurse into nested node_modules.
  await collect(path.join(pkgDir, 'node_modules'), out);
}

async function main() {
  const out = new Map();
  await collect(NODE_MODULES, out);

  const all = [...out.values()].sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
  const offenders = all.filter((p) => !isPermissive(p.license));

  const nameW = Math.max(4, ...all.map((p) => `${p.name}@${p.version}`.length));
  process.stdout.write('License audit for ide-exthost\n');
  process.stdout.write('='.repeat(nameW + 20) + '\n');
  for (const p of all) {
    const id = `${p.name}@${p.version}`;
    const mark = isPermissive(p.license) ? ' ' : 'X';
    process.stdout.write(`${mark} ${id.padEnd(nameW)}  ${p.license}\n`);
  }
  process.stdout.write('-'.repeat(nameW + 20) + '\n');
  process.stdout.write(`total: ${all.length} package(s); flagged: ${offenders.length}\n`);

  process.stdout.write('\nExternal runtime-downloaded dependencies (NOT bundled; out of npm-audit scope):\n');
  for (const d of EXTERNAL_RUNTIME_DEPENDENCIES) {
    process.stdout.write(`  ${d.name} — ${d.license} — ${d.source}\n`);
  }

  if (offenders.length) {
    process.stdout.write('\nNON-PERMISSIVE / UNKNOWN:\n');
    for (const p of offenders) {
      process.stdout.write(`  ${p.name}@${p.version} — ${p.license}\n`);
    }
    process.exit(1);
  }
  process.stdout.write('All dependencies are permissively licensed.\n');
  process.exit(0);
}

if (import.meta.url === pathToFileURL(process.argv[1] || '').href) {
  await main();
}

export { isPermissive, readLicense, ALLOWLIST, KNOWN_OVERRIDES };
