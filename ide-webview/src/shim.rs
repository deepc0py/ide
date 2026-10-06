//! Pure HTML preparation for extension webviews.
//!
//! Before an extension's HTML is handed to the native `WKWebView` we:
//! 1. rewrite every VS Code resource URL to our custom scheme
//!    (see [`crate::uri::rewrite_resource_uris`]),
//! 2. inject the `acquireVsCodeApi()` shim (`postMessage` / `getState` /
//!    `setState`), modelled on
//!    `vendor/vscode/src/vs/workbench/contrib/webview/browser/pre/index.html`,
//!    and
//! 3. inject the `--vscode-*` theme CSS variables (and theme-kind body class)
//!    so extension React UIs render with the host theme.

use std::collections::BTreeMap;

use crate::uri::rewrite_resource_uris;

/// The VS Code theme kind, mirrored onto the webview `<body>` as the class the
/// workbench uses (`vscode-light` / `vscode-dark` / `vscode-high-contrast`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    Light,
    Dark,
    HighContrast,
    HighContrastLight,
}

impl ThemeKind {
    /// The `<body>` class the workbench applies for this kind.
    pub fn body_class(self) -> &'static str {
        match self {
            ThemeKind::Light => "vscode-light",
            ThemeKind::Dark => "vscode-dark",
            ThemeKind::HighContrast => "vscode-high-contrast",
            ThemeKind::HighContrastLight => "vscode-high-contrast vscode-high-contrast-light",
        }
    }
}

impl Default for ThemeKind {
    fn default() -> Self {
        ThemeKind::Dark
    }
}

/// Inputs to [`prepare_html`].
#[derive(Debug, Clone, Default)]
pub struct HtmlPrep {
    /// Whether the extension's scripts (and therefore our shim) run. When
    /// `false` we only rewrite resource URLs and inject theme CSS.
    pub enable_scripts: bool,
    /// `--vscode-*` theme variables, without the leading `--`. e.g.
    /// `"editor-background" -> "#1e1e1e"`.
    pub theme_vars: BTreeMap<String, String>,
    /// The theme kind, used for the `<body>` class.
    pub theme_kind: ThemeKind,
    /// JSON-encoded initial state restored into `getState()` for this load.
    pub initial_state: Option<String>,
}

/// The JavaScript shim installing `globalThis.acquireVsCodeApi`.
///
/// `__IDE_INITIAL_STATE__` is replaced with a JSON expression (or `undefined`)
/// and `__IDE_BODY_CLASS__` with the theme-kind body class.
const SHIM_TEMPLATE: &str = r#"(function(){
  if (window.__ideVscodeApiInstalled) { return; }
  window.__ideVscodeApiInstalled = true;
  var state = __IDE_INITIAL_STATE__;
  var acquired = false;
  function post(obj) {
    try { window.ipc.postMessage(JSON.stringify(obj)); } catch (e) { /* host gone */ }
  }
  Object.defineProperty(window, 'acquireVsCodeApi', {
    value: function acquireVsCodeApi() {
      if (acquired) {
        throw new Error('An instance of the VS Code API has already been acquired');
      }
      acquired = true;
      return Object.freeze({
        postMessage: function (message, transfer) {
          post({ __ide: 'webview', message: message });
        },
        setState: function (newState) {
          state = newState;
          post({ __ide: 'state', state: newState });
          return newState;
        },
        getState: function () {
          return state;
        }
      });
    },
    configurable: false,
    writable: false
  });
  var bodyClass = "__IDE_BODY_CLASS__";
  function applyBodyClass() {
    if (!document.body || !bodyClass) { return; }
    bodyClass.split(' ').forEach(function (c) { if (c) { document.body.classList.add(c); } });
    document.body.setAttribute('data-vscode-theme-kind', bodyClass.split(' ')[0]);
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', applyBodyClass);
  } else {
    applyBodyClass();
  }
})();"#;

/// Build the `<script>`/`<style>` block injected at the top of `<head>`.
fn injection_block(prep: &HtmlPrep) -> String {
    let mut block = String::new();

    if prep.enable_scripts {
        let initial_state = prep.initial_state.as_deref().unwrap_or("undefined");
        let script = SHIM_TEMPLATE
            .replace("__IDE_INITIAL_STATE__", initial_state)
            .replace("__IDE_BODY_CLASS__", prep.theme_kind.body_class());
        block.push_str("<script>");
        block.push_str(&script);
        block.push_str("</script>");
    }

    // Theme CSS variables (and a sensible default color/background) under :root.
    block.push_str("<style id=\"_ide_vscode_theme\">:root{");
    for (name, value) in &prep.theme_vars {
        // Values are CSS tokens (colors / sizes); strip anything that could
        // break out of the declaration.
        let safe = sanitize_css_value(value);
        block.push_str("--vscode-");
        block.push_str(&sanitize_css_name(name));
        block.push(':');
        block.push_str(&safe);
        block.push(';');
    }
    block.push_str("}</style>");

    block
}

/// Keep only characters valid in a CSS custom-property name.
fn sanitize_css_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Strip characters that could terminate a declaration/rule or inject markup.
fn sanitize_css_value(value: &str) -> String {
    value
        .chars()
        .filter(|c| !matches!(c, ';' | '{' | '}' | '<' | '>'))
        .collect()
}

/// Case-insensitively find the byte offset just past the first `<head ...>`
/// opening tag, or an insertion point before `<body`, or after `<html ...>`.
fn head_insertion_point(html: &str) -> usize {
    let lower = html.to_ascii_lowercase();

    // After `<head ...>`.
    if let Some(start) = find_tag(&lower, "<head") {
        if let Some(gt) = lower[start..].find('>') {
            return start + gt + 1;
        }
    }
    // Before `<body ...>`.
    if let Some(start) = find_tag(&lower, "<body") {
        return start;
    }
    // After `<html ...>`.
    if let Some(start) = find_tag(&lower, "<html") {
        if let Some(gt) = lower[start..].find('>') {
            return start + gt + 1;
        }
    }
    // Fragment with no html/head/body: prepend.
    0
}

/// Find `needle` as a tag start: the needle followed by whitespace or `>`.
fn find_tag(lower_html: &str, needle: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = lower_html[from..].find(needle) {
        let idx = from + rel;
        let after = lower_html[idx + needle.len()..].chars().next();
        match after {
            Some(c) if c.is_whitespace() || c == '>' => return Some(idx),
            None => return Some(idx),
            _ => from = idx + needle.len(),
        }
    }
    None
}

/// Rewrite resource URLs and inject the shim + theme into `html`.
pub fn prepare_html(html: &str, prep: &HtmlPrep) -> String {
    let rewritten = rewrite_resource_uris(html);
    let block = injection_block(prep);
    let at = head_insertion_point(&rewritten);
    let mut out = String::with_capacity(rewritten.len() + block.len());
    out.push_str(&rewritten[..at]);
    out.push_str(&block);
    out.push_str(&rewritten[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("editor-background".to_string(), "#1e1e1e".to_string());
        m.insert("foreground".to_string(), "#cccccc".to_string());
        m
    }

    #[test]
    fn injects_shim_and_theme_into_head() {
        let prep = HtmlPrep {
            enable_scripts: true,
            theme_vars: vars(),
            theme_kind: ThemeKind::Dark,
            initial_state: None,
        };
        let html = "<html><head><title>x</title></head><body></body></html>";
        let out = prepare_html(html, &prep);

        // Shim present and before the page's own head content.
        assert!(out.contains("acquireVsCodeApi"));
        let shim_at = out.find("acquireVsCodeApi").unwrap();
        let title_at = out.find("<title>").unwrap();
        assert!(shim_at < title_at, "shim must be injected before page content");

        // Theme variables present.
        assert!(out.contains("--vscode-editor-background:#1e1e1e;"));
        assert!(out.contains("--vscode-foreground:#cccccc;"));
        // Body class wired through the shim.
        assert!(out.contains("vscode-dark"));
        // Default undefined initial state.
        assert!(out.contains("var state = undefined;"));
    }

    #[test]
    fn omits_shim_when_scripts_disabled_but_keeps_theme() {
        let prep = HtmlPrep {
            enable_scripts: false,
            theme_vars: vars(),
            theme_kind: ThemeKind::Light,
            initial_state: None,
        };
        let html = "<html><head></head><body></body></html>";
        let out = prepare_html(html, &prep);
        assert!(!out.contains("acquireVsCodeApi"));
        assert!(out.contains("--vscode-editor-background:#1e1e1e;"));
    }

    #[test]
    fn embeds_initial_state_json() {
        let prep = HtmlPrep {
            enable_scripts: true,
            theme_vars: BTreeMap::new(),
            theme_kind: ThemeKind::Dark,
            initial_state: Some(r#"{"count":3}"#.to_string()),
        };
        let out = prepare_html("<head></head>", &prep);
        assert!(out.contains(r#"var state = {"count":3};"#));
    }

    #[test]
    fn rewrites_resource_urls_during_prepare() {
        let prep = HtmlPrep {
            enable_scripts: false,
            theme_vars: BTreeMap::new(),
            theme_kind: ThemeKind::Dark,
            initial_state: None,
        };
        let html = r#"<head><link href="https://file+.vscode-resource.vscode-cdn.net/Users/a/x.css"></head>"#;
        let out = prepare_html(html, &prep);
        assert!(out.contains("vscode-resource://file/Users/a/x.css"));
        assert!(!out.contains("vscode-cdn.net"));
    }

    #[test]
    fn handles_fragment_without_head() {
        let prep = HtmlPrep {
            enable_scripts: true,
            theme_vars: BTreeMap::new(),
            theme_kind: ThemeKind::Dark,
            initial_state: None,
        };
        let out = prepare_html("<div>hi</div>", &prep);
        // Injected at the very start.
        assert!(out.starts_with("<script>"));
        assert!(out.contains("<div>hi</div>"));
    }

    #[test]
    fn inserts_before_body_when_no_head() {
        let prep = HtmlPrep {
            enable_scripts: true,
            theme_vars: BTreeMap::new(),
            theme_kind: ThemeKind::Dark,
            initial_state: None,
        };
        let out = prepare_html("<html><body><p>x</p></body></html>", &prep);
        let inject = out.find("<script>").unwrap();
        let body = out.find("<body>").unwrap();
        assert!(inject < body);
    }

    #[test]
    fn sanitizes_css_value_injection() {
        let mut m = BTreeMap::new();
        m.insert(
            "evil".to_string(),
            "red;} body{display:none".to_string(),
        );
        let prep = HtmlPrep {
            enable_scripts: false,
            theme_vars: m,
            theme_kind: ThemeKind::Dark,
            initial_state: None,
        };
        let out = prepare_html("<head></head>", &prep);
        assert!(!out.contains("body{display:none"));
        assert!(out.contains("--vscode-evil:red bodydisplay:none;"));
    }
}
