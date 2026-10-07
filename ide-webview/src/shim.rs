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
    /// Webview CSS custom properties, keyed by the *full* property name
    /// (including the leading `--`), e.g. `"--vscode-editor-background" ->
    /// "#1f1f1f"`, `"--text-link-decoration" -> "none"`. Built by
    /// [`crate::theme::webview_theme_vars`].
    pub theme_vars: BTreeMap<String, String>,
    /// The theme kind, used for the `<body>` class and `data-vscode-theme-kind`.
    pub theme_kind: ThemeKind,
    /// Human-readable theme label (`data-vscode-theme-name`).
    pub theme_name: String,
    /// Theme settings id (`data-vscode-theme-id`).
    pub theme_id: String,
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
  var themeName = "__IDE_THEME_NAME__";
  var themeId = "__IDE_THEME_ID__";
  function applyBodyClass() {
    if (!document.body) { return; }
    if (bodyClass) {
      bodyClass.split(' ').forEach(function (c) { if (c) { document.body.classList.add(c); } });
      document.body.setAttribute('data-vscode-theme-kind', bodyClass.split(' ')[0]);
    }
    document.body.setAttribute('data-vscode-theme-name', themeName);
    document.body.setAttribute('data-vscode-theme-id', themeId);
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', applyBodyClass);
  } else {
    applyBodyClass();
  }
})();"#;

/// VS Code's webview default stylesheet
/// (`vs/workbench/contrib/webview/browser/pre/index.html`), wrapped in the
/// low-priority `@layer vscode-default` so an extension's own CSS always wins.
/// Adapted for the native `WKWebView`: the page itself paints the editor
/// background (in VS Code the workbench paints behind a transparent body; here
/// there is no workbench behind the surface, so without this the view is white),
/// and `<button>` gets `appearance: none` so WebKit honors the themed background
/// like Chromium (VS Code's engine) does instead of drawing a light native
/// control for buttons whose extension CSS leaves the background transparent.
const DEFAULT_STYLES: &str = r#"@layer vscode-default {
  html { background-color: var(--vscode-editor-background); scrollbar-color: var(--vscode-scrollbarSlider-background) var(--vscode-editor-background); }
  body { overscroll-behavior-x: none; background-color: transparent; color: var(--vscode-editor-foreground); font-family: var(--vscode-font-family); font-weight: var(--vscode-font-weight); font-size: var(--vscode-font-size); margin: 0; padding: 0 20px; }
  button { -webkit-appearance: none; appearance: none; background-color: transparent; color: inherit; font: inherit; }
  img, video { max-width: 100%; max-height: 100%; }
  a, a code { color: var(--vscode-textLink-foreground); }
  p > a { text-decoration: var(--text-link-decoration); }
  a:hover { color: var(--vscode-textLink-activeForeground); }
  a:focus, input:focus, select:focus, textarea:focus { outline: 1px solid -webkit-focus-ring-color; outline-offset: -1px; }
  code { font-family: var(--monaco-monospace-font); color: var(--vscode-textPreformat-foreground); background-color: var(--vscode-textPreformat-background); padding: 1px 3px; border-radius: 4px; }
  pre code { padding: 0; }
  blockquote { background: var(--vscode-textBlockQuote-background); border-color: var(--vscode-textBlockQuote-border); }
  kbd { background-color: var(--vscode-keybindingLabel-background); color: var(--vscode-keybindingLabel-foreground); border-style: solid; border-width: 1px; border-radius: 3px; border-color: var(--vscode-keybindingLabel-border); border-bottom-color: var(--vscode-keybindingLabel-bottomBorder); box-shadow: inset 0 -1px 0 var(--vscode-widget-shadow); vertical-align: middle; padding: 1px 3px; }
  ::-webkit-scrollbar { width: 10px; height: 10px; }
  ::-webkit-scrollbar-corner { background-color: var(--vscode-editor-background); }
  ::-webkit-scrollbar-thumb { background-color: var(--vscode-scrollbarSlider-background); }
  ::-webkit-scrollbar-thumb:hover { background-color: var(--vscode-scrollbarSlider-hoverBackground); }
  ::-webkit-scrollbar-thumb:active { background-color: var(--vscode-scrollbarSlider-activeBackground); }
}"#;

/// Escape a string for use inside a double-quoted JS string literal.
fn js_string(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '\\' => vec!['\\', '\\'],
            '"' => vec!['\\', '"'],
            '\n' => vec!['\\', 'n'],
            '\r' => vec!['\\', 'r'],
            '<' | '>' => vec![],
            _ => vec![c],
        })
        .collect()
}

/// Build the `<style>`/`<script>` block injected at the top of `<head>`:
/// the default stylesheet, the `:root` theme variables, then (if scripts are
/// enabled) the `acquireVsCodeApi` shim that also applies the theme body class
/// and `data-vscode-theme-*` attributes.
fn injection_block(prep: &HtmlPrep) -> String {
    let mut block = String::new();

    // 1. Default webview stylesheet (low-priority layer).
    block.push_str("<style id=\"_defaultStyles\">");
    block.push_str(DEFAULT_STYLES);
    block.push_str("</style>");

    // 2. Theme CSS variables under :root (full property names).
    block.push_str("<style id=\"_ide_vscode_theme\">:root{");
    for (name, value) in &prep.theme_vars {
        let safe = sanitize_css_value(value);
        block.push_str(&sanitize_css_name(name));
        block.push(':');
        block.push_str(&safe);
        block.push(';');
    }
    block.push_str("}</style>");

    // 3. The acquireVsCodeApi shim + body class / data attributes.
    if prep.enable_scripts {
        let initial_state = prep.initial_state.as_deref().unwrap_or("undefined");
        let script = SHIM_TEMPLATE
            .replace("__IDE_INITIAL_STATE__", initial_state)
            .replace("__IDE_BODY_CLASS__", prep.theme_kind.body_class())
            .replace("__IDE_THEME_NAME__", &js_string(&prep.theme_name))
            .replace("__IDE_THEME_ID__", &js_string(&prep.theme_id));
        block.push_str("<script>");
        block.push_str(&script);
        block.push_str("</script>");
    }

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
        m.insert("--vscode-editor-background".to_string(), "#1e1e1e".to_string());
        m.insert("--vscode-foreground".to_string(), "#cccccc".to_string());
        m
    }

    #[test]
    fn injects_shim_and_theme_into_head() {
        let prep = HtmlPrep {
            enable_scripts: true,
            theme_vars: vars(),
            theme_kind: ThemeKind::Dark,
            theme_name: "Dark Modern".to_string(),
            theme_id: "Default Dark Modern".to_string(),
            initial_state: None,
        };
        let html = "<html><head><title>x</title></head><body></body></html>";
        let out = prepare_html(html, &prep);

        // Shim present and before the page's own head content.
        assert!(out.contains("acquireVsCodeApi"));
        let shim_at = out.find("acquireVsCodeApi").unwrap();
        let title_at = out.find("<title>").unwrap();
        assert!(shim_at < title_at, "shim must be injected before page content");

        // Default stylesheet injected (low-priority layer) and before page head.
        assert!(out.contains("@layer vscode-default"));
        assert!(out.contains("id=\"_defaultStyles\""));
        assert!(out.find("_defaultStyles").unwrap() < title_at);

        // Theme variables present (full property names, not prefixed again).
        assert!(out.contains("--vscode-editor-background:#1e1e1e;"));
        assert!(out.contains("--vscode-foreground:#cccccc;"));
        // Body class + theme id/name wired through the shim.
        assert!(out.contains("vscode-dark"));
        assert!(out.contains("data-vscode-theme-kind"));
        assert!(out.contains("Default Dark Modern"));
        // Default undefined initial state.
        assert!(out.contains("var state = undefined;"));
    }

    #[test]
    fn omits_shim_when_scripts_disabled_but_keeps_theme() {
        let prep = HtmlPrep {
            enable_scripts: false,
            theme_vars: vars(),
            theme_kind: ThemeKind::Light,
            ..HtmlPrep::default()
        };
        let html = "<html><head></head><body></body></html>";
        let out = prepare_html(html, &prep);
        assert!(!out.contains("acquireVsCodeApi"));
        // Theme + default stylesheet still injected without scripts.
        assert!(out.contains("--vscode-editor-background:#1e1e1e;"));
        assert!(out.contains("@layer vscode-default"));
    }

    #[test]
    fn embeds_initial_state_json() {
        let prep = HtmlPrep {
            enable_scripts: true,
            initial_state: Some(r#"{"count":3}"#.to_string()),
            ..HtmlPrep::default()
        };
        let out = prepare_html("<head></head>", &prep);
        assert!(out.contains(r#"var state = {"count":3};"#));
    }

    #[test]
    fn rewrites_resource_urls_during_prepare() {
        let prep = HtmlPrep {
            enable_scripts: false,
            ..HtmlPrep::default()
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
            ..HtmlPrep::default()
        };
        let out = prepare_html("<div>hi</div>", &prep);
        // The injected block (default stylesheet first) is prepended.
        assert!(out.starts_with("<style id=\"_defaultStyles\">"));
        assert!(out.contains("<div>hi</div>"));
        // The shim still precedes the page fragment.
        assert!(out.find("acquireVsCodeApi").unwrap() < out.find("<div>hi</div>").unwrap());
    }

    #[test]
    fn inserts_before_body_when_no_head() {
        let prep = HtmlPrep {
            enable_scripts: true,
            ..HtmlPrep::default()
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
            "--vscode-evil".to_string(),
            "red;} body{display:none".to_string(),
        );
        let prep = HtmlPrep {
            enable_scripts: false,
            theme_vars: m,
            ..HtmlPrep::default()
        };
        let out = prepare_html("<head></head>", &prep);
        assert!(!out.contains("body{display:none"));
        assert!(out.contains("--vscode-evil:red bodydisplay:none;"));
    }
}
