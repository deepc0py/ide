# ide-webview — native WKWebView host for VS Code extension webviews

Fork slice: `WebviewHost-2`.

## What this crate does
Embeds a `WKWebView` (via [`wry`] `build_as_child`) as a child view of an
existing floem/winit window, so every window + webview in the IDE shares one
OS process. Presents VS Code's webview contract natively:

- `WebviewHost::create(parent: RawWindowHandle, bounds, html, options, on_message) -> Handle`
- `Handle::{set_html, post_message, set_bounds, set_visible, dispose}`

## Key design decisions

### Resource scheme (`https` can't be intercepted)
`WKWebView` cannot install a `WKURLSchemeHandler` for the builtin `https`
scheme, so we cannot serve `https://file+.vscode-resource.vscode-cdn.net/...`
directly. Instead every resource URL is **rewritten** to the custom scheme
`vscode-resource://file/<abs path>` (which wry *can* intercept), handling:
1. the CSP wildcard `https://*.vscode-resource.vscode-cdn.net` → `vscode-resource:`,
2. legacy `vscode-webview-resource:`,
3. concrete CDN hosts `https://<scheme>+<auth>.vscode-resource.vscode-cdn.net`,
4. legacy opaque `vscode-resource:/<path>`.

The authority (`file+<auth>`) is collapsed to `file`; the absolute filesystem
path lives in the URL path. Because the page's CSP `cspSource` references are
rewritten to `vscode-resource:` too, extension CSP meta tags keep working.

### Resource-root enforcement
`uri::resolve_resource_path` percent-decodes, lexically normalizes (`..`/`.`),
checks containment against **canonicalized** roots, then canonicalizes the
target (defeating symlink escape) and re-checks. `/tmp` → `/private/tmp`
symlinks are handled by the canonicalizing fallback. Out-of-root → HTTP 403.

### `acquireVsCodeApi()` shim
Injected at the top of `<head>` (`shim.rs`), modelled on
`vendor/vscode/.../webview/browser/pre/index.html`. Provides
`postMessage` / `getState` / `setState`, routed to Rust via
`window.ipc.postMessage(JSON)`; host→page delivery is a `MessageEvent` dispatch
via `evaluate_script`. `--vscode-*` theme variables are injected as a `:root`
`<style>` and the theme-kind body class is applied, so React UIs render.
State from `setState` is persisted in the `Handle` and re-injected across
`set_html`.

## lapce-app wiring (`webview` feature, macOS only)
`lapce-app/src/webview_view.rs` exposes:
- `WebviewController` — owns the live `Handle`s for a window and positions each
  one to track its placeholder.
- `webview_placeholder(controller, id) -> impl View` — a floem `empty()` view
  that reports window-relative bounds (`on_move` = absolute origin, `on_resize`
  = size) and visibility (`on_cleanup`) to the controller.

Gated behind `--features webview` (adds the optional `ide-webview` dep) so the
default lapce-app build is unaffected. The integration owner connects the
controller to the `ide/webview/*` protocol and supplies the parent
`RawHandle` (e.g. via floem's `WindowIdExt::with_window_handle`, available in
newer floem revs).

## Tests / verification
- `CARGO_TARGET_DIR=/tmp/ide-target-WebviewHost cargo test -p ide-webview`
  → 21 unit tests pass (URI rewrite, resource-root enforcement incl. traversal
  + sibling-prefix, content types, shim/theme injection).
- `CARGO_TARGET_DIR=/tmp/ide-target-WebviewHost cargo run -p ide-webview --example demo`
  → opens a window, loads local HTML referencing a CSS file via the
  `vscode-resource` CDN URI, performs a JS→Rust `postMessage` round trip,
  confirms the resource loaded (body background `rgb(18,52,86)` == `#123456`)
  and `setState`/`getState`, writes a screenshot, exits 0.

## Known gaps
- Host is macOS only (`#[cfg(target_os = "macos")]`); other platforms compile
  the pure `uri`/`shim` modules only.
- `local lapce-app` integration is a seam; `cargo check -p lapce-app --features
  webview` should be run by the integration owner (skipped here to avoid
  storming concurrent sibling builds; all floem/ide-webview APIs used were
  verified against the pinned floem rev `31fa8f4`).
