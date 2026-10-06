//! Types for the custom `ide/*` protocol surfaced by the shared VS Code
//! extension host over each per-workspace LSP socket.
//!
//! The shared host (`exthost/`) speaks standard LSP plus a small set of custom
//! `ide/*` notifications/requests that expose VS Code UI contributions
//! (status-bar items, command palette entries, output, inline decorations and
//! native webviews) to the native IDE. The proxy parses the JSON these methods
//! carry into the structs below and forwards them to the UI as
//! [`crate::core::CoreNotification`] variants.
//!
//! Field names mirror the host's camelCase JSON. Webview `handle`s are opaque
//! **strings** minted by the host (e.g. `view:claude:1700000000000`).

use lsp_types::{Range, Url};
use serde::{Deserialize, Serialize};

/// Method strings used by the `ide/*` protocol.
pub mod method {
    pub const STATUS_BAR_SET: &str = "ide/statusBar/set";
    pub const STATUS_BAR_REMOVE: &str = "ide/statusBar/remove";
    pub const COMMANDS_CHANGED: &str = "ide/commands/changed";
    pub const OUTPUT_APPEND: &str = "ide/output/append";
    pub const VIEWS_REGISTER: &str = "ide/views/register";
    pub const WEBVIEW_CREATE: &str = "ide/webview/create";
    pub const WEBVIEW_SET_HTML: &str = "ide/webview/setHtml";
    pub const WEBVIEW_SET_TITLE: &str = "ide/webview/setTitle";
    pub const WEBVIEW_POST_MESSAGE: &str = "ide/webview/postMessage";
    pub const WEBVIEW_DISPOSE: &str = "ide/webview/dispose";
    pub const DECORATIONS_SET: &str = "ide/decorations/set";

    // Client -> server.
    pub const WEBVIEW_ON_MESSAGE: &str = "ide/webview/onMessage";
    pub const WEBVIEW_RESOLVE_VIEW: &str = "ide/webview/resolveView";
    pub const COMMANDS_LIST: &str = "ide/commands/list";
    pub const EXECUTE_COMMAND: &str = "workspace/executeCommand";
}

/// A status-bar contribution (`ide/statusBar/set`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusBarItem {
    pub id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tooltip: Option<String>,
    /// Command id executed (via `workspace/executeCommand`) when clicked.
    #[serde(default)]
    pub command: Option<String>,
    /// `"left"` or `"right"`.
    #[serde(default)]
    pub alignment: Option<String>,
    #[serde(default)]
    pub priority: Option<f64>,
}

impl StatusBarItem {
    pub fn is_left(&self) -> bool {
        self.alignment.as_deref() != Some("right")
    }
}

/// A contributed command (`ide/commands/changed`, `ide/commands/list`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtCommand {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandsChangedParams {
    pub commands: Vec<ExtCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandsListResult {
    #[serde(default)]
    pub commands: Vec<ExtCommand>,
}

/// An appended chunk of output-channel text (`ide/output/append`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputAppendParams {
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub text: String,
}

/// A registered view container (`ide/views/register`). `kind` is `"tree"` or
/// `"webview"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtView {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub container: String,
    #[serde(default)]
    pub kind: String,
}

impl ExtView {
    pub fn is_webview(&self) -> bool {
        self.kind == "webview"
    }
}

/// Options carried by `ide/webview/create`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebviewOptions {
    #[serde(default, rename = "enableScripts")]
    pub enable_scripts: bool,
    #[serde(default, rename = "localResourceRoots")]
    pub local_resource_roots: Vec<String>,
}

/// A native webview to host (`ide/webview/create`). `kind` is `"view"` (sidebar
/// view container) or `"panel"` (editor-area panel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebviewCreate {
    pub handle: String,
    #[serde(default, rename = "viewType")]
    pub view_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub html: String,
    #[serde(default)]
    pub options: WebviewOptions,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebviewSetHtml {
    pub handle: String,
    #[serde(default)]
    pub html: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebviewSetTitle {
    pub handle: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebviewPostMessage {
    pub handle: String,
    #[serde(default)]
    pub message: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebviewDispose {
    pub handle: String,
}

/// Client -> server: a message posted from the native webview back to the
/// extension (`ide/webview/onMessage`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebviewOnMessage {
    pub handle: String,
    pub message: serde_json::Value,
}

/// Client -> server: resolve a registered webview view, booting its provider
/// (`ide/webview/resolveView`). Replies with [`ResolveViewResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveViewParams {
    #[serde(rename = "viewId")]
    pub view_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveViewResult {
    pub handle: String,
}

/// `after` render option of an inline decoration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecorationAfter {
    #[serde(default, rename = "contentText")]
    pub content_text: String,
}

/// A single inline decoration (GitLens current-line blame etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decoration {
    pub range: Range,
    #[serde(default)]
    pub after: Option<DecorationAfter>,
    #[serde(default, rename = "hoverMessage")]
    pub hover_message: Option<String>,
}

/// Inline decorations to display for a document (`ide/decorations/set`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecorationsSet {
    pub uri: Url,
    #[serde(default)]
    pub decorations: Vec<Decoration>,
}

/// Arguments for `workspace/executeCommand`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteCommandParams {
    pub command: String,
    #[serde(default)]
    pub arguments: Vec<serde_json::Value>,
}
