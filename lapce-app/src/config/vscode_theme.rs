//! Loading and conversion of Visual Studio Code color themes into ide's
//! (Lapce) color-theme model.
//!
//! VS Code themes are JSON (really JSONC: `//` comments and trailing commas are
//! allowed) documents with an optional `include` chain and three sources of
//! colors:
//!   * `colors`              - workbench UI colors keyed by VS Code color ids
//!   * `tokenColors`         - TextMate scope -> foreground/fontStyle rules
//!   * `semanticTokenColors` - semantic token type -> color rules
//!
//! ide instead resolves colors against a fixed vocabulary: a flat `ui` map keyed
//! by the [`crate::config::color::LapceColor`] ids, and a `syntax` map keyed by
//! the tree-sitter highlight scope names from `lapce_core::style::SCOPES`
//! (plus a few extra keys the editor queries, such as the bracket colors and the
//! completion-kind keys).
//!
//! This module reads a VS Code theme (following its `include` chain), merges the
//! layers the way VS Code does, then projects the result onto a
//! [`ColorThemeConfig`]:
//!   * each ide UI key is resolved from a prioritized list of candidate VS Code
//!     color ids (see [`build_ui`]), falling back to editor fg/bg or a constant;
//!   * each ide syntax key is resolved by TextMate-selector matching a
//!     representative scope against `tokenColors` (then `semanticTokenColors`),
//!     falling back to the editor foreground.
//!
//! The VS Code default themes "Dark Modern" and "Light Modern" (MIT, bundled from
//! `vendor/vscode/extensions/theme-defaults`) are embedded and exposed via
//! [`default_dark_modern`] / [`default_light_modern`]. Extension-contributed
//! themes (`package.json` `contributes.themes`) are discovered with
//! [`load_extension_themes`].

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use super::color_theme::{ColorThemeConfig, ThemeBaseConfig};

// ---------------------------------------------------------------------------
// Embedded VS Code default themes (MIT, from vscode/extensions/theme-defaults).
// The include chains are:
//   dark_modern  -> dark_plus  -> dark_vs
//   light_modern -> light_plus -> light_vs
// ---------------------------------------------------------------------------

const DARK_MODERN: &str = include_str!("../../../defaults/vscode-themes/dark_modern.json");
const DARK_PLUS: &str = include_str!("../../../defaults/vscode-themes/dark_plus.json");
const DARK_VS: &str = include_str!("../../../defaults/vscode-themes/dark_vs.json");
const LIGHT_MODERN: &str =
    include_str!("../../../defaults/vscode-themes/light_modern.json");
const LIGHT_PLUS: &str = include_str!("../../../defaults/vscode-themes/light_plus.json");
const LIGHT_VS: &str = include_str!("../../../defaults/vscode-themes/light_vs.json");

fn embedded_theme(name: &str) -> Option<&'static str> {
    // `name` is the bare file name referenced by an `include` entry, e.g.
    // "./dark_plus.json" -> "dark_plus.json".
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    Some(match name {
        "dark_modern.json" => DARK_MODERN,
        "dark_plus.json" => DARK_PLUS,
        "dark_vs.json" => DARK_VS,
        "light_modern.json" => LIGHT_MODERN,
        "light_plus.json" => LIGHT_PLUS,
        "light_vs.json" => LIGHT_VS,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Raw (deserialized) VS Code theme document.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct RawTheme {
    name: Option<String>,
    include: Option<String>,
    #[serde(rename = "type")]
    theme_type: Option<String>,
    #[serde(default)]
    colors: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "tokenColors")]
    token_colors: Vec<RawTokenColor>,
    #[serde(default, rename = "semanticTokenColors")]
    semantic_token_colors: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct RawTokenColor {
    #[serde(default)]
    scope: ScopeField,
    #[serde(default)]
    settings: TokenSettings,
}

impl RawTokenColor {
    /// The individual TextMate selectors this rule applies to. A `scope` string
    /// may itself be a comma-separated list of selectors.
    fn selectors(&self) -> Vec<&str> {
        let raw: Vec<&str> = match &self.scope {
            ScopeField::One(s) => s.split(',').collect(),
            ScopeField::Many(v) => {
                v.iter().flat_map(|s| s.split(',')).collect()
            }
            ScopeField::None => Vec::new(),
        };
        raw.into_iter().map(str::trim).filter(|s| !s.is_empty()).collect()
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ScopeField {
    One(String),
    Many(Vec<String>),
    None,
}

impl Default for ScopeField {
    fn default() -> Self {
        ScopeField::None
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
struct TokenSettings {
    foreground: Option<String>,
    #[serde(rename = "fontStyle")]
    #[allow(dead_code)]
    font_style: Option<String>,
}

// ---------------------------------------------------------------------------
// Merged theme (include chain flattened).
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MergedTheme {
    name: Option<String>,
    theme_type: Option<String>,
    colors: BTreeMap<String, String>,
    token_colors: Vec<RawTokenColor>,
    semantic: BTreeMap<String, String>,
}

impl MergedTheme {
    fn overlay(&mut self, raw: RawTheme) {
        for (k, v) in raw.colors {
            if let Some(c) = value_as_color(&v) {
                self.colors.insert(k, c);
            }
        }
        // VS Code appends the including theme's token rules after the included
        // ones; later rules win on ties, matching our selector resolution.
        self.token_colors.extend(raw.token_colors);
        for (k, v) in raw.semantic_token_colors {
            if let Some(c) = semantic_as_color(&v) {
                self.semantic.insert(k, c);
            }
        }
        if raw.name.is_some() {
            self.name = raw.name;
        }
        if raw.theme_type.is_some() {
            self.theme_type = raw.theme_type;
        }
    }
}

fn parse_jsonc(content: &str) -> Result<RawTheme, String> {
    json5::from_str::<RawTheme>(content).map_err(|e| e.to_string())
}

/// Resolve a theme and its `include` chain where each referenced document is
/// fetched by name from the embedded set.
fn resolve_embedded(content: &str) -> Result<MergedTheme, String> {
    let raw = parse_jsonc(content)?;
    let mut merged = MergedTheme::default();
    if let Some(include) = raw.include.clone() {
        if let Some(parent) = embedded_theme(&include) {
            merged = resolve_embedded(parent)?;
        }
    }
    merged.overlay(raw);
    Ok(merged)
}

/// Resolve a theme and its `include` chain from disk; each `include` is resolved
/// relative to the including file's directory.
fn resolve_path(path: &Path) -> Result<MergedTheme, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let raw = parse_jsonc(&content)?;
    let mut merged = MergedTheme::default();
    if let Some(include) = raw.include.clone() {
        let include_path = path
            .parent()
            .map(|p| p.join(&include))
            .unwrap_or_else(|| PathBuf::from(&include));
        if include_path.exists() {
            merged = resolve_path(&include_path)?;
        }
    }
    merged.overlay(raw);
    Ok(merged)
}

// ---------------------------------------------------------------------------
// Color helpers.
// ---------------------------------------------------------------------------

fn value_as_color(v: &serde_json::Value) -> Option<String> {
    normalize_hex(v.as_str()?)
}

/// `semanticTokenColors` values are either a color string or a settings object
/// `{ "foreground": "#..", "fontStyle": ".." }`.
fn semantic_as_color(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => normalize_hex(s),
        serde_json::Value::Object(map) => {
            normalize_hex(map.get("foreground")?.as_str()?)
        }
        _ => None,
    }
}

/// Normalize a VS Code color literal to a 6- or 8-digit lowercase `#rrggbb(aa)`
/// hex string, expanding the 3/4-digit short forms. Returns `None` for anything
/// that is not a hex literal (e.g. a `null` unset).
fn normalize_hex(s: &str) -> Option<String> {
    let s = s.trim();
    let h = s.strip_prefix('#')?;
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let expanded = match h.len() {
        3 | 4 => h.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 | 8 => h.to_string(),
        _ => return None,
    };
    Some(format!("#{}", expanded.to_lowercase()))
}

/// Blend a straight alpha byte onto a resolved `#rrggbb` color, producing
/// `#rrggbbaa`. If the color already carries alpha it is left unchanged.
fn with_alpha(color: &str, alpha: &str) -> String {
    let h = color.strip_prefix('#').unwrap_or(color);
    if h.len() >= 8 {
        return color.to_string();
    }
    format!("#{}{alpha}", &h[..h.len().min(6)])
}

// ---------------------------------------------------------------------------
// TextMate selector matching for syntax colors.
// ---------------------------------------------------------------------------

/// How specifically `selector` matches `scope`, as a count of matched dotted
/// segments; `None` if it does not match. A selector matches a scope when the
/// selector's right-most atom is a dotted-prefix of the scope (TextMate rule).
fn selector_specificity(selector: &str, scope: &str) -> Option<usize> {
    // Descendant selectors ("source.cpp keyword"): only the right-most atom is
    // matched against our single representative scope.
    let atom = selector.rsplit(' ').next().unwrap_or(selector).trim();
    if atom.is_empty() {
        // The empty selector is the theme default; treat as a weak match.
        return Some(0);
    }
    if scope == atom || scope.starts_with(&format!("{atom}.")) {
        Some(atom.split('.').count())
    } else {
        None
    }
}

/// Resolve the foreground for `scope` against `token_colors` using TextMate
/// precedence (most specific selector wins; later rule wins on a tie).
fn match_token_scope(
    token_colors: &[RawTokenColor],
    scope: &str,
) -> Option<String> {
    let mut best: Option<(usize, usize, String)> = None;
    for (order, tc) in token_colors.iter().enumerate() {
        let Some(fg) = tc.settings.foreground.as_ref() else {
            continue;
        };
        let Some(color) = normalize_hex(fg) else {
            continue;
        };
        for selector in tc.selectors() {
            if let Some(spec) = selector_specificity(selector, scope) {
                let better = match &best {
                    None => true,
                    Some((bspec, border, _)) => {
                        spec > *bspec || (spec == *bspec && order >= *border)
                    }
                };
                if better {
                    best = Some((spec, order, color.clone()));
                }
            }
        }
    }
    best.map(|(_, _, c)| c)
}

// ---------------------------------------------------------------------------
// Projection onto ide's color-theme model.
// ---------------------------------------------------------------------------

/// `(ide syntax key, representative TextMate scope, optional semantic token type)`
///
/// The union covers every scope name in `lapce_core::style::SCOPES`, the extra
/// keys the stock dark theme defines, and the completion-kind keys queried by
/// `LapceConfig::completion_color`.
const SYNTAX_MAP: &[(&str, &str, Option<&str>)] = &[
    ("comment", "comment", Some("comment")),
    ("constant", "constant.language", None),
    ("number", "constant.numeric", Some("number")),
    ("type", "entity.name.type", Some("type")),
    ("typeAlias", "entity.name.type", Some("type")),
    ("type.builtin", "support.type", Some("type.defaultLibrary")),
    ("builtinType", "support.type", Some("type.defaultLibrary")),
    ("enum", "entity.name.type.enum", Some("enum")),
    ("struct", "entity.name.type.struct", Some("struct")),
    ("structure", "entity.name.type.struct", Some("struct")),
    ("class", "entity.name.class", Some("class")),
    ("interface", "entity.name.type.interface", Some("interface")),
    ("attribute", "entity.other.attribute-name", None),
    ("constructor", "entity.name.function", None),
    ("function", "entity.name.function", Some("function")),
    ("method", "entity.name.function", Some("method")),
    ("function.method", "entity.name.function", Some("method")),
    ("label", "entity.name.label", None),
    ("keyword", "keyword", Some("keyword")),
    ("selfKeyword", "variable.language", None),
    ("string", "string", Some("string")),
    ("string.escape", "constant.character.escape", None),
    ("escape", "constant.character.escape", None),
    ("field", "variable.other.member", Some("property")),
    ("property", "variable.other.property", Some("property")),
    ("variable.other.member", "variable.other.member", Some("property")),
    ("enumMember", "variable.other.enummember", Some("enumMember")),
    ("enum-member", "variable.other.enummember", Some("enumMember")),
    ("variable", "variable", Some("variable")),
    ("operator", "keyword.operator", Some("operator")),
    ("embedded", "meta.embedded", None),
    ("symbol", "constant.other.symbol", None),
    ("snippet", "", None),
    ("punctuation", "punctuation", None),
    ("punctuation.special", "punctuation.section", None),
    ("punctuation.delimiter", "punctuation.separator", None),
    ("tag", "entity.name.tag", None),
    ("text", "", None),
    ("text.literal", "markup.inline.raw", None),
    ("text.title", "markup.heading", None),
    ("text.uri", "markup.underline.link", None),
    ("text.reference", "markup.underline.link", None),
    ("markup.heading", "markup.heading", None),
    ("markup.bold", "markup.bold", None),
    ("markup.italic", "markup.italic", None),
    ("markup.list", "markup.list", None),
    ("markup.quote", "markup.quote", None),
    ("markup.link.url", "markup.underline.link", None),
    ("markup.link.label", "markup.underline.link", None),
    ("markup.link.text", "string.other.link", None),
];

fn build_syntax(m: &MergedTheme) -> BTreeMap<String, String> {
    let fg = m
        .colors
        .get("editor.foreground")
        .cloned()
        .unwrap_or_else(|| "#cccccc".to_string());

    let mut syntax = BTreeMap::new();
    for (key, scope, semantic) in SYNTAX_MAP {
        let color = (!scope.is_empty())
            .then(|| match_token_scope(&m.token_colors, scope))
            .flatten()
            .or_else(|| semantic.and_then(|s| m.semantic.get(s).cloned()))
            .unwrap_or_else(|| fg.clone());
        syntax.insert((*key).to_string(), color);
    }

    // Bracket pair colorization colors live in the syntax map too.
    let bracket = |cands: &[&str], fallback: &str| -> String {
        pick(&m.colors, cands).unwrap_or_else(|| fallback.to_string())
    };
    syntax.insert(
        "bracket.color.1".to_string(),
        bracket(&["editorBracketHighlight.foreground1"], &fg),
    );
    syntax.insert(
        "bracket.color.2".to_string(),
        bracket(&["editorBracketHighlight.foreground2"], &fg),
    );
    syntax.insert(
        "bracket.color.3".to_string(),
        bracket(&["editorBracketHighlight.foreground3"], &fg),
    );
    syntax.insert(
        "bracket.unpaired".to_string(),
        bracket(
            &[
                "editorBracketHighlight.unexpectedBracket.foreground",
                "errorForeground",
            ],
            "#ff5555",
        ),
    );

    syntax
}

fn pick(colors: &BTreeMap<String, String>, candidates: &[&str]) -> Option<String> {
    candidates.iter().find_map(|c| colors.get(*c).cloned())
}

fn build_ui(m: &MergedTheme) -> BTreeMap<String, String> {
    let colors = &m.colors;
    let fg = colors
        .get("editor.foreground")
        .cloned()
        .unwrap_or_else(|| "#cccccc".to_string());
    let bg = colors
        .get("editor.background")
        .cloned()
        .unwrap_or_else(|| "#1f1f1f".to_string());

    let mut ui = BTreeMap::new();
    let mut set = |key: &str, cands: &[&str], fallback: &str| {
        let value = pick(colors, cands).unwrap_or_else(|| fallback.to_string());
        ui.insert(key.to_string(), value);
    };

    // lapce.*
    set("lapce.error", &["errorForeground", "editorError.foreground"], "#f14c4c");
    set(
        "lapce.warn",
        &["editorWarning.foreground", "list.warningForeground"],
        "#cca700",
    );
    set("lapce.dropdown_shadow", &["widget.shadow"], "#00000000");
    set(
        "lapce.border",
        &["editorGroup.border", "panel.border", "contrastBorder"],
        &bg,
    );
    set("lapce.scroll_bar", &["scrollbarSlider.background"], "#79797966");
    set("lapce.button.primary.background", &["button.background"], "#0078d4");
    set("lapce.button.primary.foreground", &["button.foreground"], "#ffffff");

    set(
        "lapce.tab.active.background",
        &["tab.activeBackground", "editor.background"],
        &bg,
    );
    set("lapce.tab.active.foreground", &["tab.activeForeground"], &fg);
    set(
        "lapce.tab.active.underline",
        &["tab.activeBorderTop", "tab.activeBorder", "focusBorder"],
        "#0078d4",
    );
    set(
        "lapce.tab.inactive.background",
        &["tab.inactiveBackground", "editorGroupHeader.tabsBackground"],
        &bg,
    );
    set("lapce.tab.inactive.foreground", &["tab.inactiveForeground"], &fg);
    set(
        "lapce.tab.inactive.underline",
        &["tab.unfocusedActiveBorderTop", "editorGroupHeader.tabsBorder"],
        "#00000000",
    );
    set("lapce.tab.separator", &["tab.border"], "");

    set("lapce.icon.active", &["icon.foreground", "foreground"], &fg);
    set(
        "lapce.icon.inactive",
        &["disabledForeground", "descriptionForeground"],
        "#808080",
    );

    set("lapce.remote.icon", &["statusBarItem.remoteForeground"], "#ffffff");
    set("lapce.remote.local", &["statusBarItem.remoteBackground"], "#0078d4");
    set("lapce.remote.connected", &["statusBarItem.remoteBackground"], "#16825d");
    set("lapce.remote.connecting", &["editorWarning.foreground"], "#c18401");
    set("lapce.remote.disconnected", &["errorForeground"], "#e45649");

    set("lapce.plugin.name", &["foreground"], &fg);
    set("lapce.plugin.description", &["foreground"], &fg);
    set("lapce.plugin.author", &["descriptionForeground"], "#808080");

    // editor.*
    set("editor.background", &["editor.background"], &bg);
    set("editor.foreground", &["editor.foreground"], &fg);
    set(
        "editor.dim",
        &["editorLineNumber.foreground", "descriptionForeground"],
        "#6e7681",
    );
    set("editor.focus", &["editor.foreground"], &fg);
    set("editor.caret", &["editorCursor.foreground", "foreground"], &fg);
    set(
        "editor.selection",
        &["editor.selectionBackground", "editor.inactiveSelectionBackground"],
        "#264f78",
    );
    set(
        "editor.current_line",
        &["editor.lineHighlightBackground", "editor.lineHighlightBorder"],
        &bg,
    );
    set(
        "editor.debug_break_line",
        &["editor.stackFrameHighlightBackground"],
        "#528abf37",
    );
    set("editor.link", &["textLink.foreground", "editorLink.activeForeground"], "#4daafc");
    set("editor.visible_whitespace", &["editorWhitespace.foreground"], "#3e4451");
    set(
        "editor.indent_guide",
        &["editorIndentGuide.background1", "editorIndentGuide.background"],
        "#404040",
    );
    set("editor.drag_drop_background", &["editor.dragAndDropBackground"], "#79c1fc55");
    set("editor.drag_drop_tab_background", &[], "#0b0e1455");
    set(
        "editor.sticky_header_background",
        &["editorStickyScroll.background", "editor.background"],
        &bg,
    );

    set("inlay_hint.foreground", &["editorInlayHint.foreground"], &fg);
    set("inlay_hint.background", &["editorInlayHint.background"], "#528abf37");

    set(
        "completion_lens.foreground",
        &["editorGhostText.foreground", "editorInlayHint.foreground", "descriptionForeground"],
        "#6e7681",
    );

    set(
        "source_control.added",
        &["gitDecoration.addedResourceForeground", "editorGutter.addedBackground"],
        "#50a14fcc",
    );
    set(
        "source_control.removed",
        &["gitDecoration.deletedResourceForeground", "editorGutter.deletedBackground"],
        "#ff5266cc",
    );
    set(
        "source_control.modified",
        &["gitDecoration.modifiedResourceForeground", "editorGutter.modifiedBackground"],
        "#0184bccc",
    );

    set(
        "tooltip.background",
        &["editorHoverWidget.background", "editorWidget.background"],
        &bg,
    );
    set("tooltip.foreground", &["editorHoverWidget.foreground", "foreground"], &fg);

    // palette / quick input
    set(
        "palette.background",
        &["quickInput.background", "editorWidget.background", "dropdown.listBackground"],
        &bg,
    );
    set("palette.foreground", &["quickInput.foreground", "foreground"], &fg);
    set(
        "palette.current.background",
        &["list.activeSelectionBackground", "list.focusBackground"],
        "#04395e",
    );
    set(
        "palette.current.foreground",
        &["list.activeSelectionForeground", "foreground"],
        &fg,
    );

    set(
        "completion.background",
        &["editorSuggestWidget.background", "editorWidget.background"],
        &bg,
    );
    set(
        "completion.current",
        &["editorSuggestWidget.selectedBackground", "list.activeSelectionBackground"],
        "#04395e",
    );

    set(
        "hover.background",
        &["editorHoverWidget.background", "editorWidget.background"],
        &bg,
    );

    set("activity.background", &["activityBar.background"], &bg);
    set(
        "activity.current",
        &["activityBar.activeBackground", "editor.background"],
        &bg,
    );

    set("debug.breakpoint", &["debugIcon.breakpointForeground"], "#e51400");
    set("debug.breakpoint.hover", &["editor.stackFrameHighlightBackground"], "#e0646466");

    // panels / side bar
    set("panel.background", &["sideBar.background", "panel.background"], &bg);
    set("panel.foreground", &["sideBar.foreground", "foreground"], &fg);
    set("panel.foreground.dim", &["descriptionForeground"], "#808080");
    set(
        "panel.current.background",
        &["list.activeSelectionBackground", "list.inactiveSelectionBackground"],
        "#04395e",
    );
    set(
        "panel.current.foreground",
        &["list.activeSelectionForeground", "foreground"],
        &fg,
    );
    set("panel.current.foreground.dim", &["descriptionForeground"], "#808080");
    set("panel.hovered.background", &["list.hoverBackground"], "#2a2d2e");
    set(
        "panel.hovered.active.background",
        &["list.hoverBackground", "list.activeSelectionBackground"],
        "#2a2d2e",
    );
    set("panel.hovered.foreground", &["list.hoverForeground", "foreground"], &fg);
    set("panel.hovered.foreground.dim", &["descriptionForeground"], "#808080");

    // status bar (non-modal values; modal entries only show in vim mode)
    set("status.background", &["statusBar.background"], &bg);
    set("status.foreground", &["statusBar.foreground", "foreground"], &fg);
    let accent = pick(colors, &["statusBar.background", "focusBorder"])
        .unwrap_or_else(|| "#0078d4".to_string());
    set("status.modal.normal.background", &["focusBorder"], &accent);
    set("status.modal.normal.foreground", &["button.foreground"], "#ffffff");
    set("status.modal.insert.background", &["errorForeground"], "#e06c75");
    set("status.modal.insert.foreground", &["button.foreground"], "#ffffff");
    set("status.modal.visual.background", &["editorWarning.foreground"], "#e5c07b");
    set("status.modal.visual.foreground", &["button.foreground"], "#000000");
    set("status.modal.terminal.background", &["terminal.ansiMagenta"], "#c678dd");
    set("status.modal.terminal.foreground", &["button.foreground"], "#000000");

    set("markdown.blockquote", &["textBlockQuote.border", "descriptionForeground"], "#898989");

    // terminal
    set(
        "terminal.cursor",
        &["terminalCursor.foreground", "terminal.foreground", "editor.foreground"],
        &fg,
    );
    set("terminal.foreground", &["terminal.foreground", "editor.foreground"], &fg);
    set("terminal.background", &["terminal.background", "editor.background"], &bg);
    set("terminal.black", &["terminal.ansiBlack"], "#000000");
    set("terminal.red", &["terminal.ansiRed"], "#cd3131");
    set("terminal.green", &["terminal.ansiGreen"], "#0dbc79");
    set("terminal.yellow", &["terminal.ansiYellow"], "#e5e510");
    set("terminal.blue", &["terminal.ansiBlue"], "#2472c8");
    set("terminal.magenta", &["terminal.ansiMagenta"], "#bc3fbc");
    set("terminal.cyan", &["terminal.ansiCyan"], "#11a8cd");
    set("terminal.white", &["terminal.ansiWhite"], "#e5e5e5");
    set("terminal.bright_black", &["terminal.ansiBrightBlack"], "#666666");
    set("terminal.bright_red", &["terminal.ansiBrightRed"], "#f14c4c");
    set("terminal.bright_green", &["terminal.ansiBrightGreen"], "#23d18b");
    set("terminal.bright_yellow", &["terminal.ansiBrightYellow"], "#f5f543");
    set("terminal.bright_blue", &["terminal.ansiBrightBlue"], "#3b8eea");
    set("terminal.bright_magenta", &["terminal.ansiBrightMagenta"], "#d670d6");
    set("terminal.bright_cyan", &["terminal.ansiBrightCyan"], "#29b8db");
    set("terminal.bright_white", &["terminal.ansiBrightWhite"], "#e5e5e5");

    // error lens (resolved colors with derived translucent backgrounds; done
    // after the `set` closure's last use to avoid overlapping borrows of `ui`)
    let err_fg = pick(colors, &["editorError.foreground", "errorForeground"])
        .unwrap_or_else(|| "#f14c4c".to_string());
    let warn_fg = pick(colors, &["editorWarning.foreground"])
        .unwrap_or_else(|| "#cca700".to_string());
    let other_fg = pick(colors, &["editorInfo.foreground", "descriptionForeground"])
        .unwrap_or_else(|| "#6e7681".to_string());
    ui.insert("error_lens.error.foreground".to_string(), err_fg.clone());
    ui.insert("error_lens.error.background".to_string(), with_alpha(&err_fg, "26"));
    ui.insert("error_lens.warning.foreground".to_string(), warn_fg.clone());
    ui.insert("error_lens.warning.background".to_string(), with_alpha(&warn_fg, "26"));
    ui.insert("error_lens.other.foreground".to_string(), other_fg.clone());
    ui.insert("error_lens.other.background".to_string(), with_alpha(&other_fg, "26"));

    ui
}

fn merged_to_color_theme(
    m: &MergedTheme,
    name_override: Option<&str>,
) -> ColorThemeConfig {
    let name = name_override
        .map(ToString::to_string)
        .or_else(|| m.name.clone())
        .unwrap_or_else(|| "VSCode Theme".to_string());

    let high_contrast = m
        .theme_type
        .as_deref()
        .map(|t| matches!(t, "hc" | "hcLight" | "hcDark"))
        .filter(|hc| *hc);

    ColorThemeConfig {
        path: PathBuf::new(),
        name,
        high_contrast,
        base: ThemeBaseConfig::default(),
        syntax: build_syntax(m),
        ui: build_ui(m),
    }
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Convert a VS Code theme document (with no `include`, or whose includes are
/// already inlined) into a [`ColorThemeConfig`].
pub fn from_str(content: &str, name_override: Option<&str>) -> Result<ColorThemeConfig, String> {
    let raw = parse_jsonc(content)?;
    let mut merged = MergedTheme::default();
    if let Some(include) = raw.include.clone() {
        if let Some(parent) = embedded_theme(&include) {
            merged = resolve_embedded(parent)?;
        }
    }
    merged.overlay(raw);
    Ok(merged_to_color_theme(&merged, name_override))
}

/// Load a VS Code theme from disk, following its `include` chain relative to the
/// file's directory.
pub fn from_path(path: &Path, name_override: Option<&str>) -> Result<ColorThemeConfig, String> {
    let merged = resolve_path(path)?;
    Ok(merged_to_color_theme(&merged, name_override))
}

/// The bundled VS Code "Dark Modern" theme.
pub fn default_dark_modern() -> ColorThemeConfig {
    let merged = resolve_embedded(DARK_MODERN).expect("embedded dark_modern theme is valid");
    merged_to_color_theme(&merged, Some("Dark Modern"))
}

/// The bundled VS Code "Light Modern" theme.
pub fn default_light_modern() -> ColorThemeConfig {
    let merged = resolve_embedded(LIGHT_MODERN).expect("embedded light_modern theme is valid");
    merged_to_color_theme(&merged, Some("Light Modern"))
}

/// A discovered extension theme contribution.
#[derive(Debug, Deserialize)]
struct ContributesTheme {
    label: Option<String>,
    path: String,
    #[serde(rename = "uiTheme")]
    #[allow(dead_code)]
    ui_theme: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtensionContributes {
    #[serde(default)]
    themes: Vec<ContributesTheme>,
}

#[derive(Debug, Deserialize)]
struct ExtensionManifest {
    #[serde(default)]
    contributes: Option<ExtensionContributes>,
}

/// Scan a VS Code extensions directory for `contributes.themes` and load each
/// contributed color theme. Each entry in the directory is an extension folder
/// containing a `package.json`.
pub fn load_extension_themes(extensions_dir: &Path) -> Vec<ColorThemeConfig> {
    let mut themes = Vec::new();
    let Ok(entries) = std::fs::read_dir(extensions_dir) else {
        return themes;
    };
    for entry in entries.flatten() {
        let ext_dir = entry.path();
        if !ext_dir.is_dir() {
            continue;
        }
        let manifest_path = ext_dir.join("package.json");
        let Ok(manifest_src) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<ExtensionManifest>(&manifest_src) else {
            continue;
        };
        let Some(contributes) = manifest.contributes else {
            continue;
        };
        for theme in contributes.themes {
            let theme_path = ext_dir.join(&theme.path);
            match from_path(theme_path.as_path(), theme.label.as_deref()) {
                Ok(config) => themes.push(config),
                Err(err) => {
                    tracing::warn!(
                        "Failed to load extension theme {theme_path:?}: {err}"
                    );
                }
            }
        }
    }
    themes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_hex() {
        assert_eq!(normalize_hex("#fff").as_deref(), Some("#ffffff"));
        assert_eq!(normalize_hex("#ccc3").as_deref(), Some("#cccccc33"));
        assert_eq!(normalize_hex("#1F1F1F").as_deref(), Some("#1f1f1f"));
        assert_eq!(normalize_hex("#1F1F1F80").as_deref(), Some("#1f1f1f80"));
        assert_eq!(normalize_hex("not a color"), None);
        assert_eq!(normalize_hex("#xyz"), None);
    }

    #[test]
    fn test_selector_specificity() {
        // exact + prefix matches
        assert_eq!(selector_specificity("keyword", "keyword.control.ts"), Some(1));
        assert_eq!(
            selector_specificity("keyword.control", "keyword.control.ts"),
            Some(2)
        );
        // selector more specific than scope -> no match
        assert_eq!(selector_specificity("keyword.control.ts", "keyword"), None);
        // descendant selector: right-most atom is used
        assert_eq!(
            selector_specificity("source.cpp keyword.operator", "keyword.operator.new"),
            Some(2)
        );
        // non match
        assert_eq!(selector_specificity("string", "keyword"), None);
    }

    #[test]
    fn test_include_chain_dark_modern() {
        // Dark Modern -> Dark+ -> Dark (Visual Studio). Each layer contributes.
        let merged = resolve_embedded(DARK_MODERN).unwrap();
        assert_eq!(merged.name.as_deref(), Some("Dark Modern"));
        // overridden by dark_modern (child wins)
        assert_eq!(merged.colors.get("editor.background").map(String::as_str), Some("#1f1f1f"));
        // only present in dark_vs (deepest include), so the chain was walked
        assert_eq!(merged.colors.get("editor.foreground").map(String::as_str), Some("#cccccc"));
        // token rules were accumulated from the includes
        assert!(!merged.token_colors.is_empty());
        // comment rule comes from dark_vs
        let comment = match_token_scope(&merged.token_colors, "comment");
        assert_eq!(comment.as_deref(), Some("#6a9955"));
        // semanticTokenColors from dark_plus
        assert_eq!(merged.semantic.get("numberLiteral").map(String::as_str), Some("#b5cea8"));
    }

    #[test]
    fn test_dark_modern_color_theme() {
        let theme = default_dark_modern();
        assert_eq!(theme.name, "Dark Modern");
        // UI keys resolve to concrete colors
        assert_eq!(theme.ui.get("editor.background").map(String::as_str), Some("#1f1f1f"));
        assert_eq!(theme.ui.get("editor.foreground").map(String::as_str), Some("#cccccc"));
        // tab active background maps from tab.activeBackground
        assert_eq!(theme.ui.get("lapce.tab.active.background").map(String::as_str), Some("#1f1f1f"));
        // syntax scopes resolve via tokenColors
        assert_eq!(theme.syntax.get("comment").map(String::as_str), Some("#6a9955"));
        assert_eq!(theme.syntax.get("keyword").map(String::as_str), Some("#569cd6"));
        assert_eq!(theme.syntax.get("string").map(String::as_str), Some("#ce9178"));
        assert_eq!(theme.syntax.get("function").map(String::as_str), Some("#dcdcaa"));
        assert_eq!(theme.syntax.get("type").map(String::as_str), Some("#4ec9b0"));
        // number falls back to semanticTokenColors (numberLiteral) since dark
        // themes color numeric constants semantically
        assert_eq!(theme.syntax.get("number").map(String::as_str), Some("#b5cea8"));
        // every ui key must be present and a valid hex so nothing leaks from the
        // stock theme fallback
        for (k, v) in &theme.ui {
            if k == "lapce.tab.separator" {
                continue; // intentionally empty
            }
            assert!(
                normalize_hex(v).is_some(),
                "ui key {k} has non-hex value {v}"
            );
        }
    }

    #[test]
    fn test_light_modern_color_theme() {
        let theme = default_light_modern();
        assert_eq!(theme.name, "Light Modern");
        assert!(theme.ui.contains_key("editor.background"));
        // light background is bright
        let bg = theme.ui.get("editor.background").unwrap();
        assert!(normalize_hex(bg).is_some());
    }
}
