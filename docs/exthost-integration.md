# Shared extension host ⇄ native IDE integration

How the native IDE (Rust/floem) hosts the shared VS Code extension host
(`exthost/`), opens a workspace per window, attaches the per-workspace LSP
bridge, and renders the host's `ide/*` UI contributions. See also
`exthost/MEMORY.md`, `ide/DEFERRED.md`, and `ide-webview/NOTES.md`.

## Lifecycle (one host per IDE process)

`lapce-proxy/src/exthost.rs` owns a process-global [`ExtHost`] manager. The
first window that initializes a local workspace lazily spawns
`node exthost/dist/host.js` and connects to its control socket
(Content-Length framed JSON-RPC 2.0). Resolution order:

| what | env override | default |
| --- | --- | --- |
| node | `IDE_NODE` | `node` on `PATH` |
| host.js | `IDE_EXTHOST` | `exthost/dist/host.js` next to the exe, else repo-relative |
| extensions dir | `IDE_EXTENSIONS_DIR` | `<data-local>/extensions` |
| host data dir | `IDE_EXTHOST_DATA` | `<data-local>/exthost` |
| host HOME | `IDE_EXTHOST_HOME` | = host data dir |

`<data-local>` honors `IDE_DATA_DIR`. If node or `host.js` is missing the IDE
logs once and runs **without** extensions. The host (and its language-server
children) is killed on `AppEvent::WillTerminate`
(`lapce_proxy::exthost::shutdown()`), and each window closes its workspace on
`Shutdown`.

## Per-window wiring

On `ProxyNotification::Initialize` the `Dispatcher` (off-thread, so node boot
never blocks the proxy loop):

1. `exthost::open_workspace([window folder])` → `{workspaceId, lspSocket}`.
2. `proxy.attach_lsp_server("exthost", lspSocket, [], [])` — empty
   languages/extensions means **match all documents**
   (`LspServerConfig::matches`/`document_selector` catch-all), so every file in
   the window is routed to the host for its extensions to triage.

The attached socket is a standard LSP server, so diagnostics, hover, definition,
formatting, etc. flow through the **existing** proxy LSP client unchanged.

## `ide/*` protocol → UI

Custom notifications the host sends over that socket are parsed in
`lapce-proxy/src/plugin/psp.rs` (`handle_ide_notification`) into
`lapce_rpc::ide_ext` types and forwarded as new `CoreNotification::Ide*`
variants. The app (`lapce-app/src/window_tab.rs::handle_core_notification`)
applies them to per-window reactive state (`lapce-app/src/ide_ext.rs`,
`WindowTabData::ide_ext`). Render surfaces:

| contribution | method(s) | render |
| --- | --- | --- |
| status-bar items | `ide/statusBar/set` / `remove` | `status.rs` — left/right items, clickable → `workspace/executeCommand` |
| commands | `ide/commands/changed`, `ide/commands/list` | command palette (`PaletteKind::ExtensionCommand`, workbench cmd *Show Extension Commands*) → `workspace/executeCommand` |
| inline decorations | `ide/decorations/set` | `doc.rs` phantom text — end-of-line "after" text (GitLens current-line blame) |
| webviews | `ide/webview/create` / `setHtml` / `postMessage` / `dispose`, `ide/webview/resolveView` | `webview_view.rs::ext_webview_panel` — native `WKWebView` dock; both-way `postMessage` via `ide/webview/onMessage` |
| views | `ide/views/register` | webview views auto-resolved into the dock |

Client→host requests (`workspace/executeCommand`, `ide/commands/list`,
`ide/webview/resolveView`) go through `ProxyRequest::ExtHostRequest`, routed to
the attached `exthost` server by name
(`PluginCatalog::handle_exthost_request`). `ide/webview/onMessage` goes through
`ProxyNotification::ExtHostNotification`.

### Webview parent handle

The pinned floem rev (`31fa8f4`) does not expose a window's raw handle, so
`ext_webview_panel` obtains the floem window's content `NSView` through AppKit
(`objc2`/`objc2-app-kit`, main thread) and passes it to `ide-webview`'s
`build_as_child`. The `webview` cargo feature is enabled by default for the
`ide` binary on macOS (root `Cargo.toml`). Webview handles are **strings**
(host-minted, e.g. `view:claudeVSCodeSidebar:...`); the controller keys native
views by a deterministic hash of the handle.

## Test

`lapce-proxy/tests/exthost_integration.rs` drives the real `exthost/dist`
through the same `Dispatcher` path windows use (`common::Harness`), after
`exthost/test/prepare-fixtures.mjs` fetches the 6 extensions (reusing
`extensions.mjs`) + builds a git/TS fixture (reusing `fixtures.mjs`) into
`/tmp`. It asserts: an ESLint `PublishDiagnostics` reaches core; an
`ide/statusBar/set` is parsed; `ide/commands/list` is non-empty; Claude Code's
view resolves to an `ide/webview/create` with non-empty HTML. Last run:
**869 commands, 2 ESLint diagnostics, ESLint status item, Claude webview 2626
bytes HTML** — PASS. Builds `exthost/dist` automatically if missing; only skips
(with a message) when `node` is absent.

Run: `CARGO_TARGET_DIR=/tmp/t cargo test -p lapce-proxy --test exthost_integration`.

## Smoke

`cargo build --release` → `release/ide <git-ts-fixture>`; screenshots in
`/tmp/ide-smoke/`. Captured via `screencapture -l<windowid>` (full-screen
`screencapture -x` returns black in this headless-display session; the
window-list capture path works). Observed:

- GitLens current-line blame rendered inline after `const x = 1;`:
  *"IDE Smoke, 2 years ago • smoke commit"* (our `ide/decorations/set` phantom
  text).
- Extension status-bar items **Claude Code**, **Prettier**, **ESLint** in the
  status bar; **TypeScript / LF / Ln,Col** native items alongside.
- The **Extension** webview dock mounted on the right (Claude's view
  auto-resolved).

`screencapture` does not composite the `WKWebView`'s separate GPU surface, so
the webview region captures as blank in this session; the HTML delivery itself
is proven by the integration test (2626 bytes).

## Known gaps

- **`ide/output/append` is not emitted by the host** (confirmed by the exthost
  owner): `OutputChannel.append()` text goes through the stubbed spdlog file
  logger and never reaches the main thread as a payload. `IdeExtData.output` /
  `append_output` are plumbed for when/if a capturing logger lands; today
  extension log output continues to surface via `LogMessage` in the logs. No
  dedicated output *panel view* is wired. Documented in `ide/DEFERRED.md`.
- The webview dock binds to the key/main window's `NSView`; with multiple
  windows a webview created while a given window is focused attaches to that
  window. Per-`WindowId` NSView mapping would need a floem bump that exposes the
  raw handle.
