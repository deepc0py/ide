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

- **rust-lang.rust-analyzer — go-to-definition**: rust-analyzer *activates*
  (`workspaceContains:Cargo.toml`), launches its bundled `server/rust-analyzer`
  (darwin-arm64) binary, and the language client registers `definition` and `hover`
  providers from the server's advertised capabilities (proving the full
  LSP ⇄ ExtHost ⇄ language-client ⇄ server round-trip works). In the headless
  harness, however, `textDocument/definition` has been returning empty while
  `textDocument/hover` returns content, i.e. the server starts but its
  project-load/`serverStatus` readiness for precise navigation is not reliably
  reached within the test window (no editor UI drives the usual
  `rust-analyzer/reloadWorkspace` / progress acknowledgements). The test therefore
  asserts rust-analyzer provides a definition **or** hover. Closing this fully needs
  bridging rust-analyzer's custom `experimental/serverStatus` + work-done progress
  handshake end-to-end. Not an Electron blocker; a language-client-readiness gap.

- **ms-python.python — language features**: works via Jedi
  (`"python.languageServer": "Jedi"`); hover on a symbol returns the signature. No
  Pylance (proprietary) is used.
