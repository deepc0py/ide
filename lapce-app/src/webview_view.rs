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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::LazyLock;

use parking_lot::Mutex;

use floem::View;
use floem::peniko::kurbo::{Point, Rect};
use floem::reactive::{SignalGet, SignalWith, create_effect};
use floem::views::{Decorators, container, dyn_container, empty, label, stack};
use floem::IntoView;
use floem::window::WindowId;
use serde_json::json;

use ide_webview::raw_window_handle::RawWindowHandle;
pub use ide_webview::{
    Handle, ThemeKind, WebviewBounds, WebviewError, WebviewHost, WebviewOptions,
};

use crate::config::color::LapceColor;
use crate::window_tab::WindowTabData;

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

    /// Evaluate `js` in the page for `id`, delivering the result string to `cb`.
    /// Used by the webview render self-test.
    pub fn evaluate(
        &self,
        id: WebviewId,
        js: &str,
        cb: impl Fn(String) + Send + 'static,
    ) -> Result<(), WebviewError> {
        match self.webviews.borrow().get(&id) {
            Some(handle) => handle.evaluate(js, cb),
            None => Ok(()),
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

/// Deterministic native [`WebviewId`] for a protocol handle string, so the
/// placeholder view and the reconcile effect agree without extra bookkeeping.
fn webview_id_for(handle: &str) -> WebviewId {
    let mut hasher = DefaultHasher::new();
    handle.hash(&mut hasher);
    hasher.finish()
}

/// Maps each floem [`WindowId`] to its AppKit `NSWindow` pointer. Populated when
/// a window gains focus (at which point the OS key window *is* that floem
/// window), so webviews can be parented to the window that actually owns them
/// even when several are open and a different one is focused. Pointers are only
/// dereferenced on the main thread while the window is alive.
static WINDOW_NSWINDOWS: LazyLock<Mutex<HashMap<WindowId, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Floem windows in creation order, so each maps to the AppKit `NSWindow`
/// created in the same order (used when focus events are unavailable).
static WINDOW_ORDINALS: LazyLock<Mutex<Vec<WindowId>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Record the association `window_id -> key NSWindow`. Call from the floem
/// `WindowGotFocus` handler: the AppKit key window is exactly this floem window.
pub fn register_focused_window(window_id: WindowId) {
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_app_kit::NSApplication;

    register_window_ordinal(window_id);
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    if let Some(window) = app.keyWindow().or_else(|| app.mainWindow()) {
        let ptr = Retained::as_ptr(&window) as usize;
        WINDOW_NSWINDOWS.lock().insert(window_id, ptr);
    }
}

/// Record this floem window in creation order. Called when the window is built,
/// so each window maps to the AppKit `NSWindow` created in the same order even
/// when the app is never focused (headless/background), where focus events and
/// `keyWindow` are unavailable.
pub fn register_window_ordinal(window_id: WindowId) {
    let mut v = WINDOW_ORDINALS.lock();
    if !v.contains(&window_id) {
        v.push(window_id);
    }
}

fn window_ordinal(window_id: WindowId) -> usize {
    let mut v = WINDOW_ORDINALS.lock();
    if let Some(i) = v.iter().position(|w| *w == window_id) {
        return i;
    }
    v.push(window_id);
    v.len() - 1
}

/// The content `NSView` to parent a native child webview to, for the floem
/// window `window_id`. Resolution order:
/// 1. the `NSWindow` recorded when this window was focused (most precise, real
///    usage), else
/// 2. the real `NSApp` window at this window's creation ordinal (works without
///    focus — windows are created in the same order floem creates them, and
///    `windowNumber` increases monotonically), else
/// 3. the key/main window.
///
/// The pinned floem revision does not expose a window's raw handle, so we reach
/// it through AppKit on the main thread.
fn parent_window_handle(window_id: WindowId) -> Option<RawWindowHandle> {
    use ide_webview::raw_window_handle::AppKitWindowHandle;
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, Message};
    use objc2_app_kit::{NSApplication, NSView, NSWindow};

    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);

    // 1. A window we recorded while it was focused.
    let mapped: Option<Retained<NSWindow>> = {
        let ptr = WINDOW_NSWINDOWS.lock().get(&window_id).copied();
        ptr.and_then(|ptr| {
            let raw = ptr as *const NSWindow;
            // SAFETY: main thread; the window is alive while its panel exists.
            unsafe { raw.as_ref() }.map(|w| w.retain())
        })
    };

    // 2. The real content window at this window's creation ordinal.
    let by_ordinal = || -> Option<Retained<NSWindow>> {
        let windows = app.windows();
        let mut reals: Vec<Retained<NSWindow>> = windows
            .iter()
            .filter(|w| {
                if w.contentView().is_none() {
                    return false;
                }
                let f = w.frame();
                // Skip tooltip/panel surfaces; content windows are sizeable.
                f.size.width >= 200.0 && f.size.height >= 200.0
            })
            .collect();
        reals.sort_by_key(|w| w.windowNumber());
        let ordinal = window_ordinal(window_id);
        reals.into_iter().nth(ordinal)
    };

    let window: Retained<NSWindow> = mapped
        .or_else(by_ordinal)
        .or_else(|| app.keyWindow().or_else(|| app.mainWindow()))?;
    let view: Retained<NSView> = window.contentView()?;
    let ptr = Retained::as_ptr(&view) as *mut std::ffi::c_void;
    let nn = std::ptr::NonNull::new(ptr)?;
    Some(RawWindowHandle::AppKit(AppKitWindowHandle::new(nn)))
}

/// A right-docked panel that hosts the shared extension host's webviews (e.g.
/// Claude Code's sidebar). It reconciles the native [`WebviewController`] with
/// the reactive `ide_ext.webviews` set, auto-resolves registered webview views,
/// and relays messages both ways. Collapses to zero width when none is active.
pub fn ext_webview_panel(window_tab_data: Rc<WindowTabData>) -> impl View {
    let ide_ext = window_tab_data.ide_ext.clone();
    let proxy = window_tab_data.common.proxy.clone();
    let config = window_tab_data.common.config;

    let window_id = window_tab_data.common.window_common.window_id;
    let controller = WebviewController::new();
    let html_state: Rc<RefCell<HashMap<String, String>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let resolved: Rc<RefCell<std::collections::HashSet<String>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));
    let drained: Rc<Cell<u64>> = Rc::new(Cell::new(0));

    // Auto-resolve each registered webview view once, so it appears in the dock
    // (also exposed via the extension command palette).
    {
        let ide_ext = ide_ext.clone();
        let proxy = proxy.clone();
        let resolved = resolved.clone();
        create_effect(move |_| {
            let views = ide_ext.views.get();
            for v in views.iter().filter(|v| v.is_webview()) {
                if resolved.borrow_mut().insert(v.id.clone()) {
                    proxy.ext_host_request(
                        "ide/webview/resolveView".to_string(),
                        json!({ "viewId": v.id }),
                        |_| {},
                    );
                }
            }
        });
    }

    // Reconcile native webviews with the host's webview set.
    {
        let ide_ext = ide_ext.clone();
        let controller = controller.clone();
        let proxy = proxy.clone();
        let html_state = html_state.clone();
        create_effect(move |_| {
            let webviews = ide_ext.webviews.get();
            let active = ide_ext.active_webview.get();
            for (handle, wv) in webviews.iter() {
                let id = webview_id_for(handle);
                let known = html_state.borrow().contains_key(handle);
                if !known {
                    let Some(parent) = parent_window_handle(window_id) else {
                        continue;
                    };
                    let options = WebviewOptions {
                        enable_scripts: wv.enable_scripts,
                        local_resource_roots: wv.local_resource_roots.clone(),
                        theme_vars: Default::default(),
                        theme_kind: ThemeKind::Dark,
                    };
                    let proxy2 = proxy.clone();
                    let h = handle.clone();
                    match controller.attach(id, parent, &wv.html, options, move |msg| {
                        proxy2.ext_host_notification(
                            "ide/webview/onMessage".to_string(),
                            json!({ "handle": h.clone(), "message": msg }),
                        );
                    }) {
                        Ok(()) => {
                            html_state
                                .borrow_mut()
                                .insert(handle.clone(), wv.html.clone());
                        }
                        Err(e) => tracing::error!("attach webview failed: {e}"),
                    }
                } else {
                    let changed = html_state
                        .borrow()
                        .get(handle)
                        .map(|h| h != &wv.html)
                        .unwrap_or(true);
                    if changed {
                        let _ = controller.set_html(id, &wv.html);
                        html_state
                            .borrow_mut()
                            .insert(handle.clone(), wv.html.clone());
                    }
                }
            }
            // Dispose webviews the host removed.
            let known: Vec<String> =
                html_state.borrow().keys().cloned().collect();
            for h in known {
                if !webviews.contains_key(&h) {
                    controller.dispose(webview_id_for(&h));
                    html_state.borrow_mut().remove(&h);
                }
            }
            // Only the active webview is visible, and only while the dock is open.
            let dock_open = ide_ext.dock_visible.get();
            let handles: Vec<String> =
                html_state.borrow().keys().cloned().collect();
            for h in handles {
                controller.set_visible(
                    webview_id_for(&h),
                    dock_open && active.as_deref() == Some(&h),
                );
            }
        });
    }

    // Drain host -> webview messages.
    {
        let ide_ext = ide_ext.clone();
        let controller = controller.clone();
        let drained = drained.clone();
        create_effect(move |_| {
            let msgs = ide_ext.post_messages.get();
            let last = drained.get();
            let mut max = last;
            msgs.iter().for_each(|m| {
                if m.id > last {
                    let _ = controller
                        .post_message(webview_id_for(&m.handle), &m.message);
                    if m.id > max {
                        max = m.id;
                    }
                }
            });
            if max != last {
                drained.set(max);
            }
        });
    }

    // Render self-test (opt-in via IDE_WEBVIEW_SELFTEST): once a webview is
    // attached and the dock is open, probe the live DOM and log the result, so a
    // smoke test can prove the page actually rendered (not blank) from outside
    // the WKWebView's separate GPU surface.
    if std::env::var_os("IDE_WEBVIEW_SELFTEST").is_some() {
        // Auto-reveal the dock once a webview exists so the probe can run and a
        // screenshot captures rendered content. Default builds keep it hidden.
        {
            let ide_ext = ide_ext.clone();
            create_effect(move |_| {
                let has_webview =
                    ide_ext.webviews.with(|m| !m.is_empty());
                if has_webview && !ide_ext.dock_visible.get_untracked() {
                    ide_ext.show_dock(None);
                }
            });
        }
        let ide_ext = ide_ext.clone();
        let controller = controller.clone();
        let probed: Rc<RefCell<std::collections::HashSet<WebviewId>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        create_effect(move |_| {
            if !ide_ext.dock_visible.get() {
                return;
            }
            let Some(handle) = ide_ext.active_webview.get() else {
                return;
            };
            let id = webview_id_for(&handle);
            if !controller.is_attached(id) {
                return;
            }
            if !probed.borrow_mut().insert(id) {
                return;
            }
            let controller = controller.clone();
            floem::action::exec_after(
                std::time::Duration::from_millis(6000),
                move |_| {
                    let h = handle.clone();
                    let _ = controller.evaluate(
                        id,
                        "JSON.stringify({\
                         nodes:document.querySelectorAll('*').length,\
                         scripts:document.querySelectorAll('script').length,\
                         textLen:(document.body?document.body.innerText.length:0),\
                         title:document.title,\
                         text:(document.body?document.body.innerText:'').replace(/\\s+/g,' ').slice(0,180)})",
                        move |res| {
                            eprintln!("[webview-selftest] {h} => {res}");
                        },
                    );
                },
            );
        });
    }

    let controller_view = controller.clone();
    let ide_ext_title = ide_ext.clone();
    let ide_ext_active = ide_ext.clone();
    let ide_ext_style = ide_ext.clone();
    stack((
        label(move || {
            ide_ext_title
                .active_webview
                .get()
                .and_then(|h| {
                    ide_ext_title.webviews.with(|m| {
                        m.get(&h).map(|w| {
                            if w.title.is_empty() {
                                "Extension".to_string()
                            } else {
                                w.title.clone()
                            }
                        })
                    })
                })
                .unwrap_or_else(|| "Extension".to_string())
        })
        .style(move |s| {
            let config = config.get();
            s.padding_horiz(10.0)
                .height(30.0)
                .items_center()
                .color(config.color(LapceColor::PANEL_FOREGROUND))
                .background(config.color(LapceColor::PANEL_BACKGROUND))
        }),
        dyn_container(
            move || ide_ext_active.active_webview.get(),
            move |active| match active {
                Some(handle) => container(webview_placeholder(
                    controller_view.clone(),
                    webview_id_for(&handle),
                ))
                .style(|s| s.size_full())
                .into_any(),
                None => empty().into_any(),
            },
        )
        .style(|s| s.flex_grow(1.0_f32).size_full()),
    ))
    .style(move |s| {
        let config = config.get();
        let visible = ide_ext_style.dock_visible.get();
        let s = s
            .flex_col()
            .height_full()
            .background(config.color(LapceColor::PANEL_BACKGROUND));
        if visible {
            s.width(440.0)
                .border_left(1.0)
                .border_color(config.color(LapceColor::LAPCE_BORDER))
        } else {
            s.width(0.0)
        }
    })
    .debug_name("ExtensionWebviewDock")
}
