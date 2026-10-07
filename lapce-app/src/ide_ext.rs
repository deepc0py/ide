//! Per-window state for the shared VS Code extension host's UI contributions.
//!
//! The proxy forwards the host's `ide/*` protocol to core as
//! [`CoreNotification`](lapce_rpc::core::CoreNotification) variants; those land
//! in [`WindowTabData::handle_core_notification`](crate::window_tab::WindowTabData)
//! which updates the reactive signals below. The render surfaces
//! (status bar, command palette, output panel, editor decorations, webview
//! panel) read these signals.

use std::path::PathBuf;

use floem::reactive::{
    RwSignal, Scope, SignalGet, SignalUpdate, SignalWith,
};
use im::{HashMap, Vector};
use lapce_rpc::ide_ext::{
    Decoration, ExtCommand, ExtView, StatusBarItem, WebviewCreate,
};

/// A native webview announced by the host (`ide/webview/create`).
#[derive(Clone, Debug)]
pub struct IdeWebview {
    pub handle: String,
    pub view_type: String,
    pub title: String,
    pub html: String,
    /// `"view"` (sidebar view container) or `"panel"` (editor-area panel).
    pub kind: String,
    pub enable_scripts: bool,
    pub local_resource_roots: Vec<PathBuf>,
}

impl IdeWebview {
    fn from_create(create: WebviewCreate) -> Self {
        let local_resource_roots = create
            .options
            .local_resource_roots
            .iter()
            .filter_map(|r| {
                url::Url::parse(r)
                    .ok()
                    .and_then(|u| u.to_file_path().ok())
                    .or_else(|| Some(PathBuf::from(r)))
            })
            .collect();
        IdeWebview {
            handle: create.handle,
            view_type: create.view_type,
            title: create.title,
            html: create.html,
            kind: create.kind,
            enable_scripts: create.options.enable_scripts,
            local_resource_roots,
        }
    }
}

/// A queued host -> webview message (`ide/webview/postMessage`), tagged with a
/// monotonic id so the native host can drain without re-delivering.
#[derive(Clone, Debug)]
pub struct WebviewMessage {
    pub id: u64,
    pub handle: String,
    pub message: serde_json::Value,
}

#[derive(Clone)]
pub struct IdeExtData {
    pub scope: Scope,
    /// Status-bar items keyed by id (upsert / remove).
    pub status_items: RwSignal<HashMap<String, StatusBarItem>>,
    /// Contributed commands (command palette entries).
    pub commands: RwSignal<Vector<ExtCommand>>,
    /// Appended output-channel chunks `(channel, text)`.
    pub output: RwSignal<Vector<(String, String)>>,
    /// Registered view containers.
    pub views: RwSignal<Vector<ExtView>>,
    /// Live webviews keyed by handle.
    pub webviews: RwSignal<HashMap<String, IdeWebview>>,
    /// Handle of the webview currently shown in the extension dock.
    pub active_webview: RwSignal<Option<String>>,
    /// Whether the extension webview dock is shown. Hidden by default; the user
    /// reveals it (palette command / Claude Code status item / toggle) — opening
    /// a webview never forces the dock open on its own.
    pub dock_visible: RwSignal<bool>,
    /// Inline decorations keyed by document path.
    pub decorations: RwSignal<HashMap<PathBuf, Vector<Decoration>>>,
    /// Monotonic queue of host -> webview messages.
    pub post_messages: RwSignal<Vector<WebviewMessage>>,
    next_msg_id: RwSignal<u64>,
}

impl std::fmt::Debug for IdeExtData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IdeExtData")
    }
}

impl IdeExtData {
    pub fn new(cx: Scope) -> Self {
        IdeExtData {
            scope: cx,
            status_items: cx.create_rw_signal(HashMap::new()),
            commands: cx.create_rw_signal(Vector::new()),
            output: cx.create_rw_signal(Vector::new()),
            views: cx.create_rw_signal(Vector::new()),
            webviews: cx.create_rw_signal(HashMap::new()),
            active_webview: cx.create_rw_signal(None),
            dock_visible: cx.create_rw_signal(false),
            decorations: cx.create_rw_signal(HashMap::new()),
            post_messages: cx.create_rw_signal(Vector::new()),
            next_msg_id: cx.create_rw_signal(0),
        }
    }

    pub fn set_status_item(&self, item: StatusBarItem) {
        self.status_items
            .update(|m| {
                m.insert(item.id.clone(), item);
            });
    }

    pub fn remove_status_item(&self, id: &str) {
        self.status_items.update(|m| {
            m.remove(id);
        });
    }

    /// Status-bar items split into (left, right), each ordered by descending
    /// priority (VS Code convention), then id for stability.
    pub fn status_items_split(&self) -> (Vec<StatusBarItem>, Vec<StatusBarItem>) {
        self.status_items.with(|m| {
            let mut left: Vec<StatusBarItem> = Vec::new();
            let mut right: Vec<StatusBarItem> = Vec::new();
            for item in m.values() {
                if item.is_left() {
                    left.push(item.clone());
                } else {
                    right.push(item.clone());
                }
            }
            let sort = |v: &mut Vec<StatusBarItem>| {
                v.sort_by(|a, b| {
                    b.priority
                        .unwrap_or(0.0)
                        .partial_cmp(&a.priority.unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.id.cmp(&b.id))
                });
            };
            sort(&mut left);
            sort(&mut right);
            // De-duplicate items that render identically. Items are keyed by id
            // in the map, but some extensions register two differently-ided
            // items with the same label (e.g. a duplicate "Prettier"); collapse
            // those to the first (highest-priority) one so the bar isn't noisy.
            let render_key = |text: &str| {
                let mut out = String::new();
                let mut rest = text;
                while let Some(start) = rest.find("$(") {
                    out.push_str(&rest[..start]);
                    match rest[start..].find(')') {
                        Some(end) => rest = &rest[start + end + 1..],
                        None => {
                            rest = "";
                            break;
                        }
                    }
                }
                out.push_str(rest);
                out.split_whitespace().collect::<Vec<_>>().join(" ")
            };
            let dedup = |v: &mut Vec<StatusBarItem>| {
                let mut seen = std::collections::HashSet::new();
                v.retain(|item| seen.insert(render_key(&item.text)));
            };
            dedup(&mut left);
            dedup(&mut right);
            (left, right)
        })
    }

    pub fn set_commands(&self, commands: Vec<ExtCommand>) {
        self.commands.set(Vector::from(commands));
    }

    pub fn append_output(&self, channel: String, text: String) {
        self.output.update(|o| {
            o.push_back((channel, text));
            // Bound memory: keep the last 2000 chunks.
            while o.len() > 2000 {
                o.pop_front();
            }
        });
    }

    pub fn register_views(&self, views: Vec<ExtView>) {
        self.views.update(|existing| {
            for v in views {
                if !existing.iter().any(|e| e.id == v.id) {
                    existing.push_back(v);
                }
            }
        });
    }

    pub fn webview_create(&self, create: WebviewCreate) {
        let webview = IdeWebview::from_create(create);
        let handle = webview.handle.clone();
        self.webviews.update(|m| {
            m.insert(handle.clone(), webview);
        });
        self.active_webview.set(Some(handle));
    }

    pub fn webview_set_html(&self, handle: &str, html: String) {
        self.webviews.update(|m| {
            if let Some(w) = m.get_mut(handle) {
                w.html = html;
            }
        });
    }

    pub fn webview_set_title(&self, handle: &str, title: String) {
        self.webviews.update(|m| {
            if let Some(w) = m.get_mut(handle) {
                w.title = title;
            }
        });
    }

    pub fn webview_post_message(&self, handle: String, message: serde_json::Value) {
        let id = self.next_msg_id.get_untracked() + 1;
        self.next_msg_id.set(id);
        self.post_messages.update(|q| {
            q.push_back(WebviewMessage {
                id,
                handle,
                message,
            });
            while q.len() > 1000 {
                q.pop_front();
            }
        });
    }

    pub fn webview_dispose(&self, handle: &str) {
        self.webviews.update(|m| {
            m.remove(handle);
        });
        if self.active_webview.get_untracked().as_deref() == Some(handle) {
            // Fall back to another live webview, or hide the dock if none remain.
            let next = self
                .webviews
                .with_untracked(|m| m.keys().next().cloned());
            self.active_webview.set(next.clone());
            if next.is_none() {
                self.dock_visible.set(false);
            }
        }
    }

    /// Reveal the extension webview dock, showing `handle` (or the first live
    /// webview). Used by the "open extension view" affordances.
    pub fn show_dock(&self, handle: Option<String>) {
        let handle = handle.or_else(|| {
            self.active_webview
                .get_untracked()
                .or_else(|| self.webviews.with_untracked(|m| m.keys().next().cloned()))
        });
        if handle.is_some() {
            self.active_webview.set(handle);
        }
        self.dock_visible.set(true);
    }

    /// Toggle the extension webview dock. When revealing it with nothing active,
    /// select the first available webview.
    pub fn toggle_dock(&self) {
        if self.dock_visible.get_untracked() {
            self.dock_visible.set(false);
        } else {
            self.show_dock(None);
        }
    }

    pub fn set_decorations(&self, path: PathBuf, decorations: Vec<Decoration>) {
        self.decorations.update(|m| {
            if decorations.is_empty() {
                m.remove(&path);
            } else {
                m.insert(path, Vector::from(decorations));
            }
        });
    }
}
