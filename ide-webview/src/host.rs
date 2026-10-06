//! macOS `WKWebView` host (via `wry`), embedded as a child view of an existing
//! floem/winit window.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, WindowHandle};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::http::{
    Request, Response,
    header::{ACCESS_CONTROL_ALLOW_ORIGIN, CONTENT_TYPE},
};
use wry::{Rect, WebView, WebViewBuilder};

use crate::shim::{HtmlPrep, ThemeKind, prepare_html};
use crate::uri::{RESOURCE_SCHEME, ResolveError, content_type_for, resolve_resource_path};
use crate::{WebviewBounds, WebviewError, WebviewOptions};

/// Wraps a borrowed [`RawWindowHandle`] so `wry` can attach a child webview to
/// it via `build_as_child`.
struct ParentHandle(RawWindowHandle);

impl HasWindowHandle for ParentHandle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: the caller guarantees the parent window outlives the webview
        // (the host embeds the webview as a child of that window and disposes
        // it before the window is destroyed).
        Ok(unsafe { WindowHandle::borrow_raw(self.0) })
    }
}

/// Factory for native extension webviews.
pub struct WebviewHost;

/// A live native webview embedded in a parent window. Dropping the handle (or
/// calling [`Handle::dispose`]) removes the webview from the window.
///
/// Not `Send`/`Sync`: all methods must be called on the UI (main) thread.
pub struct Handle {
    webview: WebView,
    #[allow(dead_code)]
    roots: Arc<Vec<PathBuf>>,
    enable_scripts: bool,
    theme_vars: BTreeMap<String, String>,
    theme_kind: ThemeKind,
    /// Latest JSON state reported by `setState`, re-injected across `set_html`.
    state: Rc<RefCell<Option<String>>>,
}

impl WebviewHost {
    /// Create a native webview as a child of `parent`, positioned at `bounds`.
    ///
    /// `on_message` is invoked on the UI thread with the JSON value the page
    /// passed to `acquireVsCodeApi().postMessage(...)`.
    pub fn create(
        parent: RawWindowHandle,
        bounds: WebviewBounds,
        html: &str,
        options: WebviewOptions,
        on_message: impl Fn(serde_json::Value) + 'static,
    ) -> Result<Handle, WebviewError> {
        let roots = Arc::new(options.local_resource_roots.clone());
        let state: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        let prepared = prepare_html(
            html,
            &HtmlPrep {
                enable_scripts: options.enable_scripts,
                theme_vars: options.theme_vars.clone(),
                theme_kind: options.theme_kind,
                initial_state: None,
            },
        );

        // Custom protocol: serve only files under the resource roots.
        let proto_roots = roots.clone();
        let protocol = move |_id: &str, request: Request<Vec<u8>>| -> Response<Cow<'static, [u8]>> {
            serve_resource(&request.uri().to_string(), &proto_roots)
        };

        // IPC: route `acquireVsCodeApi` traffic back to the host.
        let on_message = Rc::new(on_message);
        let ipc_state = state.clone();
        let ipc = move |request: Request<String>| {
            dispatch_ipc(request.body(), &ipc_state, on_message.as_ref());
        };

        let parent = ParentHandle(parent);

        let mut builder = WebViewBuilder::new()
            .with_bounds(to_rect(bounds))
            .with_custom_protocol(RESOURCE_SCHEME.to_string(), protocol)
            .with_ipc_handler(ipc)
            .with_html(prepared);
        if !options.enable_scripts {
            builder = builder.with_javascript_disabled();
        }

        let webview = builder
            .build_as_child(&parent)
            .map_err(|e| WebviewError::Backend(e.to_string()))?;

        Ok(Handle {
            webview,
            roots,
            enable_scripts: options.enable_scripts,
            theme_vars: options.theme_vars,
            theme_kind: options.theme_kind,
            state,
        })
    }
}

impl Handle {
    /// Replace the webview's HTML, re-injecting the shim, theme, and persisted
    /// `getState` value.
    pub fn set_html(&self, html: &str) -> Result<(), WebviewError> {
        let prepared = prepare_html(
            html,
            &HtmlPrep {
                enable_scripts: self.enable_scripts,
                theme_vars: self.theme_vars.clone(),
                theme_kind: self.theme_kind,
                initial_state: self.state.borrow().clone(),
            },
        );
        self.webview
            .load_html(&prepared)
            .map_err(|e| WebviewError::Backend(e.to_string()))
    }

    /// Deliver a message to the page (`window.addEventListener('message', ...)`).
    pub fn post_message(&self, message: &serde_json::Value) -> Result<(), WebviewError> {
        let json = serde_json::to_string(message).unwrap_or_else(|_| "null".to_string());
        let script = format!(
            "(function(){{var d={json};\
             window.dispatchEvent(new MessageEvent('message',{{data:d}}));}})();"
        );
        self.webview
            .evaluate_script(&script)
            .map_err(|e| WebviewError::Backend(e.to_string()))
    }

    /// Reposition/resize the webview within its parent window.
    pub fn set_bounds(&self, bounds: WebviewBounds) -> Result<(), WebviewError> {
        self.webview
            .set_bounds(to_rect(bounds))
            .map_err(|e| WebviewError::Backend(e.to_string()))
    }

    /// Show or hide the webview.
    pub fn set_visible(&self, visible: bool) -> Result<(), WebviewError> {
        self.webview
            .set_visible(visible)
            .map_err(|e| WebviewError::Backend(e.to_string()))
    }

    /// Dispose the webview, removing it from its parent window.
    pub fn dispose(self) {
        // Dropping `WebView` detaches and releases the native view.
        drop(self);
    }
}

/// Parse and route an IPC payload produced by the injected shim.
fn dispatch_ipc(
    body: &str,
    state: &Rc<RefCell<Option<String>>>,
    on_message: &dyn Fn(serde_json::Value),
) {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => match value.get("__ide").and_then(|v| v.as_str()) {
            Some("state") => {
                let s = value
                    .get("state")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                *state.borrow_mut() = Some(s.to_string());
            }
            Some("webview") => {
                let msg = value
                    .get("message")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                on_message(msg);
            }
            // Unknown envelope: forward verbatim.
            _ => on_message(value),
        },
        // Not JSON: forward the raw string.
        Err(_) => on_message(serde_json::Value::String(body.to_string())),
    }
}

/// Serve a resource URL, enforcing the resource roots.
fn serve_resource(uri: &str, roots: &[PathBuf]) -> Response<Cow<'static, [u8]>> {
    match resolve_resource_path(uri, roots) {
        Ok(path) => match std::fs::read(&path) {
            Ok(bytes) => Response::builder()
                .status(200)
                .header(CONTENT_TYPE, content_type_for(&path))
                .header(ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Cow::Owned(bytes))
                .expect("valid response"),
            Err(e) => error_response(404, format!("read error: {e}")),
        },
        Err(ResolveError::OutsideRoots(_)) => {
            error_response(403, "forbidden: outside resource roots".to_string())
        }
        Err(e) => error_response(404, e.to_string()),
    }
}

fn error_response(status: u16, message: String) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Cow::Owned(message.into_bytes()))
        .expect("valid error response")
}

fn to_rect(b: WebviewBounds) -> Rect {
    Rect {
        position: LogicalPosition::new(b.x, b.y).into(),
        size: LogicalSize::new(b.width, b.height).into(),
    }
}
