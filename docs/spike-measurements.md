# Spike measurements

Machine: MacBook (Apple Silicon, arm64), macOS 27.0.1. Benchmark: `bench/membench.py`, 8 windows on 8 worktrees of
`microsoft/vscode` (`/tmp/wt/vscode-{1..8}`), 2 s between launches, 40 s settle, peak of 5 samples. Total = summed
RSS of matched processes and all descendants (shared pages are double-counted, same as Activity Monitor's sum).

| Candidate | Extensions | Processes | Total RSS |
|---|---|---|---|
| VS Code 1.x stable (Electron), fresh profile | none (built-ins only) | 39 | 6441 MB |
| Lapce 0.4.6 (upstream), `lapce <path>` x8 | n/a | 17 | 417 MB — **but only 1 window**: upstream forwards folders as tabs of the existing window |
| ide (Lapce fork, folders open as new windows), 8 real windows (verified via CGWindowList) | none yet | 17 (1 ide + 16 shells) | 437 MB |
