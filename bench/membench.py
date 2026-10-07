#!/usr/bin/env python3
"""Memory benchmark: open one window per worktree, settle, sum RSS of the IDE process tree.

Usage:
  membench.py --launch 'CMD {path}' --match REGEX [--settle 30] [--worktrees DIR...]

`--launch` is run once per worktree with `{path}` substituted.
Processes whose command line matches `--match`, plus all their descendants,
are counted. Exits 0 when total RSS <= --limit-mb (default 2048), else 1.

WKWebView content (VS Code extension webviews) runs in separate WebKit XPC
processes (`com.apple.WebKit.WebContent` / `.GPU` / `.Networking`) reparented to
launchd (ppid 1), so they are NOT descendants of the IDE and the plain tree walk
misses them. With `--include-webkit` (macOS) each such process is attributed to
its *responsible* process via libproc's
`responsibility_get_pid_responsible_for_pid`; those whose responsible pid falls
inside the matched IDE tree are added to the total. If that symbol is
unavailable and exactly one IDE tree is present, all WebKit XPC processes are
attributed to it (heuristic, logged as such).
"""
import argparse
import ctypes
import os
import re
import shlex
import subprocess
import sys
import time

# WebKit XPC content/GPU/Networking helper processes (reparented to launchd).
WEBKIT_RE = re.compile(r"com\.apple\.WebKit\.(WebContent|GPU|Networking)")


def _responsible_pid_fn():
    """libproc `responsibility_get_pid_responsible_for_pid`, or None if absent."""
    try:
        libc = ctypes.CDLL(None)
        fn = libc.responsibility_get_pid_responsible_for_pid
        fn.argtypes = [ctypes.c_int]
        fn.restype = ctypes.c_int
        return fn
    except (OSError, AttributeError):
        return None


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


def tree_pids(procs, pattern):
    """Pids matching `pattern` plus all their descendants (excluding this script)."""
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
    return roots, seen


def webkit_pids(procs, tree, roots, resp_fn):
    """WebKit XPC pids owned by the IDE.

    A WKWebView helper's *responsible* pid is inherited from whatever launched
    the host: when the IDE is started via LaunchServices (`open`) it is the IDE
    process itself (in `tree`); when started from a shell/terminal (how the
    bench runs it) it is the terminal's session leader — the *same* responsible
    pid the IDE process itself reports. So the owner set is the IDE tree plus the
    responsible pid of each IDE root; a WebKit proc is attributed iff its
    responsible pid is in that set. Other GUI apps' WebKit helpers report their
    own app as responsible (not the terminal), so they are excluded. The bench
    launches no other shell-hosted WKWebView app under the same session, making
    this exact there.

    Falls back to attributing every WebKit XPC process to the IDE when the
    responsible-pid symbol is missing and exactly one IDE tree root exists.
    """
    candidates = [pid for pid, (_, _, cmd) in procs.items() if WEBKIT_RE.search(cmd)]
    if resp_fn is not None:
        owners = set(tree)
        for r in roots:
            rp = resp_fn(r)
            if rp > 1:
                owners.add(rp)
        return {pid for pid in candidates if resp_fn(pid) in owners}
    if len(roots) == 1:
        sys.stderr.write(
            "membench: responsibility_get_pid_responsible_for_pid unavailable; "
            "attributing all WebKit XPC processes to the single IDE tree\n"
        )
        return set(candidates)
    sys.stderr.write(
        "membench: responsible-pid symbol unavailable and multiple IDE trees; "
        "WebKit processes NOT attributed\n"
    )
    return set()


def total_rss(procs, pattern, include_webkit, resp_fn):
    roots, tree = tree_pids(procs, pattern)
    wk = webkit_pids(procs, tree, roots, resp_fn) if include_webkit else set()
    counted = tree | wk
    rows = sorted(
        ((procs[p][1], p, procs[p][2], p in wk) for p in counted), reverse=True
    )
    return sum(r for r, _, _, _ in rows), rows, len(wk)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--launch", help="per-worktree launch command ({path} substituted); omit with --measure-only")
    ap.add_argument("--match", required=True)
    ap.add_argument("--settle", type=float, default=30)
    ap.add_argument("--gap", type=float, default=2)
    ap.add_argument("--limit-mb", type=float, default=2048)
    ap.add_argument("--samples", type=int, default=5)
    ap.add_argument("--include-webkit", action="store_true",
                    help="also count WebKit XPC processes (WKWebView content) owned by the IDE")
    ap.add_argument("--measure-only", action="store_true",
                    help="skip launching; windows/files are already open — just settle and sample")
    ap.add_argument("--worktrees", nargs="+", default=[f"/tmp/wt/vscode-{i}" for i in range(1, 9)])
    args = ap.parse_args()

    if not args.measure_only and not args.launch:
        ap.error("--launch is required unless --measure-only is given")

    resp_fn = _responsible_pid_fn() if args.include_webkit else None

    if not args.measure_only:
        for wt in args.worktrees:
            cmd = args.launch.replace("{path}", shlex.quote(wt))
            subprocess.Popen(cmd, shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(args.gap)
    time.sleep(args.settle)

    peak, peak_rows, peak_wk = 0, [], 0
    for _ in range(args.samples):
        total, rows, nwk = total_rss(snapshot(), args.match, args.include_webkit, resp_fn)
        if total > peak:
            peak, peak_rows, peak_wk = total, rows, nwk
        time.sleep(2)

    for rss, pid, cmd, is_wk in peak_rows[:40]:
        tag = "WK " if is_wk else "   "
        print(f"{rss / 1024:9.1f} MB  {tag}{pid:>7}  {cmd[:116]}")
    mb = peak / 1024
    wk_note = f"  WEBKIT_PROCS {peak_wk}" if args.include_webkit else ""
    print(f"PROCESSES {len(peak_rows)}{wk_note}  TOTAL_RSS_MB {mb:.1f}  LIMIT_MB {args.limit_mb:.0f}")
    sys.exit(0 if mb <= args.limit_mb else 1)


if __name__ == "__main__":
    main()
