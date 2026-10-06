//! Minimal, well-defined seam for hosting native VS Code extension webviews
//! ([`ide_webview`]) inside a floem panel or side-view.
//!
//! This keeps the floem-side integration intentionally small: a
//! [`webview_placeholder`] view reserves layout space and keeps a
//! [`WebviewController`] informed of its window-relative bounds and visibility,
//! while the controller owns the live native webviews and positions each one to
//! track its placeholder. The integration owner connects the controller to the
//! `ide/webview/*` protocol notifications (create / setHtml / postMessage /
//! dispose) and supplies the parent window's
//! [`RawWindowHandle`](ide_webview::raw_window_handle::RawWindowHandle).
//!
//! Enabled by the `webview` cargo feature (macOS only).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use floem::View;
use floem::peniko::kurbo::{Point, Rect};
use floem::views::{Decorators, empty};

use ide_webview::raw_window_handle::RawWindowHandle;
pub use ide_webview::{
    Handle, ThemeKind, WebviewBounds, WebviewError, WebviewHost, WebviewOptions,
};

/// Identifies a hosted webview; mirrors the handle used by the `ide/webview/*`
/// protocol.
pub type WebviewId = u64;

/// Tracked geometry of a webview's placeholder in window-logical coordinates.
/// `origin` comes from floem's move listener (absolute window position) and
/// `size` from its resize listener.
#[derive(Debug, Clone, Copy, Default)]
struct Placeholder {
    origin: Point,
    size: (f64, f64),
    visible: bool,
}

impl Placeholder {
    fn bounds(&self) -> WebviewBounds {
        WebviewBounds::new(self.origin.x, self.origin.y, self.size.0, self.size.1)
    }
}

/// Owns the live native webviews for one window and keeps each one aligned with
/// its floem placeholder.
///
/// All methods must be called on the UI (main) thread; [`Handle`] is not
/// `Send`/`Sync`.
#[derive(Default)]
pub struct WebviewController {
    webviews: RefCell<HashMap<WebviewId, Handle>>,
    placeholders: RefCell<HashMap<WebviewId, Placeholder>>,
}

impl WebviewController {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// Create (or replace) the native webview for `id`, parented to `parent`.
    /// It is positioned at the latest bounds reported by its placeholder (or a
    /// zero rect if the placeholder has not been laid out yet).
    pub fn attach(
        &self,
        id: WebviewId,
        parent: RawWindowHandle,
        html: &str,
        options: WebviewOptions,
        on_message: impl Fn(serde_json::Value) + 'static,
    ) -> Result<(), WebviewError> {
        let placeholder = self
            .placeholders
            .borrow()
            .get(&id)
            .copied()
            .unwrap_or_default();

        let handle = WebviewHost::create(parent, placeholder.bounds(), html, options, on_message)?;
        let _ = handle.set_visible(placeholder.visible);
        self.webviews.borrow_mut().insert(id, handle);
        Ok(())
    }

    /// Whether a native webview currently exists for `id`.
    pub fn is_attached(&self, id: WebviewId) -> bool {
        self.webviews.borrow().contains_key(&id)
    }

    /// Replace the webview's HTML.
    pub fn set_html(&self, id: WebviewId, html: &str) -> Result<(), WebviewError> {
        match self.webviews.borrow().get(&id) {
            Some(handle) => handle.set_html(html),
            None => Ok(()),
        }
    }

    /// Deliver a message to the page.
    pub fn post_message(
        &self,
        id: WebviewId,
        message: &serde_json::Value,
    ) -> Result<(), WebviewError> {
        match self.webviews.borrow().get(&id) {
            Some(handle) => handle.post_message(message),
            None => Ok(()),
        }
    }

    /// Dispose the native webview for `id`, if any.
    pub fn dispose(&self, id: WebviewId) {
        if let Some(handle) = self.webviews.borrow_mut().remove(&id) {
            handle.dispose();
        }
    }

    /// Update the placeholder's window-relative origin (from floem `on_move`).
    fn set_origin(&self, id: WebviewId, origin: Point) {
        let bounds = {
            let mut map = self.placeholders.borrow_mut();
            let p = map.entry(id).or_default();
            p.origin = origin;
            p.bounds()
        };
        self.apply_bounds(id, bounds);
    }

    /// Update the placeholder's size (from floem `on_resize`).
    fn set_size(&self, id: WebviewId, size: (f64, f64)) {
        let bounds = {
            let mut map = self.placeholders.borrow_mut();
            let p = map.entry(id).or_default();
            p.size = size;
            p.bounds()
        };
        self.apply_bounds(id, bounds);
    }

    /// Update the placeholder's visibility (from floem `on_cleanup`/toggles).
    pub fn set_visible(&self, id: WebviewId, visible: bool) {
        {
            let mut map = self.placeholders.borrow_mut();
            map.entry(id).or_default().visible = visible;
        }
        if let Some(handle) = self.webviews.borrow().get(&id) {
            let _ = handle.set_visible(visible);
        }
    }

    fn apply_bounds(&self, id: WebviewId, bounds: WebviewBounds) {
        if let Some(handle) = self.webviews.borrow().get(&id) {
            let _ = handle.set_bounds(bounds);
        }
    }
}

/// A floem view that reserves space for webview `id` and keeps `controller`
/// informed of its window-relative bounds and visibility, so the native child
/// webview tracks the panel as it resizes, moves, or is removed.
///
/// The view itself paints nothing (the native webview renders on top of it);
/// it only drives layout and geometry reporting.
pub fn webview_placeholder(controller: Rc<WebviewController>, id: WebviewId) -> impl View {
    let on_resize = controller.clone();
    let on_move = controller.clone();
    let on_cleanup = controller.clone();

    empty()
        .style(|s| s.size_full())
        .on_resize(move |rect: Rect| {
            on_resize.set_size(id, (rect.width(), rect.height()));
            on_resize.set_visible(id, true);
        })
        .on_move(move |origin: Point| {
            on_move.set_origin(id, origin);
        })
        .on_cleanup(move || {
            // The placeholder left the view tree: hide the native webview so it
            // doesn't float over unrelated UI. The controller keeps the handle
            // so it can be re-shown if the placeholder returns.
            on_cleanup.set_visible(id, false);
        })
}
