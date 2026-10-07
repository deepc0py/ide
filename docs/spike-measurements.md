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
