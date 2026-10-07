//! Assembles the complete set of CSS custom properties injected into an
//! extension webview, mirroring VS Code's
//! `WebviewThemeDataProvider.getWebviewThemeData`
//! (`vs/workbench/contrib/webview/browser/themeing.ts`):
//!
//! * every registered color resolved for the active theme
//!   (`theme.getColor(id)` → `--vscode-<id with '.'→'-'>`), from the generated
//!   [`crate::theme_data`] tables,
//! * every registered size (`--vscode-fontSize-body1`, `--vscode-sash-size`, …),
//! * the font / link variables VS Code derives at runtime
//!   (`--vscode-font-family`, `--vscode-editor-font-*`, `--text-link-decoration`),
//!
//! so extension React UIs render with the host theme instead of raw,
//! browser-default HTML.

use std::collections::BTreeMap;

use crate::shim::ThemeKind;

/// macOS default UI font (VS Code `DEFAULT_FONT_FAMILY`).
pub const DEFAULT_FONT_FAMILY: &str = "-apple-system, BlinkMacSystemFont, sans-serif";
/// macOS default editor / monospace font (VS Code `DEFAULT_MAC_FONT_FAMILY`).
pub const DEFAULT_MONOSPACE_FONT: &str = "Menlo, Monaco, 'Courier New', monospace";
/// VS Code `EDITOR_FONT_DEFAULTS.fontSize` on macOS.
pub const DEFAULT_EDITOR_FONT_SIZE: u32 = 12;

fn color_table(kind: ThemeKind) -> &'static [(&'static str, &'static str)] {
    match kind {
        ThemeKind::Light | ThemeKind::HighContrastLight => crate::theme_data::LIGHT,
        ThemeKind::Dark | ThemeKind::HighContrast => crate::theme_data::DARK,
    }
}

/// The complete webview variable map for `kind` using the default fonts. The
/// editor font can be overridden with [`webview_theme_vars_with`].
pub fn webview_theme_vars(kind: ThemeKind) -> BTreeMap<String, String> {
    webview_theme_vars_with(
        kind,
        DEFAULT_MONOSPACE_FONT,
        DEFAULT_MONOSPACE_FONT,
        DEFAULT_EDITOR_FONT_SIZE,
    )
}

/// As [`webview_theme_vars`] but with the active editor font family / size
/// (from the host's configuration) and monospace font threaded through, exactly
/// as VS Code reads them from the `editor` configuration.
pub fn webview_theme_vars_with(
    kind: ThemeKind,
    editor_font_family: &str,
    monospace_font: &str,
    editor_font_size: u32,
) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for (name, value) in color_table(kind) {
        m.insert((*name).to_string(), (*value).to_string());
    }
    // Font / link variables VS Code injects via themeing.ts `styles`.
    m.insert("--vscode-font-family".into(), DEFAULT_FONT_FAMILY.into());
    m.insert("--vscode-font-weight".into(), "normal".into());
    m.insert("--vscode-font-size".into(), "13px".into());
    m.insert(
        "--vscode-editor-font-family".into(),
        editor_font_family.to_string(),
    );
    m.insert("--vscode-editor-font-weight".into(), "normal".into());
    m.insert(
        "--vscode-editor-font-size".into(),
        format!("{editor_font_size}px"),
    );
    m.insert(
        "--vscode-editor-font-feature-settings".into(),
        "normal".into(),
    );
    m.insert("--text-link-decoration".into(), "none".into());
    m.insert("--monaco-monospace-font".into(), monospace_font.to_string());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_has_core_values() {
        let m = webview_theme_vars(ThemeKind::Dark);
        assert_eq!(m.get("--vscode-editor-background").map(String::as_str), Some("#1f1f1f"));
        assert_eq!(m.get("--vscode-foreground").map(String::as_str), Some("#cccccc"));
        assert_eq!(m.get("--vscode-button-background").map(String::as_str), Some("#0078d4"));
        // Font vars are always present.
        assert_eq!(m.get("--vscode-font-family").map(String::as_str), Some(DEFAULT_FONT_FAMILY));
        assert!(m.contains_key("--vscode-editor-font-family"));
        // Size-registry var.
        assert_eq!(m.get("--vscode-fontSize-body1").map(String::as_str), Some("13px"));
    }

    #[test]
    fn light_differs_from_dark() {
        let dark = webview_theme_vars(ThemeKind::Dark);
        let light = webview_theme_vars(ThemeKind::Light);
        assert_ne!(
            dark.get("--vscode-editor-background"),
            light.get("--vscode-editor-background")
        );
    }

    #[test]
    fn editor_font_overrides_apply() {
        let m = webview_theme_vars_with(ThemeKind::Dark, "Fira Code", "Fira Code", 15);
        assert_eq!(m.get("--vscode-editor-font-family").map(String::as_str), Some("Fira Code"));
        assert_eq!(m.get("--vscode-editor-font-size").map(String::as_str), Some("15px"));
    }
}
