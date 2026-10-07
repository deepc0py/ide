# Shared extension host — memory

Measured on Apple Silicon (macOS 27), Node v26. The target workload is **8 git
worktrees of microsoft/vscode** (`/tmp/wt/vscode-{1..8}`) with the 6 required
extensions installed and active. Each worktree contains a real Rust crate
(`cli/Cargo.toml`, the VS Code CLI with heavy crates.io deps) one level down, which
matches rust-analyzer's `workspaceContains:*/Cargo.toml` activation — so
rust-analyzer *does* launch, and it dominates memory.

"Host-tree RSS" is the RSS of the host process **plus every descendant** (child
language servers: rust-analyzer, the eslint server, cargo metadata, git, …),
summed exactly like `bench/membench.py`.

## Reproduce

```
cd exthost && npm ci && npm run build
node scripts/measure.mjs --workspaces 8          # default limit 1500 MB, exit 0/1
node scripts/measure.mjs --workspaces 1 --settle 30
```

`scripts/measure.mjs` boots `dist/host.js`, opens N windows over the control socket
(one `host/openWorkspace` per window, each returning its own LSP socket), opens one
representative `.ts` file per window, lets the tree settle, then sums the host
process tree's RSS (peak of 5 samples) and prints a per-process breakdown plus
`host/stats`. It passes the real `RUSTUP_HOME` (read-only) and a `/tmp`
`CARGO_HOME` so rust-analyzer's rustup-shimmed `cargo`/`rustc` work while `HOME`
stays in `/tmp`.

**It writes no memory-saving settings.** The low-memory levers are now **shipped
product defaults** (`src/config.ts` `PRODUCT_DEFAULT_SETTINGS`, in the `defaults`
configuration layer, user-overridable): `rust-analyzer.cachePriming.enable=false`,
`rust-analyzer.checkOnSave=false` (native rust-analyzer diagnostics still report
type/borrow errors — no `cargo check` rustc swarm),
`rust-analyzer.cargo.buildScripts.enable=false`,
`rust-analyzer.procMacro.enable=false`, `python.languageServer=Jedi`. So
`measure.mjs` (settings.json = only `telemetry.telemetryLevel: off`) measures the
**real shipped defaults** and agrees with the full-product `bench/membench.py` run.

## Architecture: one shared isolate, per-window routing

All windows run in **one** extension-host isolate (one `worker_thread` inside the
host process), which hosts every open worktree as a single **multi-root** VS Code
workspace. Each window still gets its own LSP socket and the control/LSP contract is
unchanged (`host/openWorkspace -> { workspaceId, lspSocket }`). The hub
(`src/session.ts`) routes outbound traffic:

- **diagnostics** and **decorations** → the window that owns the URI (longest
  folder-prefix match), so a lint error in worktree 3 reaches only window 3;
- **commands / status bar / view + webview registration** → broadcast to all windows
  (they are extension-global in one isolate);
- **requests** (applyEdit, showMessageRequest) → the owning window (by URI) or the
  primary window.

Because the isolate is shared, **each extension and each language server loads
once** for all windows instead of once per window. rust-analyzer runs as a single
server over all worktrees' Cargo projects (crates.io dependency sources live at
identical `CARGO_HOME` paths, so they are loaded once); eslint runs a single
multi-root server; etc.

The decisive lever is **`rust-analyzer.cachePriming.enable: false`**: it makes
rust-analyzer index lazily on demand instead of eagerly indexing all 8 CLI crates +
their deps at startup. Go-to-definition/hover still work (computed on first query);
the exthost test proves cross-file definition still resolves. The companion levers
(all shipped defaults) kill the transient `rustc` swarm and extra servers:
`checkOnSave=false` (no `cargo check`), `cargo.buildScripts.enable=false`,
`procMacro.enable=false` (no `rust-analyzer-proc-macro-srv`), and `python` on Jedi.

**Dedup — one rust-analyzer, not two.** The native IDE also ships a *built-in*
rust-analyzer (`lapce-proxy` `default_lsp_servers`). Running it **and** the
rust-analyzer extension's server for the same workspace would double the cost. The
host now advertises the languages its extensions fully serve (`providedLanguages`
in `host/openWorkspace`; `host.ts` `LANGUAGE_SERVER_EXTENSIONS`), and the proxy
suppresses the matching built-in (`PluginCatalog::suppressed_languages`). So one
shared rust-analyzer serves every window; the built-in TypeScript server still runs
(no TS language-server extension among the six). See `docs/exthost-integration.md`.

## Before / after — 8 worktrees, all 6 extensions

| configuration | host-tree RSS (8 windows) |
| --- | --- |
| per-window extension hosts (one ext host + one rust-analyzer **per window**) | **10,571 MB** |
| shared isolate, rust-analyzer eager (`cachePriming` on) | 2,326 MB |
| **shared isolate + lazy rust-analyzer (final)** | **≈ 900 MB** |

Final 8-window breakdown (one sample):

| process | RSS |
| --- | --- |
| `node dist/host.js` (control + the single isolate thread) | ~340 MB |
| `rust-analyzer` (one server, all 8 Cargo projects, lazy) | ~290–380 MB |
| eslint server (one, multi-root) | ~60 MB |
| transient `cargo metadata` / `git blame` | ~20–70 MB each, brief |
| **host-tree total** | **≈ 900 MB** |

`host/stats`: process RSS ≈ 340 MB; the single isolate's V8 `heapUsed` ≈ 90 MB
(reported once per window since the isolate is shared).

1 worktree, shipped defaults: **661 MB** host tree (node host ≈ 344 MB + one
rust-analyzer ≈ 264 MB + eslint ≈ 54 MB).

## Full product — `bench/membench.py`, shipped defaults, all 6 extensions

`measure.mjs` above measures only the **host tree**. The product benchmark
(`bench/membench.py`, 8 windows, `--settle 90 --gap 5`, peak of 5 samples) counts
the whole `ide` process tree: the single `ide` process (all 8 windows — folders
open as windows of one instance), its one shared `node` host, and the host's
child servers. This is the number the hard target applies to.

| configuration | TOTAL (8 windows) | rust-analyzer | ide proc | node host | proc-macro-srv | cargo-check `rustc` swarm |
| --- | --- | --- | --- | --- | --- | --- |
| **before** — defaults did not ship the memory levers | **2585 MB** | 1342 | 738 | 274 | 2 × 55 | present |
| **after** — shipped product defaults (+ dedup, lazy webviews) | **1180 MB** (median of 3) | 384 | 305 | 349 | none | none |

The `after` figure is the **median of 3 runs** against a release build of the
current tree: 1180.0 / 1179.2 / 1185.7 MB (the breakdown row is a representative
sample; rust-analyzer 380–389, node host 348–350, ide 303–308 across runs).
rust-analyzer drops 1342 → 384 MB (lazy indexing,
`cachePriming` off); the entire transient `cargo check` → `rustc` swarm and both
`rust-analyzer-proc-macro-srv` processes are gone (`checkOnSave` / `buildScripts` /
`procMacro` off). Native rust-analyzer diagnostics and go-to-definition still work
(exthost tests + `lapce-proxy/tests/lsp_servers.rs` pass). membench launches
windows only (opens no files), so the built-in-server dedup and lazy-webview
creation don't move this number — they cut real-session cost (a second
rust-analyzer per workspace; eager hidden `WKWebView`s per window).

## Result vs. the hard target

Hard target: full `ide` process tree ≤ 2048 MB (aim ~1.5 GB).

```
full product tree, 8 windows, shipped defaults   1180 MB (median of 3)  <= 2048 MB  ✅  (and <= 1.5 GB ✅)
  rust-analyzer (one, lazy)    384 MB
  node host (one isolate)      349 MB
  ide (one proc, 8 windows)    305 MB
```

## The floor

With lazy indexing the floor is **~2 processes' worth of V8/analysis**: the Node
isolate (~340 MB: Node runtime + the bundled VS Code ext-host + 6 extensions' code,
loaded once) and one rust-analyzer (~300 MB resident even when idle, for the crate
graph + the parts of the CLI/std it has touched). Everything else is small or
transient. Pushing lower would mean either not running rust-analyzer at all for
idle windows, or trimming the ext-host bundle — neither is warranted since we are
comfortably under budget. If a future workload opened 8 *distinct* heavy Rust
projects (no shared dep sources) and the user actively navigated all of them,
rust-analyzer would grow toward the eager figure; `cachePriming` keeps idle windows
cheap regardless.
