# ide

A native, low-memory editor for watching many coding agents at once: one window per git worktree, all windows in a
single Rust process, with VS Code extension compatibility through a shared Node extension host.

Forked from [Lapce](https://github.com/lapce/lapce) (Apache-2.0). See `NOTICE` and `docs/SPIKE.md`.

## Usage

```sh
cargo build --release
./target/release/ide /path/to/worktree   # opens a new window in the running instance
```

## Benchmark

```sh
python3 bench/membench.py --launch "./target/release/ide {path}" --match 'target/release/ide'
```
