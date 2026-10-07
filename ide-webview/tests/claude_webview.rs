//! Regression test: the Claude Code extension's webview renders styled under
//! Dark Modern in the native host.
//!
//! Two guarantees, checked against the real extension bundle installed under
//! `IDE_BENCH_EXT_DIR` (default `/tmp/bench-ext`, the bench environment). When
//! that extension is not present (e.g. plain CI) the test skips, so
//! `cargo test -p ide-webview` stays green without the fixtures.
//!
//! 1. Every resource URL referenced by the webview HTML resolves to a real file
//!    under the webview's resource roots (no 403/404), and a traversal attempt
//!    is rejected (403).
//! 2. Every `--vscode-*` CSS variable referenced by the Claude webview CSS is
//!    defined by our injected Dark Modern theme CSS, except the documented set
//!    that VS Code's own Dark Modern webview also leaves undefined (null default
//!    + not in the theme), which extensions reference with `var(x, fallback)`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ide_webview::shim::{prepare_html, HtmlPrep, ThemeKind};
use ide_webview::theme::webview_theme_vars;
use ide_webview::uri::{resolve_resource_path, ResolveError};

/// `--vscode-*` variables the Claude CSS references that VS Code's Dark Modern
/// webview theme ALSO leaves undefined: their registry default is null (or they
/// are editor-internal vars VS Code never injects into webviews), and the theme
/// does not set them. Matching VS Code, we deliberately do not define these;
/// the extension's CSS falls back via `var(--x, <fallback>)`.
const VSCODE_DARK_UNDEFINED: &[&str] = &[
    "--vscode-chat-font-family",
    "--vscode-chat-font-size",
    "--vscode-contrastActiveBorder",
    "--vscode-contrastBorder",
    "--vscode-diffEditor-border",
    "--vscode-diffEditor-insertedLineBackground",
    "--vscode-diffEditor-insertedTextBorder",
    "--vscode-diffEditor-removedLineBackground",
    "--vscode-diffEditor-removedTextBorder",
    "--vscode-diffEditorGutter-insertedLineBackground",
    "--vscode-diffEditorGutter-removedLineBackground",
    "--vscode-editor-findMatchBorder",
    "--vscode-editor-lineHighlightBackground",
    "--vscode-editor-rangeHighlightBorder",
    "--vscode-editor-selectionHighlightBorder",
    "--vscode-editor-snippetFinalTabstopHighlightBackground",
    "--vscode-editor-snippetTabstopHighlightBorder",
    "--vscode-editor-symbolHighlightBorder",
    "--vscode-editor-wordHighlightBorder",
    "--vscode-editor-wordHighlightStrongBorder",
    "--vscode-editor-wordHighlightTextBorder",
    "--vscode-editorCodeLens-fontFamily",
    "--vscode-editorCodeLens-fontFamilyDefault",
    "--vscode-editorCodeLens-fontFeatureSettings",
    "--vscode-editorCodeLens-fontSize",
    "--vscode-editorCodeLens-lineHeight",
    "--vscode-editorError-background",
    "--vscode-editorError-border",
    "--vscode-editorGhostText-background",
    "--vscode-editorGhostText-border",
    "--vscode-editorHint-border",
    "--vscode-editorInfo-background",
    "--vscode-editorInfo-border",
    "--vscode-editorMarkerNavigationInfo-headerBackground",
    "--vscode-editorStickyScroll-border",
    "--vscode-editorStickyScroll-foldingOpacityTransition",
    "--vscode-editorStickyScroll-scrollableWidth",
    "--vscode-editorUnicodeHighlight-background",
    "--vscode-editorUnnecessaryCode-border",
    "--vscode-editorWarning-background",
    "--vscode-editorWarning-border",
    "--vscode-editorWidget-resizeBorder",
    "--vscode-hover-maxWidth",
    "--vscode-hover-sourceWhiteSpace",
    "--vscode-hover-whiteSpace",
    "--vscode-icon-x-content",
    "--vscode-icon-x-font-family",
    "--vscode-menu-selectionBorder",
    "--vscode-parameterHintsWidget-editorFontFamily",
    "--vscode-parameterHintsWidget-editorFontFamilyDefault",
    "--vscode-peekViewEditor-matchHighlightBorder",
];

fn claude_dir() -> Option<PathBuf> {
    let base = std::env::var("IDE_BENCH_EXT_DIR").unwrap_or_else(|_| "/tmp/bench-ext".to_string());
    let base = Path::new(&base);
    if !base.is_dir() {
        return None;
    }
    std::fs::read_dir(base)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("anthropic.claude-code"))
                && p.join("webview/index.css").is_file()
        })
}

/// Collect every `--vscode-...` custom property referenced in `css`.
fn referenced_vars(css: &str) -> BTreeSet<String> {
    let bytes = css.as_bytes();
    let mut out = BTreeSet::new();
    let needle = b"--vscode-";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < bytes.len() {
                let c = bytes[j];
                if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' {
                    j += 1;
                } else {
                    break;
                }
            }
            out.insert(String::from_utf8_lossy(&bytes[i..j]).into_owned());
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Pull every rewritten `vscode-resource://...` URL out of prepared HTML.
fn resource_urls(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = "vscode-resource://";
    let mut rest = html;
    while let Some(pos) = rest.find(needle) {
        let tail = &rest[pos..];
        let end = tail
            .find(|c: char| c == '"' || c == '\'' || c == ' ' || c == '>' || c == ')')
            .unwrap_or(tail.len());
        out.push(tail[..end].to_string());
        rest = &tail[end..];
    }
    out
}

#[test]
fn claude_webview_resources_resolve_and_theme_is_defined() {
    let Some(dir) = claude_dir() else {
        eprintln!("[skip] Claude Code extension not found under IDE_BENCH_EXT_DIR; skipping");
        return;
    };
    let abs = dir.canonicalize().expect("canonicalize claude dir");
    let roots = vec![abs.clone()];

    // --- 1. Resources referenced by the webview HTML resolve (no 403/404) ----
    // The Claude webview loads its bundle + stylesheet from the extension's
    // `webview/` dir via asWebviewUri (the CDN host form). Build the HTML VS
    // Code would hand us and assert every resource resolves to a real file.
    let mut assets: Vec<PathBuf> = Vec::new();
    for name in ["webview/index.js", "webview/index.css"] {
        let p = abs.join(name);
        assert!(p.is_file(), "expected Claude asset {p:?}");
        assets.push(p);
    }
    let mut body = String::from("<!DOCTYPE html><html><head>");
    for a in &assets {
        let url = format!(
            "https://file+.vscode-resource.vscode-cdn.net{}",
            a.to_string_lossy()
        );
        if a.extension().and_then(|e| e.to_str()) == Some("css") {
            body.push_str(&format!("<link rel=\"stylesheet\" href=\"{url}\">"));
        } else {
            body.push_str(&format!("<script src=\"{url}\"></script>"));
        }
    }
    body.push_str("</head><body></body></html>");

    let prepared = prepare_html(
        &body,
        &HtmlPrep {
            enable_scripts: true,
            theme_kind: ThemeKind::Dark,
            ..HtmlPrep::default()
        },
    );
    let urls = resource_urls(&prepared);
    assert_eq!(urls.len(), assets.len(), "every asset rewritten to a resource URL");
    for url in &urls {
        match resolve_resource_path(url, &roots) {
            Ok(path) => assert!(path.is_file(), "resource {url} -> {path:?} must exist (no 404)"),
            Err(e) => panic!("resource {url} failed to resolve (would be 403/404): {e:?}"),
        }
    }

    // A path outside the resource roots must be rejected (403), proving root
    // enforcement is active rather than serving anything referenced.
    let escape = "vscode-resource://file/etc/passwd";
    assert!(
        matches!(resolve_resource_path(escape, &roots), Err(ResolveError::OutsideRoots(_))),
        "out-of-root resource must be forbidden"
    );

    // --- 2. Every --vscode-* var the Claude CSS uses is themed -------------
    let css = std::fs::read_to_string(abs.join("webview/index.css")).expect("read claude css");
    let refs = referenced_vars(&css);
    assert!(refs.len() > 200, "sanity: Claude CSS references many vscode vars (got {})", refs.len());

    let injected = webview_theme_vars(ThemeKind::Dark);
    let allow: BTreeSet<&str> = VSCODE_DARK_UNDEFINED.iter().copied().collect();

    let mut missing: Vec<String> = Vec::new();
    for v in &refs {
        if !injected.contains_key(v) && !allow.contains(v.as_str()) {
            missing.push(v.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "Claude CSS references {} --vscode-* vars not defined by our Dark Modern theme \
         and not in the known-undefined allowlist: {:?}",
        missing.len(),
        missing
    );

    // Prove it really is the dark theme (not just presence).
    assert_eq!(injected.get("--vscode-editor-background").map(String::as_str), Some("#1f1f1f"));
    assert_eq!(injected.get("--vscode-foreground").map(String::as_str), Some("#cccccc"));
    assert_eq!(injected.get("--vscode-button-background").map(String::as_str), Some("#0078d4"));
    assert_eq!(injected.get("--vscode-button-foreground").map(String::as_str), Some("#ffffff"));
    assert!(injected.contains_key("--vscode-font-family"));
}
