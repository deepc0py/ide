//! Native host for VS Code extension webviews.
//!
//! On macOS a `WKWebView` (via `wry`) is embedded as a child of an existing
//! floem/winit window (identified by a [`raw_window_handle::RawWindowHandle`]),
//! so every window in the IDE process shares one address space instead of
//! spawning an Electron renderer per webview.
//!
//! The platform-neutral pieces — resource-URL rewriting/enforcement
//! ([`uri`]) and HTML/shim preparation ([`shim`]) — are pure and unit tested
//! on every platform. The windowed host ([`WebviewHost`]/[`Handle`]) is macOS
//! only.

use std::collections::BTreeMap;
use std::path::PathBuf;

pub mod shim;
pub mod theme;
mod theme_data;
pub mod uri;

pub use shim::ThemeKind;

/// Re-exported so embedders can name [`raw_window_handle::RawWindowHandle`]
/// without taking their own dependency on the crate.
pub use raw_window_handle;

#[cfg(target_os = "macos")]
mod host;
#[cfg(target_os = "macos")]
pub use host::{Handle, WebviewHost};

/// Logical bounds of a webview within its parent window, in the window's
/// logical (CSS-like) pixels with the origin at the window's top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WebviewBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl WebviewBounds {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Options for creating a webview, mirroring the subset of VS Code's
/// `WebviewOptions` that the native host honors.
#[derive(Debug, Clone)]
pub struct WebviewOptions {
    /// Whether the extension's scripts run (and the `acquireVsCodeApi` shim is
    /// installed). When `false`, only resource URLs and theme CSS are injected.
    pub enable_scripts: bool,
    /// Absolute directories the webview may load resources from. A resource URL
    /// resolving outside all of these is rejected (403).
    pub local_resource_roots: Vec<PathBuf>,
    /// Webview CSS custom properties keyed by the *full* property name
    /// (including the leading `--`); build with
    /// [`theme::webview_theme_vars`].
    pub theme_vars: BTreeMap<String, String>,
    /// The theme kind, surfaced as the `<body>` class and `data-vscode-theme-kind`.
    pub theme_kind: ThemeKind,
    /// Human-readable theme label (`data-vscode-theme-name`).
    pub theme_name: String,
    /// Theme settings id (`data-vscode-theme-id`).
    pub theme_id: String,
}

impl Default for WebviewOptions {
    fn default() -> Self {
        Self {
            enable_scripts: true,
            local_resource_roots: Vec::new(),
            theme_vars: BTreeMap::new(),
            theme_kind: ThemeKind::default(),
            theme_name: String::new(),
            theme_id: String::new(),
        }
    }
}

/// Errors from the native webview host.
#[derive(Debug, thiserror::Error)]
pub enum WebviewError {
    #[error("no native window handle available for the parent window")]
    NoWindowHandle,
    #[error("resource error: {0}")]
    Resolve(#[from] uri::ResolveError),
    #[error("webview backend error: {0}")]
    Backend(String),
}
