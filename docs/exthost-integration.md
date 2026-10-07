# Shared extension host ⇄ native IDE integration

How the native IDE (Rust/floem) hosts the shared VS Code extension host
(`exthost/`), opens a workspace per window, attaches the per-workspace LSP
bridge, and renders the host's `ide/*` UI contributions. See also
`exthost/MEMORY.md`, `ide/DEFERRED.md`, and `ide-webview/NOTES.md`.

## Lifecycle (one host per IDE process)

`lapce-proxy/src/exthost.rs` owns a process-global [`ExtHost`] manager. The
shared host is **opt-in**: it is only spawned when `IDE_EXTHOST_ENABLE` is set
to a truthy value (`exthost::enabled()`). The `ide` binary sets it on by default
at startup (`lapce-app/src/app.rs::launch`); the exthost integration test sets
it explicitly. Other proxy tests (e.g. `lsp_servers`) therefore never spawn
`node`, which is what let go-to-definition regress and the test suite hang.

When enabled, the first window that initializes a local workspace lazily spawns
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
logs once and runs **without** extensions.

### Guaranteed cleanup (no orphaned `node`)

The host is spawned in **its own process group** (`process_group(0)`) and is
told the proxy's pid via `--parent-pid`. Three independent mechanisms ensure it
never outlives the IDE/test process (which previously hung `cargo test` for
>40min by keeping an inherited stdout pipe open):

1. `host.ts` runs a watchdog (`process.kill(parentPid, 0)` once a second); when
   the parent is gone it `SIGKILL`s its whole process group and exits.
2. `ExtHost`'s `Drop` `SIGKILL`s the process group (host + language servers).
3. `AppEvent::WillTerminate` / window `Shutdown` call
   `lapce_proxy::exthost::shutdown()` / `close_workspace`.

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

### Multi-server request fan-out

With the catch-all host attached, a capability (definition, hover, formatting,
…) can be served by *both* a built-in server (typescript-language-server,
rust-analyzer) and the host. `PluginCatalogRpcHandler::send_request_to_all_plugins`
(`lapce-proxy/src/plugin/mod.rs`) now delivers the first **non-empty** success as
soon as it arrives and only falls back to an empty success / error once every
server has answered (`value_is_empty`). This stops a fast server that supports
the method but returns `[]`/`null` (the host does this for files its extensions
don't handle) from shadowing or blocking the real answer — the regression that
made `lsp_servers typescript` time out for 120s. Regression test:
`lapce-proxy/tests/lsp_aggregation.rs`.

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

### Extension webview dock (hidden by default)

`ide_ext.dock_visible` gates the dock; it is **false by default**, so creating /
auto-resolving a webview no longer forces a half-window empty dock open. The
user reveals it via (a) the *Toggle Extension Webview* workbench/palette command
(`LapceWorkbenchCommand::ToggleExtensionWebview`), or (b) clicking a status-bar
item whose extension registered a webview view (`webview_for_status_item` maps a
"Claude Code" click to Claude's view). `set_html`/visibility reconcile against
`dock_visible`.

### Per-window webview parenting

The pinned floem rev (`31fa8f4`) does not expose a window's raw handle, so
`parent_window_handle(window_id)` resolves the owning window's content `NSView`
through AppKit (`objc2`/`objc2-app-kit`, main thread): it prefers the `NSWindow`
recorded when that floem window was focused (`register_focused_window` on
`WindowGotFocus`), else the real `NSApp` window at the floem window's **creation
ordinal** (`register_window_ordinal`, sorted by `windowNumber`) — this makes
multi-window parenting correct even when the app is never focused (headless), and
lets each window host its own webview(s). The `webview` cargo feature is enabled
by default for the `ide` binary on macOS (root `Cargo.toml`). Webview handles are
**strings** (host-minted, e.g. `view:claudeVSCodeSidebar:...`); the controller
keys native views by a deterministic hash of the handle.

### Late-joining windows (contribution replay)

Extensions contribute status-bar items, commands, views and webviews **once**
into the shared isolate and the hub broadcasts them live. A window that opens
*after* those contributions were made (a second/third `ide <folder>` once Claude
has activated) would otherwise never see them. The hub therefore retains current
contribution state (`Session.statusBarItems` / `views` / `commands` /
`decorations`) and `Session.replayTo(ws)` replays it to each new window the
moment `host/openWorkspace` registers it (`worker.ts`); the buffered notifications
flush when that window's LSP client attaches, so ordering versus live events is
preserved.

Webview **views** are resolved **per window**: each window auto-resolves the
registered view (`ide/webview/resolveView`), minting a handle unique to that
window (`view:<id>:<ws>:<ts>:<n>`). `Session.webviewOwners` records the resolving
window so the resulting `ide/webview/create` / `setHtml` / `postMessage` /
`dispose` route **only** to that window (`mainshim.ts::emitWebview`) instead of
broadcasting — so e.g. Claude's sidebar in window B is its own live instance and
never leaks into window A. Panels (no single owner) still broadcast.

### Webview render verification

`WebviewController::evaluate` (→ `ide-webview` `Handle::evaluate`, wry
`evaluate_script_with_callback`) runs JS inside the live `WKWebView`. With
`IDE_WEBVIEW_SELFTEST=1` the dock auto-opens and the DOM is probed (node count /
`document.body.innerText`) and logged, proving rendering from outside the
WKWebView's separate GPU surface (which `screencapture` cannot composite).

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
(with a message) when `node` is absent. It then opens a **second** window on the
same shared host and asserts the late-join replay: that window receives the
replayed `ide/statusBar/set`, a non-empty `ide/commands/list` and the Claude
`ide/views/register`, and resolves its **own** webview (a handle distinct from
window A's) with non-empty HTML.

Run: `CARGO_TARGET_DIR=/tmp/t cargo test -p lapce-proxy --test exthost_integration`.

The exthost unit suite (`cd exthost && npm test`) adds a `late-joining window`
test: after the primary window has activated, it opens a new window and asserts
the replayed status bar / commands / views arrive on its socket and that it
resolves its own distinct Claude webview (2624-byte HTML) routed only to it.

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

## Smoke2 (regression fixes verified)

Run `/tmp/ide-smoke2`, `IDE_WEBVIEW_SELFTEST=1`, screenshots in
`/tmp/ide-smoke2/shots/`:

- **Dock hidden by default** — `final-nodock.png`: the editor spans the full
  width; no empty "Extension" dock.
- **Status bar** — non-overlapping; items flow with `flex_shrink(0)`.
- **Claude webview renders** — JS probe inside the live WKWebView:
  `{nodes:31, scripts:3, textLen:378, text:"Claude Code can be used with your
  Claude subscription … How do you want to log in? Claude.ai Subscription …"}`.
  The fix was host-side: `mainshim.ts` now forwards each webview's
  `enableScripts` / `localResourceRoots` (`$setOptions` + create/panel payloads)
  instead of `options: {}`; previously scripts were disabled and every resource
  403'd, so the React bundle never ran.
- **Per-window parenting** — `parent_window_handle` picks the owning window by
  creation ordinal, so a webview attaches to its own window without needing the
  app focused.

## Smoke3 (late-join, live multi-window)


Run `/tmp/ide-smoke3/run4.sh`, `IDE_WEBVIEW_SELFTEST=1`: three fixture worktrees
(`A/B/C-tsproj`, each a git repo with an ESLint error) opened **sequentially**
as three windows in one shared-host process (`ide A`; then `ide B`, `ide C` —
late joins). Per-window window-list captures in `/tmp/ide-smoke3/shots/`:

- **ESLint error counter + squiggle** — `win-A.png`: `src/index.ts` open with red
  squiggles under `const unused_A = 1;` and `debugger;`, status-bar error counter
  **⊗ 2**, and GitLens inline blame *"IDE Smoke3, 2 years ago"*. Diagnostics are
  URI-routed, so each window's counter reflects its own file.
- **Status items, no duplicates/overlap** — each window shows a single **Claude
  Code**, **Prettier**, **ESLint** item plus native **TypeScript / LF / Ln,Col**;
  nothing overlaps (`flex_shrink(0)` + `status_items_split` dedup by render key).
- **Own Claude webview per window** — the selftest JS probe runs inside each
  window's live WKWebView and logs distinct per-window handles
  (`view:claudeVSCodeSidebar:ws-1/2/3:…`) each rendering the real React UI
  (`{nodes:31, scripts:3, textLen:378, text:"Claude Code can be used with your
  Claude subscription … How do you want to log in? Claude.ai Subscription …"}`).
  This is the late-join fix end-to-end: B and C opened after A, received the
  replayed view registration, and auto-resolved their **own** webview instances.

As in Smoke/Smoke2 the WKWebView's separate GPU surface is not composited by
`screencapture`, so the dock region captures blank; content is proven by the
per-window selftest probe above and the integration/late-join tests.

## Known gaps

- **`ide/output/append` is not emitted by the host** — see `ide/DEFERRED.md`.
- *(fixed)* **Late-joining windows** now receive a full replay of current
  contributions and resolve their own per-window webviews — see *Late-joining
  windows (contribution replay)* above, the `exthost_integration` second-window
  assertion, the exthost `late-joining window` test, and Smoke3.
- *(fixed)* **ESLint error counter** renders in the live UI (Smoke3 `win-A.png`
  shows **⊗ 2** + squiggles). The publish→`main_split.diagnostics`→counter path
  needs only a document open in the window; it was previously read before ESLint
  had published.
