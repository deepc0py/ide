# Deferred / blocked items — shared extension host (`exthost/`)

Things the shared VS Code extension host cannot (yet) do headlessly, with precise
reasons. Nothing here blocks the six required extensions from activating; each item
is either a non-permissive dependency we dropped or an Electron/workbench-only API
that has no meaning without a real UI.

## Dropped dependency (licensing)

- **jschardet** — `exthost` originally pulled this transitively (VS Code encoding
  detection). License is **LGPL-2.1+** (copyleft), which is outside the permissive
  allowlist. It is a lazy `importAMDNodeModule('jschardet')` only used for charset
  guessing, so it is stubbed (`src/stubs/native.cjs`) and removed from
  `package.json`. Effect: auto-encoding-detection falls back to UTF-8. No required
  extension needs it.
- **@microsoft/1ds-core-js / @microsoft/1ds-post-js / tas-client-umd** — telemetry
  appenders. Dropped per "drop telemetry"; stubbed.

## Electron-only / native modules stubbed (`build.mjs` NATIVE list → `src/stubs/native.cjs`)

These are redirected to a loud stub; any actual call throws a clear error instead of
hanging. None are reachable by the required extensions' exercised features:

- `electron` — the renderer/main Electron API. No headless equivalent.
- `@vscode/spdlog` — native rotating file logger. Logging degrades to dropped
  (VS Code's `SpdLogLogger` already tolerates a null logger). Symbol:
  `spdlog.setFlushOn` / `createAsyncRotatingLogger`.
- `@parcel/watcher`, `@vscode/vscode-languagedetection`, `@vscode/windows-*`,
  `@vscode/policy-watcher`, `@vscode/deviceid`, `node-pty`, `kerberos`,
  `@vscode/sqlite3`, `vsda`, `@vscode/windows-ca-certs`, `native-is-elevated`,
  `@vscode/ripgrep-universal`, `@vscode/tree-sitter-wasm`, `undici` — native/optional
  addons not needed for the LSP-bridge feature set.
    - Note: one extension (rust-analyzer) calls `undici`'s `Agent` during an update
      /telemetry fetch; the stub throws and the rejection is caught by the worker's
      `unhandledRejection` handler. It does not affect language features.

## Main-thread (MainThread*) APIs intentionally stubbed "loud, not hung"

Registered for every `MainContext.*` id so the ext host never hits an "unknown
actor"; implemented behaviourally where a required feature needs it, otherwise a
fallback that logs `UNIMPLEMENTED main-thread call: <Actor>.<method>` once and
resolves `undefined`. Deferred actor areas (no UI / not required by the six
extensions): Terminal (real pty), Debug, Task running, Testing, SCM, Comments,
Notebooks, Timeline, Quick Open / Dialogs, EditorInsets, CustomEditors,
Chat*/LanguageModels (chat UI + model providers), Decorations (file-explorer
badges). These are observable in the worker log and are safe no-ops.

## Feature caveats

- **rust-lang.rust-analyzer — go-to-definition (RESOLVED)**: cross-file/cross-module
  go-to-definition now works end-to-end and the test asserts the exact target
  location (`src/math.rs`, line 0) for a `math::add` call site. Two real causes were
  found and fixed:
  1. **Toolchain unreachable in the test env.** The harness sets `HOME=/tmp/...`
     (per the sandbox rules), but `cargo`/`rustc` are rustup *shims* that locate the
     toolchain via `$RUSTUP_HOME` (default `$HOME/.rustup`). With an empty `/tmp`
     HOME, `cargo metadata` fails, so rust-analyzer never builds its crate graph and
     *both* definition and hover return empty. Fix: the test (and
     `scripts/measure.mjs`) pass the real `RUSTUP_HOME` (read-only) plus a `/tmp`
     `CARGO_HOME`, keeping HOME in `/tmp`. The production IDE runs with the user's
     real HOME, where this already works.
  2. **A bogus hover masked the failure.** `lsp.ts`'s `filterMatches` ignored a
     DocumentFilter's `pattern`, so Python's pattern-only hover provider
     (`**/*requirement*.txt`) matched *every* language and returned a spurious
     `pypi.org/project/<word>` hover on Rust files — the old "definition **or**
     hover" test passed on that. `filterMatches` now honours `pattern`
     (glob/RelativePattern), so providers only match the documents they declare.

- **ms-python.python — language features**: works via Jedi
  (`"python.languageServer": "Jedi"`); hover on a symbol returns the signature. No
  Pylance (proprietary) is used.

- **OutputChannel text / `ide/output/append`**: not emitted. In this VS Code build
  `OutputChannel.append()` routes text through a file logger (spdlog), which the
  build stubs out (native, dropped). The main thread only receives
  `MainThreadOutputService.$register(label, file, langId, extId)` and
  `$update(channelId, Append)` — neither carries the text (the real workbench reads
  it back from the log file). Surfacing `ide/output/append{channel,text}` would need
  replacing the stubbed logger with a capturing `ILogger` that forwards each
  `append()`; deferred. Clients should render extension `LogMessage`s meanwhile.
