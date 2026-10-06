# Shared extension host — memory measurements

Measured on Apple Silicon (macOS 27), Node v26, one shared `node exthost/dist/host.js`
process with one `worker_thread` per workspace. RSS is the whole host process
(`process.memoryUsage().rss` via the `host/stats` control verb); per-worker figures
are each worker isolate's V8 `heapUsed`. Reproduce with `node /tmp/measure.mjs`
(script in repo history) or the `host/stats` verb.

## 1 workspace, all 6 required extensions active

Fixtures multi-root workspace (TS + Rust + Python + Git). All six extensions
(eslint, prettier, gitlens, python, rust-analyzer, claude-code) confirmed active.

| metric | value |
| --- | --- |
| host RSS | **326 MB** |
| worker heapUsed | 107 MB |

## 8 workspaces (/tmp/wt/vscode-{1..8}), one worker each

Eight separate worker_threads in the single shared host process, each opening one
VS Code git worktree. Startup extensions (eslint, prettier, gitlens, claude-code)
activate on `onStartupFinished`; rust-analyzer / python activate only on their
`workspaceContains` / `onLanguage` events, which these JS/TS worktrees do not
trigger, so those two stay dormant per window (as they would in real use for a
non-Rust/Python repo).

| metric | value |
| --- | --- |
| host RSS (8 workers) | **1755 MB** |
| per-worker heapUsed | 80, 84, 78, 77, 77, 88, 77, 84 MB |

**Result: 1755 MB < 2048 MB ceiling.** For reference the brief measured VS Code
with 8 windows at 6.4 GB; this shared host is ~3.6× smaller for the extension-host
tier, and runs in ONE OS process (8 threads) instead of 8 Electron helper trees.

## Scaling notes / the multi-root alternative

Each worker_thread runs its own extension-host isolate, so extension *code* is
loaded once per window (not shared across windows); only the OS process, the Node
runtime, and native addons are shared. Memory therefore scales ~linearly with
(windows × active-extensions), but off a far smaller base than per-window Electron.

The 8-worker design stays under budget, so the alternative single-worker
multi-root layout (all 8 folders in one extension host, routing views per window)
was **not required**. It remains implementable (the worker already accepts a
multi-folder `folders[]` and builds a multi-root `IWorkspaceData`); it would share
extension instances across windows (lower memory) at the cost of per-window
activation isolation and more complex per-window view routing. If a future target
tightens the ceiling below ~1.8 GB for heavy (Rust+Python-in-every-window)
workloads, switch to that layout.
