# Spike measurements

Machine: MacBook (Apple Silicon, arm64), macOS 27.0.1. Benchmark: `bench/membench.py`, 8 windows on 8 worktrees of
`microsoft/vscode` (`/tmp/wt/vscode-{1..8}`), 2 s between launches, 40 s settle, peak of 5 samples. Total = summed
RSS of matched processes and all descendants (shared pages are double-counted, same as Activity Monitor's sum).

| Candidate | Extensions | Processes | Total RSS |
|---|---|---|---|
| VS Code 1.x stable (Electron), fresh profile | none (built-ins only) | 39 | 6441 MB |
