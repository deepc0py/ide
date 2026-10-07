// Regenerate `ide-webview/src/theme_data.rs` — the VS Code webview `--vscode-*`
// color + size variable tables for the bundled "Dark Modern" / "Light Modern"
// themes.
//
// This reproduces exactly what VS Code's webview theming does
// (`vs/workbench/contrib/webview/browser/themeing.ts`): for every registered
// color, `theme.getColor(id)` (= the theme's override, else the registry's
// default resolved through transparent()/darken()/oneOf()/… transforms), keyed
// as `--vscode-<id with '.'→'-'>`; likewise for the size registry.
//
// Colors/sizes are extracted statically from the vendored VS Code sources
// (`exthost/.vscode-src`) by parsing every `registerColor(...)` call and
// evaluating its defaults expression with the real transform helpers + Color
// class (bundled via esbuild). Colors VS Code registers outside that tree (the
// built-in git extension's `gitDecoration.*` and the terminal ansi palette,
// registered via a template-id loop the static scan skips) are supplemented
// from their known defaults.
//
// Usage (from the repo root `ide/`):
//   node ide-webview/tools/gen_theme_vars.mjs
//
// Requires: node >= 20, `exthost/node_modules/esbuild`, and the vendored VS
// Code sources under `exthost/.vscode-src` (run `cd exthost && npm run fetch`).

import { existsSync, readFileSync, writeFileSync, readdirSync, statSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import path from 'node:path';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const REPO = path.resolve(HERE, '../..'); // ide/
const VSRC = path.join(REPO, 'exthost/.vscode-src/src');
const VS = path.join(VSRC, 'vs');
const THEMES = path.join(REPO, 'defaults/vscode-themes');
const ESBUILD = path.join(REPO, 'exthost/node_modules/esbuild/lib/main.js');
const OUT = path.join(REPO, 'ide-webview/src/theme_data.rs');
const TMP = path.join(REPO, 'exthost/.vscode-src/.theme-gen-bundle.cjs');

if (!existsSync(VS)) {
  console.error(`vendored VS Code sources missing at ${VS}; run \`cd exthost && npm run fetch\``);
  process.exit(1);
}

// --- bundle the pure color/size helpers + Color from the vendored sources ----
const { build } = await import(pathToFileURL(ESBUILD).href);
const tsResolve = {
  name: 'ts-resolve',
  setup(b) {
    b.onResolve({ filter: /\.js$/ }, args => {
      const p = args.path.startsWith('/') ? args.path : path.resolve(path.dirname(args.importer), args.path);
      const ts = p.replace(/\.js$/, '.ts');
      if (existsSync(ts)) return { path: ts };
      if (existsSync(p)) return { path: p };
      return undefined;
    });
  },
};
const entry = `
import { darken, lighten, transparent, opaque, oneOf, ifDefinedThenElse, lessProminent, resolveColorValue, isColorDefaults } from '${VS}/platform/theme/common/colorUtils.js';
import { ColorScheme } from '${VS}/platform/theme/common/theme.js';
import { Color, RGBA, HSLA } from '${VS}/base/common/color.js';
import { getSizeRegistry, sizeValueToCss } from '${VS}/platform/theme/common/sizeUtils.js';
import '${VS}/platform/theme/common/sizeRegistry.js';
globalThis.__helpers = { darken, lighten, transparent, opaque, oneOf, ifDefinedThenElse, lessProminent, resolveColorValue, isColorDefaults, ColorScheme, Color, RGBA, HSLA, getSizeRegistry, sizeValueToCss };
`;
await build({
  stdin: { contents: entry, resolveDir: VSRC, loader: 'ts' },
  bundle: true, platform: 'node', format: 'cjs', outfile: TMP, logLevel: 'warning',
  plugins: [tsResolve], loader: { '.ts': 'ts' },
});
await import(pathToFileURL(TMP).href + '?t=' + Date.now());
const H = globalThis.__helpers;
const { resolveColorValue, isColorDefaults, ColorScheme, Color } = H;

// --- static extraction of every registerColor(...) call ----------------------
function walk(dir, acc = []) {
  for (const name of readdirSync(dir)) {
    if (name === 'test' || name === 'node_modules') continue;
    const p = path.join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) walk(p, acc);
    else if (name.endsWith('.ts') && !name.endsWith('.d.ts') && !name.endsWith('.test.ts')) acc.push(p);
  }
  return acc;
}
function scanArgList(src, open) {
  let i = open, depth = 0, inStr = null;
  for (; i < src.length; i++) {
    const c = src[i];
    if (inStr) { if (c === '\\') { i++; continue; } if (c === inStr) inStr = null; continue; }
    if (c === '"' || c === "'" || c === '`') { inStr = c; continue; }
    if (c === '/' && src[i + 1] === '/') { while (i < src.length && src[i] !== '\n') i++; continue; }
    if (c === '/' && src[i + 1] === '*') { i += 2; while (i + 1 < src.length && !(src[i] === '*' && src[i + 1] === '/')) i++; i++; continue; }
    if (c === '(') depth++;
    else if (c === ')') { depth--; if (depth === 0) break; }
  }
  return { argsSrc: src.slice(open + 1, i), end: i };
}
function splitArgs(s) {
  const args = []; let depth = 0, inStr = null, start = 0;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (inStr) { if (c === '\\') { i++; continue; } if (c === inStr) inStr = null; continue; }
    if (c === '"' || c === "'" || c === '`') { inStr = c; continue; }
    if (c === '/' && s[i + 1] === '/') { while (i < s.length && s[i] !== '\n') i++; continue; }
    if (c === '/' && s[i + 1] === '*') { i += 2; while (i + 1 < s.length && !(s[i] === '*' && s[i + 1] === '/')) i++; i++; continue; }
    if (c === '(' || c === '[' || c === '{') depth++;
    else if (c === ')' || c === ']' || c === '}') depth--;
    else if (c === ',' && depth === 0) { args.push(s.slice(start, i)); start = i + 1; }
  }
  args.push(s.slice(start));
  return args.map(a => a.trim());
}
function litString(s) {
  s = s.trim();
  if ((s[0] === "'" || s[0] === '"' || s[0] === '`') && s[s.length - 1] === s[0]) return s.slice(1, -1);
  return null;
}
const rawCalls = [];
for (const f of walk(VS)) {
  let src;
  try { src = readFileSync(f, 'utf8'); } catch { continue; }
  if (!src.includes('registerColor(')) continue;
  const re = /registerColor\s*\(/g;
  let m;
  while ((m = re.exec(src))) {
    const open = m.index + m[0].length - 1;
    const { argsSrc } = scanArgList(src, open);
    const args = splitArgs(argsSrc);
    if (args.length < 2) continue;
    const id = litString(args[0]);
    if (!id) continue;
    const before = src.slice(Math.max(0, m.index - 80), m.index);
    const am = before.match(/const\s+([A-Za-z0-9_$]+)\s*=\s*$/);
    rawCalls.push({ id, defaultsSrc: args[1], name: am ? am[1] : null });
  }
}
const nameToId = {};
for (const c of rawCalls) if (c.name) nameToId[c.name] = c.id;

const bindings = {
  Color: H.Color, RGBA: H.RGBA, HSLA: H.HSLA,
  darken: H.darken, lighten: H.lighten, transparent: H.transparent, opaque: H.opaque,
  oneOf: H.oneOf, ifDefinedThenElse: H.ifDefinedThenElse, lessProminent: H.lessProminent,
};
for (const [n, id] of Object.entries(nameToId)) if (!(n in bindings)) bindings[n] = id;
function evalDefaults(expr) {
  const scope = new Proxy(bindings, { has: () => true, get: (t, k) => (typeof k === 'string' && k in t ? t[k] : undefined) });
  return new Function('__scope', 'with(__scope){ return (' + expr + '); }')(scope);
}
const registry = {};
for (const c of rawCalls) {
  let d;
  try { d = evalDefaults(c.defaultsSrc); } catch { d = undefined; }
  if (!(c.id in registry) || d !== undefined) registry[c.id] = d;
}

// Colors VS Code registers outside the vendored workbench src.
const SUPPLEMENT = {
  'gitDecoration.addedResourceForeground': { dark: '#81b88b', light: '#587c0c' },
  'gitDecoration.modifiedResourceForeground': { dark: '#e2c08d', light: '#895503' },
  'gitDecoration.deletedResourceForeground': { dark: '#c74e39', light: '#ad0707' },
  'gitDecoration.renamedResourceForeground': { dark: '#73c991', light: '#007100' },
  'gitDecoration.untrackedResourceForeground': { dark: '#73c991', light: '#007100' },
  'gitDecoration.ignoredResourceForeground': { dark: '#8c8c8c', light: '#8e8e90' },
  'gitDecoration.conflictingResourceForeground': { dark: '#e4676b', light: '#ad0707' },
  'gitDecoration.stageModifiedResourceForeground': { dark: '#e2c08d', light: '#895503' },
  'gitDecoration.stageDeletedResourceForeground': { dark: '#c74e39', light: '#ad0707' },
  'gitDecoration.submoduleResourceForeground': { dark: '#8db9e2', light: '#1258a7' },
  'terminal.ansiBlack': { dark: '#000000', light: '#000000' },
  'terminal.ansiRed': { dark: '#cd3131', light: '#cd3131' },
  'terminal.ansiGreen': { dark: '#0dbc79', light: '#00bc00' },
  'terminal.ansiYellow': { dark: '#e5e510', light: '#949800' },
  'terminal.ansiBlue': { dark: '#2472c8', light: '#0451a5' },
  'terminal.ansiMagenta': { dark: '#bc3fbc', light: '#bc05bc' },
  'terminal.ansiCyan': { dark: '#11a8cd', light: '#0598bc' },
  'terminal.ansiWhite': { dark: '#e5e5e5', light: '#555555' },
};
for (const [id, d] of Object.entries(SUPPLEMENT)) if (!(id in registry) || registry[id] == null) registry[id] = d;

// --- theme merge + resolve ---------------------------------------------------
function stripJsonc(s) {
  let o = '', i = 0, n = s.length, inStr = false, esc = false;
  while (i < n) {
    const c = s[i];
    if (inStr) { o += c; if (esc) esc = false; else if (c === '\\') esc = true; else if (c === '"') inStr = false; i++; continue; }
    if (c === '"') { inStr = true; o += c; i++; continue; }
    if (c === '/' && s[i + 1] === '/') { while (i < n && s[i] !== '\n') i++; continue; }
    if (c === '/' && s[i + 1] === '*') { i += 2; while (i + 1 < n && !(s[i] === '*' && s[i + 1] === '/')) i++; i += 2; continue; }
    o += c; i++;
  }
  return o.replace(/,(\s*[}\]])/g, '$1');
}
function mergedColors(files) {
  const mm = {};
  for (const f of files) Object.assign(mm, JSON.parse(stripJsonc(readFileSync(path.join(THEMES, f), 'utf8'))).colors || {});
  return mm;
}
const DARK = mergedColors(['dark_vs.json', 'dark_plus.json', 'dark_modern.json']);
const LIGHT = mergedColors(['light_vs.json', 'light_plus.json', 'light_modern.json']);

function resolveDefault(id, theme) {
  const d = registry[id];
  if (d === undefined || d === null) return undefined;
  const cv = isColorDefaults(d) ? d[theme.type] : d;
  return resolveColorValue(cv ?? null, theme);
}
function makeTheme(type, overrides) {
  const ov = new Map();
  for (const [id, hex] of Object.entries(overrides)) {
    if (typeof hex === 'string' && hex[0] === '#') { const c = Color.fromHex(hex); if (c) ov.set(id, c); }
  }
  const theme = {
    type,
    getColor(id) { return ov.has(id) ? ov.get(id) : resolveDefault(id, theme); },
    defines(id) { return ov.has(id) || (id in registry && registry[id] != null); },
  };
  return theme;
}
const sizeReg = H.getSizeRegistry();
function exportFor(type, overrides) {
  const theme = makeTheme(type, overrides);
  const map = {};
  for (const id of new Set([...Object.keys(registry), ...Object.keys(overrides)])) {
    const c = theme.getColor(id);
    if (c) map['--vscode-' + id.replace(/\./g, '-')] = c.toString();
  }
  for (const entry of sizeReg.getSizes()) {
    const v = sizeReg.resolveDefaultSize(entry.id, theme);
    if (v) map['--vscode-' + entry.id.replace(/\./g, '-')] = H.sizeValueToCss(v);
  }
  // sash.{size,hover-size} come from the workbench sash.css :root (not the size
  // registry); webview CSS references them, VS Code's defaults are 4px.
  map['--vscode-sash-size'] = '4px';
  map['--vscode-sash-hover-size'] = '4px';
  return map;
}
const dark = exportFor(ColorScheme.DARK, DARK);
const light = exportFor(ColorScheme.LIGHT, LIGHT);

// --- emit Rust ---------------------------------------------------------------
const esc = s => s.replace(/\\/g, '\\\\').replace(/"/g, '\\"');
function table(name, d) {
  const rows = Object.keys(d).sort().map(k => `    ("${esc(k)}", "${esc(d[k])}"),`);
  return `pub static ${name}: &[(&str, &str)] = &[\n${rows.join('\n')}\n];`;
}
const header = `//! VS Code webview theme variable tables — GENERATED, do not edit by hand.
//!
//! Each entry is a CSS custom property (full name, incl. leading \`--\`) mapped to
//! the value VS Code's webview theming injects for the corresponding default
//! color theme. Produced by resolving VS Code's own color + size registries
//! (vendored under \`exthost/.vscode-src\`) against the bundled "Dark Modern" /
//! "Light Modern" themes (\`defaults/vscode-themes/*.json\`), exactly as
//! \`vs/workbench/contrib/webview/browser/themeing.ts\` does
//! (\`theme.getColor(id)\` for every registered color, \`id.replace('.', '-')\`),
//! plus the built-in git extension's \`gitDecoration.*\` colors and the terminal
//! ansi palette. Regenerate with \`ide-webview/tools/gen_theme_vars.mjs\`.
//!
//! Font / layout variables that VS Code derives at runtime (editor font family,
//! size, …) are NOT here — they are added by [\`crate::theme\`].

`;
writeFileSync(OUT, header + table('DARK', dark) + '\n\n' + table('LIGHT', light) + '\n');
try { (await import('node:fs')).unlinkSync(TMP); } catch { /* ignore */ }
console.error(`wrote ${OUT}: dark ${Object.keys(dark).length}, light ${Object.keys(light).length} vars`);
