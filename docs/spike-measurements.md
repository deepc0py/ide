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

Not measured: VS Code workbench inside WKWebView (Tauri/wry). VS Code's per-window renderer above is 75–120 MB and
the per-window extension host 60–120 MB; a WKWebView-hosted workbench carries the same JS workbench heap per window,
so it lands in the same band as Electron minus the Chromium overhead. [INFERENCE]
