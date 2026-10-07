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

1. `exthost::open_workspace([window folder])` → `{workspaceId, lspSocket,
   providedLanguages}`.
2. `proxy.attach_lsp_server("exthost", lspSocket, [], [], providedLanguages)` —
   empty
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

### Dedup: one language server per host, not per window

The host would otherwise run a *second* rust-analyzer — the native IDE ships a
built-in rust-analyzer (`lapce-proxy/src/plugin/lsp_config.rs`
`default_lsp_servers`) that `maybe_start_lsp` spawns per workspace when a `.rs`
file opens, **and** the `rust-lang.rust-analyzer` extension spawns its own inside
the shared host. Two rust-analyzers per window is the opposite of the goal.

So the host advertises, in its `host/openWorkspace` reply, the languages for
which an installed extension provides a *full* language server
(`providedLanguages`; `exthost/src/host.ts` `LANGUAGE_SERVER_EXTENSIONS` maps
`rust-lang.rust-analyzer → rust`, `ms-python.python → python` — linters/
formatters like eslint/prettier are excluded, they have no definitions). The
proxy threads this list through `AttachLspServer` into
`PluginCatalog::suppressed_languages`; `maybe_start_lsp` then **never** starts a
built-in server for a suppressed language. The host's single rust-analyzer
(shared across all windows/worktrees) is authoritative. TypeScript has no
language-server extension among the six, so the built-in
typescript-language-server still runs — that is how TS definitions/diagnostics
are served. The bridge attaches at window `Initialize`, before any document
opens, so suppression is in place before `maybe_start_lsp` can fire.

### Product default settings (memory)

Shipped defaults make the memory-heavy servers cheap without the user setting
anything. `exthost/src/config.ts` `PRODUCT_DEFAULT_SETTINGS` overlays the
`defaults` configuration layer (below `userLocal`, so any one is overridable in
`settings.json`): `rust-analyzer.cachePriming.enable=false` (lazy indexing),
`rust-analyzer.checkOnSave=false` (no `cargo check` rustc swarm — native
rust-analyzer diagnostics still report type/borrow errors),
`rust-analyzer.cargo.buildScripts.enable=false`,
`rust-analyzer.procMacro.enable=false`, and `python.languageServer=Jedi`. See
`exthost/MEMORY.md` for the measured effect.

## `ide/*` protocol → UI

Custom notifications the host sends over that socket are parsed in
`lapce-proxy/src/plugin/psp.rs` (`handle_ide_notification`) into
`lapce_rpc::ide_ext` types and forwarded as new `CoreNotification::Ide*`
variants. The app (`lapce-app/src/window_tab.rs::handle_core_notification`)
applies them to per-window reactive state (`lapce-app/src/ide_ext.rs`,
`WindowTabData::ide_ext`). Render surfaces:

| contribution | method(s) | render |
| --- | --- | --- |
| status-bar items | `ide/statusBar/set` / `remove` | `status.rs` — left/center(panel toggles)/right regions; each region is `.clip()`-wrapped and ext items are `flex_shrink(0)`, so items that don't fit are truncated/hidden by the clip (native cursor/LF/lang kept) and **never overlap** at any width (incl. 800px); clickable → `workspace/executeCommand` |
| commands | `ide/commands/changed`, `ide/commands/list` | command palette (`PaletteKind::ExtensionCommand`, workbench cmd *Show Extension Commands*) → `workspace/executeCommand` |
| inline decorations | `ide/decorations/set` | `editor/view.rs::paint_ext_decorations` — dimmed end-of-line "after" text (GitLens current-line blame), painted as a viewport-clipped overlay (NOT phantom text), so it stays on the line's last visual row, truncates at the editor's right edge, and never affects real-text wrapping or spills onto an extra visual line |
| webviews | `ide/webview/create` / `setHtml` / `postMessage` / `dispose`, `ide/webview/resolveView` | `webview_view.rs::ext_webview_panel` — native `WKWebView` dock; both-way `postMessage` via `ide/webview/onMessage` |
| views | `ide/views/register` | webview views auto-resolved into the dock |

Client→host requests (`workspace/executeCommand`, `ide/commands/list`,
`ide/webview/resolveView`) go through `ProxyRequest::ExtHostRequest`, routed to
the attached `exthost` server by name
(`PluginCatalog::handle_exthost_request`). `ide/webview/onMessage` goes through
`ProxyNotification::ExtHostNotification`.

### Interactive UI prompts (host → window requests)

Extensions that call `window.showInputBox`, `window.showQuickPick` or a modal
`window.showInformationMessage(…, {modal:true}, …)` (the SonarQube setup
assistant does) need a human answer. The host sends these as **server→client LSP
requests** on the per-window socket and awaits the reply:

| method | params | reply (or `null` on cancel) |
| --- | --- | --- |
| `window/showInputBox` | `{title?, prompt?, placeHolder?, value?, password?}` | `{value}` |
| `window/showQuickPick` | `{title?, placeHolder?, items:[{label, description?, detail?, handle}]}` | `{handle}` |
| `window/showMessageRequest` | `{type, message, modal?, detail?, actions:[{title}]}` | `{title}` |

Host side (`exthost/`): `MainThreadQuickOpen.$input`/`$show`+`$setItems` and the
modal path of `MainThreadMessageService.$showMessage` route through
`Session.requestActive`, which targets the window whose `workspace/executeCommand`
is currently running (`Session.activeWorkspace`, set around `executeCommand` in
`lsp.ts`) so the prompt renders where the user invoked the command. Native side:
`psp.rs::process_request` parses these three methods, registers the pending
`ResponseSender` in `PluginCatalogRpcHandler`, and emits a `CoreNotification::Ide*`
prompt; the app (`lapce-app/src/ide_prompt.rs`) renders a floem overlay (input
box with optional masking, filterable quick pick, modal dialog with buttons) and
replies via `ProxyNotification::IdePromptResponse { id, result }`, which resolves
the host request. Esc / backdrop (non-modal) cancels with `null`; pending prompts
are drained to `null` on window close so host requests never leak. Contributed
command **titles + categories** come from each extension's
`contributes.commands` (`Session.commandMeta`), so the palette shows e.g.
*SonarQube: Set Up Local Server* rather than the raw command id.

### Extension webview dock (hidden by default)

`ide_ext.dock_visible` gates the dock; it is **false by default**, so creating /
auto-resolving a webview no longer forces a half-window empty dock open. The
user reveals it via (a) the *Toggle Extension Webview* workbench/palette command
(`LapceWorkbenchCommand::ToggleExtensionWebview`), or (b) clicking a status-bar
item whose extension registered a webview view (`webview_for_status_item` maps a
"Claude Code" click to Claude's view). `set_html`/visibility reconcile against
`dock_visible`.

### Webview theming (styled like VS Code)

A webview's HTML is prepared by `ide-webview` (`shim::prepare_html`) before it
reaches the `WKWebView`. To render extension React UIs styled instead of raw
white/serif HTML, it injects the three things VS Code's webview harness provides
(`vs/workbench/contrib/webview/browser/{themeing.ts,pre/index.html}`):
1. the **default stylesheet** (`@layer vscode-default`, so extension CSS wins) —
   body/links/`code`/scrollbars from the theme vars; adapted so the page paints
   `--vscode-editor-background` and `<button>` gets `appearance: none` (WebKit
   otherwise draws a light native control for transparent-background buttons
   that Chromium — VS Code's engine — renders themed);
2. the **full `--vscode-*` variable set** under `:root` — every registered color
   + size resolved for the active theme plus the fonts, from the generated
   `ide-webview/src/theme_data.rs` (see `tools/gen_theme_vars.mjs`), mirroring
   `WebviewThemeDataProvider`;
3. the **theme body class + data attributes** (`vscode-dark`/`vscode-light`,
   `data-vscode-theme-kind`/`-name`/`-id`).

`webview_view.rs::webview_theme_options` builds the variable map from the active
theme: the kind is derived from the editor background/foreground (NOT
`color.color_preference`, whose inverted `is_light` reads `Light` for Dark
Modern), and the host's editor font is threaded in. Regression test:
`ide-webview/tests/claude_webview.rs` (Claude's resources all resolve; every
`--vscode-*` the Claude CSS references is themed, bar the set VS Code's Dark
Modern also leaves undefined).

### openExternal (login flows)

An extension that opens a URL — `vscode.env.openExternal(uri)` →
`MainThreadWindow.$openUri`, or `executeCommand('vscode.open', uri)` /
`'vscode.env.openExternal'` → `MainThreadCommands.$executeCommand` — is routed
through `exthost/src/openExternal.ts`, which spawns the OS default-browser opener
(`open`, overridable with `IDE_OPENER`) detached, logs `[ide] openExternal
<url>`, and returns without completing anything. E.g. clicking Claude's
*Claude.ai Subscription* posts to the extension, whose login flow then requests
the external URL open. Tests (`exthost/test/openExternal.test.mjs`) stub the
opener and assert the URL is requested + logged.

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

Because `screencapture`'s window-list capture cannot composite the WKWebView's
GPU surface (the dock region reads blank in a window screenshot even when the
page is live), visibility is also proven by `WebviewController::snapshot` (→
`Handle::snapshot_png`, `WKWebView takeSnapshotWithConfiguration:` → `NSImage` →
PNG). With `IDE_WEBVIEW_SNAPSHOT_DIR=<dir>` the selftest writes a PNG of each
window's live Claude view; a non-blank snapshot also proves the webview's
frame/bounds are non-zero and on-screen (match the dock). Native webviews are
created lazily: `ext_webview_panel`'s reconcile effect only calls
`controller.attach` once `ide_ext.dock_visible` is true, so no hidden WKWebView
surface exists while the dock is closed.

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

## Smoke4 (status-bar non-overlap, blame overlay, webview snapshots)

Run `/tmp/ide-smoke4/s4_status.sh <width> <tag>` and `s4_webview.sh`
(`IDE_WEBVIEW_SELFTEST=1`, `IDE_WEBVIEW_SNAPSHOT_DIR=<dir>`); screenshots in
`/tmp/ide-smoke4/shots/`. The 800px window is the fresh-data-dir default; a
specific width is seeded into `<IDE_DATA_DIR>/db/window` (floem clamps nothing —
the display is 2560pt). Observed:

- **Status bar, no overlap** — `statusbar-800.png`: the diagnostics counter and
  panel-toggle icons no longer collide; the right region shows
  `Prettier ESLint · Ln 1, Col 1, Char 0 · LF · TypeScript` cleanly, with the
  ext items that don't fit (Claude Code, GitLens status) truncated/hidden by the
  region clip rather than drawn over the natives. `statusbar-1400.png`: every
  item fits and is fully visible, still non-overlapping.
- **Inline blame on one line** — line 1 renders
  `// window A marker    IDE Smoke3, 2 years ago • window A …` on a single visual
  line, truncated with `…` at the editor's right edge; there is **no** stray
  ellipsis/extra wrapped line below it (the regression the overlay fix removed).
- **Claude webview rendered per window** — `webview-A/B/C.png` are
  `WKWebView takeSnapshot` PNGs of windows A/B/C's own live views (handles
  `ws-1/2/3`), each showing the real Claude Code login UI ("Welcome to Claude
  Code", "Claude.ai Subscription", …). This proves the dock content renders with
  correct (non-zero, on-screen) bounds in every window, even though the
  window-list `win-B.png` capture shows the dock region blank (GPU surface not
  composited by `screencapture`).

## Smoke5 (webview theming — styled dark UIs)

Run `/tmp/ide-smoke5` with a git+TS fixture, `IDE_WEBVIEW_SELFTEST=1`,
`IDE_WEBVIEW_SNAPSHOT_DIR=/tmp/ide-smoke5/shots`, default theme (Dark Modern).
`WKWebView takeSnapshot` PNGs in `/tmp/ide-smoke5/`:

- **`claude-code.png`** — Claude Code's sidebar webview, now **dark-themed**:
  `--vscode-editor-background` (#1f1f1f) page, `--vscode-editor-foreground`
  (#cccccc) text, the VS Code UI font (sans-serif), a bordered *"Welcome to
  Claude Code"* header, and the three login option buttons
  (*Claude.ai Subscription*, *Anthropic Console*, *Bedrock, Foundry, or Vertex*)
  rendered as VS Code Dark Modern secondary buttons — transparent fill, light
  border + label. Before the fix (Smoke4 `webview-B.png`) this was raw white
  serif HTML with default buttons: the extension CSS loaded but no theme vars /
  default stylesheet were injected.
- **`gitlens.png`** — a GitLens webview (Create Cloud Patch) fully styled: dark
  panel, dark inputs with a blue focus border, the primary *Create Cloud Patch*
  button in VS Code blue (`--vscode-button-background` #0078d4), blue links, and
  checkboxes — proving GitLens webviews pick up the injected theme too.

The selftest DOM probe confirms live content
(`Claude Code can be used with your Claude subscription … Claude.ai Subscription
…`). The all-resources-resolve + full-variable-coverage guarantees are covered
by `ide-webview/tests/claude_webview.rs`; the `openExternal` login path by
`exthost/test/openExternal.test.mjs`.

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
- *(fixed)* **Unstyled webviews** — Claude/GitLens webviews rendered as raw
  white/serif HTML because no theme vars or default stylesheet were injected and
  the theme kind was mis-derived. Now the full VS Code `--vscode-*` set + default
  stylesheet + body classes are injected from the active theme — see *Webview
  theming* above and Smoke5.
