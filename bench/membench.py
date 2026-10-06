#!/usr/bin/env python3
"""Memory benchmark: open one window per worktree, settle, sum RSS of the IDE process tree.

Usage:
  membench.py --launch 'CMD {path}' --match REGEX [--settle 30] [--worktrees DIR...]

`--launch` is run once per worktree with `{path}` substituted.
Processes whose command line matches `--match`, plus all their descendants,
are counted. Exits 0 when total RSS <= --limit-mb (default 2048), else 1.
"""
import argparse
import os
import re
import shlex
import subprocess
import sys
import time


def snapshot():
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,rss=,command="], capture_output=True, text=True).stdout
    procs = {}
    for line in out.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 4:
            continue
        pid, ppid, rss, cmd = int(parts[0]), int(parts[1]), int(parts[2]), parts[3]
        procs[pid] = (ppid, rss, cmd)
    return procs


def tree_rss(procs, pattern):
    rx = re.compile(pattern)
    roots = {pid for pid, (_, _, cmd) in procs.items() if rx.search(cmd) and pid != os.getpid()}
    children = {}
    for pid, (ppid, _, _) in procs.items():
        children.setdefault(ppid, []).append(pid)
    seen, stack = set(), list(roots)
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        stack.extend(children.get(pid, []))
    rows = sorted(((procs[p][1], p, procs[p][2]) for p in seen), reverse=True)
    return sum(r for r, _, _ in rows), rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--launch", required=True)
    ap.add_argument("--match", required=True)
    ap.add_argument("--settle", type=float, default=30)
    ap.add_argument("--gap", type=float, default=2)
    ap.add_argument("--limit-mb", type=float, default=2048)
    ap.add_argument("--samples", type=int, default=5)
    ap.add_argument("--worktrees", nargs="+", default=[f"/tmp/wt/vscode-{i}" for i in range(1, 9)])
    args = ap.parse_args()

    for wt in args.worktrees:
        cmd = args.launch.replace("{path}", shlex.quote(wt))
        subprocess.Popen(cmd, shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(args.gap)
    time.sleep(args.settle)

    peak, peak_rows = 0, []
    for _ in range(args.samples):
        total, rows = tree_rss(snapshot(), args.match)
        if total > peak:
            peak, peak_rows = total, rows
        time.sleep(2)

    for rss, pid, cmd in peak_rows[:40]:
        print(f"{rss / 1024:9.1f} MB  {pid:>7}  {cmd[:120]}")
    mb = peak / 1024
    print(f"PROCESSES {len(peak_rows)}  TOTAL_RSS_MB {mb:.1f}  LIMIT_MB {args.limit_mb:.0f}")
    sys.exit(0 if mb <= args.limit_mb else 1)


if __name__ == "__main__":
    main()
