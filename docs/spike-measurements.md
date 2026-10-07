# Spike measurements

Machine: MacBook (Apple Silicon, arm64), macOS 27.0.1. Benchmark: `bench/membench.py`, 8 windows on 8 worktrees of
`microsoft/vscode` (`/tmp/wt/vscode-{1..8}`), 2 s between launches, 40 s settle, peak of 5 samples. Total = summed
RSS of matched processes and all descendants (shared pages are double-counted, same as Activity Monitor's sum).

| Candidate | Extensions | Processes | Total RSS |
|---|---|---|---|
| VS Code 1.x stable (Electron), fresh profile | none (built-ins only) | 39 | 6441 MB |
| Lapce 0.4.6 (upstream), `lapce <path>` x8 | n/a | 17 | 417 MB — **but only 1 window**: upstream forwards folders as tabs of the existing window |
| ide (Lapce fork, folders open as new windows), 8 real windows (verified via CGWindowList) | none yet | 17 (1 ide + 16 shells) | 437 MB |
| Zed stable (GPUI, GPL-3.0 editor), `cli --new` x8 | none | 2 | 420 MB (9 windows incl. startup window) |
| ide (Lapce fork) + shared extension host, 8 windows, **all 6 extensions** — *before* memory work | 6 (eslint, gitlens, prettier, python/Jedi, rust-analyzer, Claude Code) | 28 | **2585 MB** (rust-analyzer 1342, ide 738, node host 274, 2× proc-macro-srv 55, + transient `cargo check` rustc) |
| ide + shared extension host, 8 windows, **all 6 extensions** — *after* (shipped product defaults + server dedup + lazy webviews) | 6 | 19 | **1180 MB** (median of 3: 1180.0 / 1179.2 / 1185.7; rust-analyzer 384, node host 349, ide 305; no proc-macro-srv, no cargo-check rustc) |

Not measured: VS Code workbench inside WKWebView (Tauri/wry). VS Code's per-window renderer above is 75–120 MB and
the per-window extension host 60–120 MB; a WKWebView-hosted workbench carries the same JS workbench heap per window,
so it lands in the same band as Electron minus the Chromium overhead. [INFERENCE]

## WebKit-inclusive scenarios (extension webviews counted)

The table above sums the `ide` process tree, but VS Code extension **webviews**
render in separate WebKit XPC processes (`com.apple.WebKit.WebContent` / `.GPU`
/ `.Networking`) that macOS reparents to `launchd` — so they are *not* tree
descendants and were previously uncounted. `bench/membench.py --include-webkit`
attributes each WebKit process to the IDE via its *responsible pid*
(`responsibility_get_pid_responsible_for_pid`, matched against the IDE tree **or
the IDE root's own responsible/session-leader pid**, which covers shell-launched
runs where the WKWebView helpers inherit the terminal's responsible pid). All
numbers below are **median of 3 runs**, 8 windows on `/tmp/wt/vscode-{1..8}`,
5 s between launches, 120 s settle, peak of 4 samples, WebKit included.

| Scenario | Reductions | Procs | WebKit procs | Total RSS |
|---|---|---|---|---|
| **(a)** 8 windows, extension docks closed | n/a | 19 | 0 | **1131 MB** (1130.0 / 1132.6 / 1131.3) |
| **(b)** 8 windows, Claude Code view open in every window — *before* | — | 59 | 39 | **5448 MB** (5448.5 / 5442.4 / 6629.7) |
| **(b)** 8 windows, Claude Code view open in every window — *after* | active-view-only + cross-window suspension | 23 | 3 | **1429 MB** (1421.5 / 1429.1 / 1450.2) |
| **(c)** 8 windows + 1 TS + 1 Rust file per window, 120 s settle | `maxTsServerMemory` cap | 28 | 0 | **1319 MB** (1319.0 / 1331.0 / 701.8) |

All scenarios are under the 2048 MB budget.

### (b) breakdown and the webview reductions

*Before:* 5448 MB. One window alone spawned **9 WebKit processes (~690 MB)** —
the client auto-resolved and attached a live `WKWebView` for **every** registered
webview view (Claude: sidebar + secondary + sessions; GitLens: graph, commit
details, patch details, welcome = 7 views), each costing its own `WebContent`
process, plus a shared GPU + Networking. ×8 windows ≈ 39–47 WebKit processes.

Two reductions in `lapce-app/src/webview_view.rs` (WebKit is process-per-webview;
wry 0.57 exposes no shared `WKProcessPool`, so the levers are *how many* webviews
are live):
1. **Active-view-only** — the dock shows one view at a time, so only the active
   view keeps a live `WKWebView`; the other registered views are disposed and
   re-attached lazily when selected (the shim persists `getState`). Per-window
   WebKit dropped **9 procs / 690 MB → 1 `WebContent` + shared GPU/Net / ~92 MB**.
2. **Cross-window suspension** — only the active (focused, or most-recently-opened
   when headless) window hosts a live webview; background windows dispose their
   `WebContent` and re-attach (with full theme fidelity) on focus. 8 live
   `WebContent` → **1**. *After:* 1429 MB (3 WebKit procs: 1 `WebContent` + 1 GPU
   + 1 Networking). Breakdown: rust-analyzer 380, ide 354, node host 313,
   `WebContent` ~130, Jedi 63, 16 shells ~143, GPU 29, Networking 12.

### (c) and the tsserver cap

`strings.ts` lives in VS Code's `src/` TypeScript project, whose full type graph
needs **~4.5 GB** in a single tsserver — more than the entire budget for *one*
window, and there is one tsserver per window. Uncapped, a single tsserver
balloons to ~4.5 GB in ~20 s then OOM-crashes; 8 at once would need ~36 GB.
`lsp_config.rs` now caps each tsserver's heap via `maxTsServerMemory`
(`--max-old-space-size`, default 1024 MB, env `IDE_TS_MAX_MEMORY_MB`). The cap
holds the pre-exit spike to ~1 GB/server (keeping 8 simultaneous indexers from
thrashing the machine) but does **not** make tsserver fit the oversized repo: it
OOM-*exits* rather than degrading (typescript-language-server 6.x then stops it,
no restart; and its parser can't force the lightweight syntax server —
`useSyntaxServer:"always"` silently maps to `auto`). So at the 120 s sample the
per-window tsservers have mostly exited and **(c) measures ~1319 MB** (highly
variable, 701–1331 MB, depending on how many are still mid-index) — i.e. under
budget, but with TS semantics *not* live on this pathological repo. rust-analyzer
is unaffected: one shared instance (384 MB) serves all windows' Rust via the
exthost, exactly as in the baseline.

### Startup auto-open for benchmarking

`IDE_OPEN_VIEW=<viewId>` (e.g. `IDE_OPEN_VIEW=claudeVSCodeSidebar`) reveals the
extension dock with that view on startup — a documented, cheap alternative to
`IDE_WEBVIEW_SELFTEST` (no DOM probe/snapshot), used to drive scenario (b).

### Exact commands

```sh
# Common environment (shared host on, 6 extensions installed under /tmp/bench-ext).
# The binary is copied to a uniquely-named path so --match is unambiguous.
cp target/release/ide /tmp/mbench/idebench
export IDE_DATA_DIR=/tmp/ide-bench-data IDE_EXTENSIONS_DIR=/tmp/bench-ext RUSTUP_HOME=$HOME/.rustup

# (a) 8 windows, docks closed:
python3 bench/membench.py --include-webkit --match 'mbench/idebench' \
  --launch '/tmp/mbench/idebench --wait {path}' --gap 5 --settle 120 --samples 4

# (b) 8 windows, Claude Code open in every window:
python3 bench/membench.py --include-webkit --match 'mbench/idebench' \
  --launch 'IDE_OPEN_VIEW=claudeVSCodeSidebar /tmp/mbench/idebench --wait {path}' \
  --gap 5 --settle 120 --samples 4

# (c) 8 windows + one TS + one Rust file per window (files route to the active
# window, so open each window then immediately its files before the next opens):
for i in $(seq 1 8); do
  wt=/tmp/wt/vscode-$i
  /tmp/mbench/idebench --wait "$wt" & sleep 5
  /tmp/mbench/idebench "$wt/src/vs/base/common/strings.ts" & sleep 1
  /tmp/mbench/idebench "$wt/cli/src/lib.rs" & sleep 2
done
python3 bench/membench.py --include-webkit --measure-only \
  --match 'mbench/idebench' --settle 120 --samples 4
```

## With SonarQube for IDE (connected mode, optional)

`node exthost/scripts/measure.mjs --sonarlint --workspaces 8` (host tree only; the SonarQube server runs in its own
external container and is excluded):

| Config | SonarLint JVM | Sonar JS/TS bridge | node host | host tree |
|---|---|---|---|---|
| SonarLint defaults | 1321 MB | — | — | 2557 MB |
| shipped caps (`sonarlint.ls.vmargs=-Xmx512m…`, `sonar.javascript.node.maxspace=512`) | 596 MB | 779 MB | 406 MB | 1838 MB |

With the IDE process (~300 MB for 8 windows), enabling SonarQube for IDE pushes the total to roughly 2.1 GB. That is
about 100 MB over the 2 GB target. The bridge's RSS isn't V8 heap, so the heap cap doesn't reduce it. Without
SonarQube for IDE, the 6 required extensions measure 1131–1429 MB in total (see above).
