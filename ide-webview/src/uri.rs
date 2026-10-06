//! Pure URI handling for VS Code webview resources.
//!
//! VS Code's `asWebviewUri` rewrites a local `file:` resource to
//! `https://${scheme}+${authority}.vscode-resource.vscode-cdn.net${path}`
//! (see `vendor/vscode/src/vs/workbench/contrib/webview/common/webview.ts`).
//!
//! A native `WKWebView` cannot install a URL scheme handler for the builtin
//! `https` scheme, so we rewrite every VS Code resource URL (and the legacy
//! `vscode-resource:` / `vscode-webview-resource:` variants) to a single
//! custom scheme [`RESOURCE_SCHEME`] that we *can* intercept. The custom
//! protocol handler then serves only files that live under one of the
//! webview's `local_resource_roots`.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use percent_encoding::percent_decode_str;
use regex::Regex;

/// The custom scheme we rewrite every webview resource URL to. A scheme we own
/// (unlike `https`) can be intercepted by a `WKURLSchemeHandler` via `wry`.
pub const RESOURCE_SCHEME: &str = "vscode-resource";

/// VS Code's resource CDN host suffix.
pub const CDN_SUFFIX: &str = ".vscode-resource.vscode-cdn.net";

/// Matches a concrete CDN resource host, e.g.
/// `https://file+.vscode-resource.vscode-cdn.net`.
static CDN_HOST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://[A-Za-z0-9+._%\-]+\.vscode-resource\.vscode-cdn\.net")
        .expect("valid cdn host regex")
});

/// Matches the legacy opaque form `vscode-resource:/path` (exactly one slash,
/// i.e. not `vscode-resource://authority/...` and not a bare `vscode-resource:`
/// CSP scheme-source).
static OPAQUE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"vscode-resource:/(?P<rest>[^/])").expect("valid opaque regex"));

/// Rewrite every VS Code webview resource URL in `html` to [`RESOURCE_SCHEME`].
///
/// Handles, in order:
/// 1. the CSP wildcard source `https://*.vscode-resource.vscode-cdn.net`,
/// 2. the legacy `vscode-webview-resource:` scheme,
/// 3. concrete CDN hosts `https://<scheme>+<auth>.vscode-resource.vscode-cdn.net`,
/// 4. the legacy opaque form `vscode-resource:/<path>`.
///
/// The `<scheme>+<auth>` authority is collapsed to the fixed authority `file`
/// (we only serve local files); the absolute filesystem path lives in the URL
/// path and is resolved by [`resolve_resource_path`].
pub fn rewrite_resource_uris(html: &str) -> String {
    // 1. CSP wildcard scheme-source -> bare scheme-source.
    let mut out = html.replace(
        &format!("https://*{CDN_SUFFIX}"),
        &format!("{RESOURCE_SCHEME}:"),
    );

    // 2. Legacy webview-resource scheme -> our scheme (preserve the rest).
    out = out.replace("vscode-webview-resource://", &format!("{RESOURCE_SCHEME}://"));
    out = out.replace("vscode-webview-resource:", &format!("{RESOURCE_SCHEME}:"));

    // 3. Concrete CDN hosts -> custom scheme with fixed `file` authority.
    out = CDN_HOST_RE
        .replace_all(&out, format!("{RESOURCE_SCHEME}://file"))
        .into_owned();

    // 4. Legacy opaque single-slash form -> custom scheme with authority.
    out = OPAQUE_RE
        .replace_all(&out, format!("{RESOURCE_SCHEME}://file/$rest"))
        .into_owned();

    out
}

/// Errors returned while resolving a webview resource URL to a file on disk.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("not a {RESOURCE_SCHEME} url: {0}")]
    WrongScheme(String),
    #[error("resource path is empty")]
    EmptyPath,
    #[error("resource {0} is outside the allowed resource roots")]
    OutsideRoots(String),
    #[error("resource {0} could not be read: {1}")]
    Io(String, String),
}

/// Extract the absolute filesystem path encoded in a [`RESOURCE_SCHEME`] URL.
///
/// Accepts `vscode-resource://<authority>/<abs path>` and
/// `vscode-resource:///<abs path>`. Query and fragment are stripped and the
/// path is percent-decoded. The returned path is lexically normalized
/// (`.`/`..` resolved) but not yet checked against any root.
pub fn path_from_uri(uri: &str) -> Result<PathBuf, ResolveError> {
    let rest = uri
        .strip_prefix(&format!("{RESOURCE_SCHEME}://"))
        .or_else(|| uri.strip_prefix(&format!("{RESOURCE_SCHEME}:")))
        .ok_or_else(|| ResolveError::WrongScheme(uri.to_string()))?;

    // Drop query / fragment.
    let rest = rest
        .split(['?', '#'])
        .next()
        .unwrap_or("");

    // Split off the authority (everything up to the first `/`). Whatever is
    // left, starting at that `/`, is the absolute filesystem path.
    let path_part = match rest.find('/') {
        Some(idx) => &rest[idx..],
        None => return Err(ResolveError::EmptyPath),
    };

    let decoded = percent_decode_str(path_part)
        .decode_utf8_lossy()
        .into_owned();
    if decoded.is_empty() || decoded == "/" {
        return Err(ResolveError::EmptyPath);
    }

    Ok(lexical_normalize(Path::new(&decoded)))
}

/// Resolve a [`RESOURCE_SCHEME`] URL to a real file path, enforcing that it
/// lives under one of `roots`. Symlinks are resolved and re-checked so a link
/// inside a root cannot escape it.
pub fn resolve_resource_path(uri: &str, roots: &[PathBuf]) -> Result<PathBuf, ResolveError> {
    let requested = path_from_uri(uri)?;

    let canon_roots: Vec<PathBuf> = roots
        .iter()
        .map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| lexical_normalize(r)))
        .collect();

    // Lexical containment check first (cheap, defeats `..` traversal).
    if !canon_roots.iter().any(|r| requested.starts_with(r)) {
        // The requested path may itself be a symlink target outside; still try
        // canonicalizing to compare, but if lexical check fails we only accept
        // it when the canonical form is under a root.
        match std::fs::canonicalize(&requested) {
            Ok(real) if canon_roots.iter().any(|r| real.starts_with(r)) => return Ok(real),
            _ => return Err(ResolveError::OutsideRoots(uri.to_string())),
        }
    }

    // Lexically inside a root: resolve symlinks and re-check if the file exists.
    match std::fs::canonicalize(&requested) {
        Ok(real) => {
            if canon_roots.iter().any(|r| real.starts_with(r)) {
                Ok(real)
            } else {
                Err(ResolveError::OutsideRoots(uri.to_string()))
            }
        }
        // File does not exist (yet): the lexical path was already proven to be
        // under a root, so report it as a (missing) in-root path.
        Err(_) => Ok(requested),
    }
}

/// Lexically normalize a path: make component-wise, resolving `.` and `..`
/// without touching the filesystem. Absolute paths stay absolute.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                // Pop a normal component; keep going if we're at the root.
                if !out.pop() {
                    // nothing to pop (root or empty) -> ignore to stay in-tree
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Best-effort MIME type from a file extension, covering the asset types that
/// extension webviews load. Falls back to `application/octet-stream`.
pub fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs" | "cjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",
        Some("wasm") => "application/wasm",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("eot") => "application/vnd.ms-fontobject",
        Some("txt" | "log") => "text/plain; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_cdn_resource_url() {
        let html = r#"<link href="https://file+.vscode-resource.vscode-cdn.net/Users/a/b/style.css">"#;
        let out = rewrite_resource_uris(html);
        assert_eq!(
            out,
            r#"<link href="vscode-resource://file/Users/a/b/style.css">"#
        );
    }

    #[test]
    fn rewrites_csp_wildcard_source() {
        let html = "default-src 'none'; style-src https://*.vscode-resource.vscode-cdn.net;";
        let out = rewrite_resource_uris(html);
        assert_eq!(out, "default-src 'none'; style-src vscode-resource:;");
    }

    #[test]
    fn rewrites_legacy_webview_resource_scheme() {
        let html = r#"<img src="vscode-webview-resource://abc/Users/a/img.png">"#;
        let out = rewrite_resource_uris(html);
        assert_eq!(out, r#"<img src="vscode-resource://abc/Users/a/img.png">"#);
    }

    #[test]
    fn rewrites_legacy_opaque_single_slash() {
        let html = r#"<img src="vscode-resource:/Users/a/img.png">"#;
        let out = rewrite_resource_uris(html);
        assert_eq!(out, r#"<img src="vscode-resource://file/Users/a/img.png">"#);
    }

    #[test]
    fn preserves_bare_scheme_source_in_csp() {
        // A bare `vscode-resource:` scheme-source (followed by `;`) must not be
        // turned into a `//file/` path.
        let html = "img-src vscode-resource: https:;";
        let out = rewrite_resource_uris(html);
        assert_eq!(out, "img-src vscode-resource: https:;");
    }

    #[test]
    fn path_from_uri_extracts_fs_path() {
        let p = path_from_uri("vscode-resource://file/Users/a/b/style.css").unwrap();
        assert_eq!(p, PathBuf::from("/Users/a/b/style.css"));
    }

    #[test]
    fn path_from_uri_percent_decodes() {
        let p = path_from_uri("vscode-resource://file/Users/a/b/my%20file.css").unwrap();
        assert_eq!(p, PathBuf::from("/Users/a/b/my file.css"));
    }

    #[test]
    fn path_from_uri_strips_query_and_fragment() {
        let p = path_from_uri("vscode-resource://file/Users/a/x.js?v=1#frag").unwrap();
        assert_eq!(p, PathBuf::from("/Users/a/x.js"));
    }

    #[test]
    fn path_from_uri_resolves_dotdot_lexically() {
        let p = path_from_uri("vscode-resource://file/Users/a/../b/x.css").unwrap();
        assert_eq!(p, PathBuf::from("/Users/b/x.css"));
    }

    #[test]
    fn path_from_uri_rejects_wrong_scheme() {
        let err = path_from_uri("https://example.com/x").unwrap_err();
        assert!(matches!(err, ResolveError::WrongScheme(_)));
    }

    #[test]
    fn resolve_allows_file_under_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let file = root.join("style.css");
        std::fs::write(&file, b"body{}").unwrap();

        let uri = format!(
            "vscode-resource://file{}",
            file.to_string_lossy()
        );
        let resolved = resolve_resource_path(&uri, &[root.clone()]).unwrap();
        assert_eq!(
            std::fs::canonicalize(&file).unwrap(),
            resolved
        );
    }

    #[test]
    fn resolve_rejects_traversal_outside_root() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("allowed");
        std::fs::create_dir(&root).unwrap();
        let secret = base.path().join("secret.txt");
        std::fs::write(&secret, b"top secret").unwrap();

        // Try to escape the root with `..`.
        let uri = format!(
            "vscode-resource://file{}/../secret.txt",
            root.to_string_lossy()
        );
        let err = resolve_resource_path(&uri, &[root.clone()]).unwrap_err();
        assert!(matches!(err, ResolveError::OutsideRoots(_)));
    }

    #[test]
    fn resolve_rejects_sibling_prefix_root() {
        // `/tmp/rootx` must not be accepted just because it shares a textual
        // prefix with root `/tmp/root`.
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let sibling = base.path().join("rootx");
        std::fs::create_dir(&sibling).unwrap();
        let file = sibling.join("x.css");
        std::fs::write(&file, b"body{}").unwrap();

        let uri = format!("vscode-resource://file{}", file.to_string_lossy());
        let err = resolve_resource_path(&uri, &[root.clone()]).unwrap_err();
        assert!(matches!(err, ResolveError::OutsideRoots(_)));
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type_for(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(
            content_type_for(Path::new("a.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for(Path::new("a.png")), "image/png");
        assert_eq!(content_type_for(Path::new("a.unknown")), "application/octet-stream");
    }
}
