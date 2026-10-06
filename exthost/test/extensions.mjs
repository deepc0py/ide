// Download + extract required VS Code extensions from Open VSX (open-vsx.org only).
//
// Usage:
//   import { ensureExtensions, EXTENSION_IDS, LOCK } from './extensions.mjs';
//   const dirs = await ensureExtensions();            // populate /tmp cache, returns { id: absDir }
//   node test/extensions.mjs                          // (re)generate extensions.lock.json via resolveLatest()
//
// Each resolved extension is extracted so that `<cacheDir>/<id>-<version>/package.json`
// exists directly (the vsix `extension/` subtree becomes the extension root).
//
// Only Node built-ins are used plus a shell-out to /usr/bin/unzip for extraction.

import { createHash } from 'node:crypto';
import { createReadStream, createWriteStream, existsSync, readFileSync } from 'node:fs';
import * as fs from 'node:fs/promises';
import { Readable } from 'node:stream';
import { finished } from 'node:stream/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const execFileP = promisify(execFile);
const __dirname = path.dirname(fileURLToPath(import.meta.url));
const LOCK_PATH = path.join(__dirname, 'extensions.lock.json');

const OPEN_VSX = 'https://open-vsx.org';
const TARGET_PLATFORM = 'darwin-arm64';
const DOWNLOAD_TIMEOUT_MS = 20 * 60 * 1000; // generous: big platform builds (claude-code ~100MB)
const META_TIMEOUT_MS = 60 * 1000;

// Canonical ids (lowercase) used as map keys + exported list.
export const EXTENSION_IDS = [
  'dbaeumer.vscode-eslint',
  'esbenp.prettier-vscode',
  'eamodio.gitlens',
  'ms-python.python',
  'rust-lang.rust-analyzer',
  'anthropic.claude-code',
];

// Source descriptors. `target: 'darwin-arm64'` means we prefer the Apple-Silicon
// platform build; `universal` means a single cross-platform vsix.
// `namespace`/`name` use the exact casing Open VSX serves download URLs with.
const SOURCES = [
  { id: 'dbaeumer.vscode-eslint', namespace: 'dbaeumer', name: 'vscode-eslint', target: 'universal' },
  { id: 'esbenp.prettier-vscode', namespace: 'esbenp', name: 'prettier-vscode', target: 'universal' },
  { id: 'eamodio.gitlens', namespace: 'eamodio', name: 'gitlens', target: 'universal' },
  // ms-python.python has no darwin-arm64 target build on Open VSX -> universal.
  { id: 'ms-python.python', namespace: 'ms-python', name: 'python', target: 'universal' },
  // rust-analyzer ships the server binary only in platform builds.
  { id: 'rust-lang.rust-analyzer', namespace: 'rust-lang', name: 'rust-analyzer', target: 'darwin-arm64' },
  // claude-code has a darwin-arm64 target build.
  { id: 'anthropic.claude-code', namespace: 'Anthropic', name: 'claude-code', target: 'darwin-arm64' },
];

function srcFor(id) {
  const s = SOURCES.find((x) => x.id === id);
  if (!s) throw new Error(`Unknown extension id: ${id}`);
  return s;
}

function loadLock() {
  try {
    if (existsSync(LOCK_PATH)) {
      return JSON.parse(readFileSync(LOCK_PATH, 'utf8'));
    }
  } catch {
    /* ignore malformed/missing lock */
  }
  return null;
}

export const LOCK = loadLock();

async function fetchJson(url, timeoutMs = META_TIMEOUT_MS) {
  const ctl = new AbortController();
  const t = setTimeout(() => ctl.abort(), timeoutMs);
  try {
    const res = await fetch(url, { signal: ctl.signal, redirect: 'follow' });
    if (!res.ok) throw new Error(`GET ${url} -> ${res.status}`);
    return await res.json();
  } finally {
    clearTimeout(t);
  }
}

async function fetchText(url, timeoutMs = META_TIMEOUT_MS) {
  const ctl = new AbortController();
  const t = setTimeout(() => ctl.abort(), timeoutMs);
  try {
    const res = await fetch(url, { signal: ctl.signal, redirect: 'follow' });
    if (!res.ok) return null;
    return await res.text();
  } catch {
    return null;
  } finally {
    clearTimeout(t);
  }
}

// Open VSX exposes files.sha256 as a URL to a `.sha256` sidecar. Fetch it and
// extract a 64-hex digest if one is actually published; otherwise null.
async function resolveSha256(filesSha256Url) {
  if (!filesSha256Url) return null;
  const text = await fetchText(filesSha256Url);
  if (!text) return null;
  const m = text.trim().match(/\b([a-fA-F0-9]{64})\b/);
  return m ? m[1].toLowerCase() : null;
}

// Resolve metadata for one source into a lock entry.
async function resolveOne(src) {
  let meta;
  if (src.target && src.target !== 'universal') {
    // Try the platform build first; fall back to universal if absent.
    meta = await fetchJson(`${OPEN_VSX}/api/${src.namespace}/${src.name}/${src.target}/latest`);
    if (meta?.error || !meta?.files?.download) {
      meta = await fetchJson(`${OPEN_VSX}/api/${src.namespace}/${src.name}`);
    }
  } else {
    meta = await fetchJson(`${OPEN_VSX}/api/${src.namespace}/${src.name}`);
  }
  if (!meta?.files?.download) {
    throw new Error(`No download URL resolved for ${src.id} (${JSON.stringify(meta?.error)})`);
  }
  const sha256 = await resolveSha256(meta.files.sha256);
  return {
    id: src.id,
    namespace: src.namespace,
    name: src.name,
    target: meta.targetPlatform || src.target || 'universal',
    version: meta.version,
    download: meta.files.download,
    sha256,
  };
}

// (Re)resolve every source to its latest Open VSX version. Used to generate the lock.
export async function resolveLatest() {
  const extensions = {};
  for (const src of SOURCES) {
    process.stderr.write(`[resolve] ${src.id} (${src.target}) ...\n`);
    const entry = await resolveOne(src);
    extensions[src.id] = entry;
    process.stderr.write(`[resolve] ${src.id} -> ${entry.version} (${entry.target})\n`);
  }
  return { generatedAt: new Date().toISOString(), openVsx: OPEN_VSX, targetPlatform: TARGET_PLATFORM, extensions };
}

async function writeLock(lock) {
  await fs.writeFile(LOCK_PATH, JSON.stringify(lock, null, 2) + '\n', 'utf8');
  return LOCK_PATH;
}

async function sha256File(file) {
  const hash = createHash('sha256');
  const stream = createReadStream(file);
  await finished(stream.on('data', (c) => hash.update(c)));
  return hash.digest('hex');
}

async function download(url, dest) {
  const ctl = new AbortController();
  const t = setTimeout(() => ctl.abort(), DOWNLOAD_TIMEOUT_MS);
  try {
    const res = await fetch(url, { signal: ctl.signal, redirect: 'follow' });
    if (!res.ok || !res.body) throw new Error(`GET ${url} -> ${res.status}`);
    const out = createWriteStream(dest);
    await finished(Readable.fromWeb(res.body).pipe(out));
  } finally {
    clearTimeout(t);
  }
}

// Extract the vsix `extension/` subtree into `destDir` (so destDir has package.json directly).
async function extractExtension(vsix, destDir) {
  const staging = destDir + '.staging';
  await fs.rm(staging, { recursive: true, force: true });
  await fs.mkdir(staging, { recursive: true });
  // -o overwrite, -q quiet; only the extension/ tree is needed.
  await execFileP('/usr/bin/unzip', ['-o', '-q', vsix, 'extension/*', '-d', staging], {
    maxBuffer: 64 * 1024 * 1024,
  });
  const extRoot = path.join(staging, 'extension');
  if (!existsSync(path.join(extRoot, 'package.json'))) {
    throw new Error(`vsix missing extension/package.json: ${vsix}`);
  }
  await fs.rm(destDir, { recursive: true, force: true });
  await fs.rename(extRoot, destDir);
  await fs.rm(staging, { recursive: true, force: true });
}

// Resolve a single extension entry, preferring the lock file for reproducibility.
async function entryFor(id) {
  if (LOCK?.extensions?.[id]?.download) return LOCK.extensions[id];
  return resolveOne(srcFor(id));
}

/**
 * Download + extract all required extensions into `cacheDir`.
 * Idempotent: skips any extension whose dir already holds a package.json.
 * @returns {Promise<Record<string,string>>} map of id -> absolute extension dir
 */
export async function ensureExtensions(cacheDir = '/tmp/ide-exthost-extensions') {
  await fs.mkdir(cacheDir, { recursive: true });
  const result = {};
  for (const id of EXTENSION_IDS) {
    const entry = await entryFor(id);
    const dir = path.resolve(cacheDir, `${id}-${entry.version}`);
    const pkg = path.join(dir, 'package.json');
    if (existsSync(pkg)) {
      process.stderr.write(`[cache]  ${id}@${entry.version} -> ${dir} (present)\n`);
      result[id] = dir;
      continue;
    }
    const tmpVsix = path.join(cacheDir, `.${id}-${entry.version}.vsix`);
    process.stderr.write(`[get]    ${id}@${entry.version} (${entry.target}) <- ${entry.download}\n`);
    await download(entry.download, tmpVsix);
    if (entry.sha256) {
      const got = await sha256File(tmpVsix);
      if (got !== entry.sha256) {
        await fs.rm(tmpVsix, { force: true });
        throw new Error(`sha256 mismatch for ${id}: expected ${entry.sha256} got ${got}`);
      }
      process.stderr.write(`[verify] ${id}@${entry.version} sha256 ok\n`);
    }
    process.stderr.write(`[unzip]  ${id}@${entry.version} -> ${dir}\n`);
    await extractExtension(tmpVsix, dir);
    await fs.rm(tmpVsix, { force: true });
    result[id] = dir;
  }
  return result;
}

// Run as main: regenerate the lock file.
if (import.meta.url === pathToFileURL(process.argv[1] || '').href) {
  const lock = await resolveLatest();
  const written = await writeLock(lock);
  process.stderr.write(`[lock]   wrote ${written}\n`);
  for (const [id, e] of Object.entries(lock.extensions)) {
    process.stdout.write(`${id}\t${e.version}\t${e.target}\t${e.sha256 ? 'sha256' : 'no-sha256'}\n`);
  }
}
