//! Importer for VS Code `keybindings.json` files.
//!
//! VS Code stores user keybindings as a JSONC array of objects shaped like
//! `{ "key": "cmd+k cmd+c", "command": "editor.action.commentLine",
//!    "when": "editorTextFocus", "args": {..} }`. A leading `-` on the command
//! means "unbind". This module parses that format (tolerating `//` / `/* */`
//! comments and trailing commas), translates the VS Code key chord, command id
//! and `when` clause into the ide equivalents, and renders an ide keymaps TOML
//! document that is fed back through the normal [`KeyMapLoader`] path so the
//! prefix maps stay consistent.
//!
//! `when` handling: VS Code context expressions are mapped token by token
//! against [`map_context`]. We only understand `!`, `&&` and `||` (matching the
//! ide `CheckCondition` grammar); parenthesised or comparison expressions, and
//! any clause containing an unknown context, cannot be represented. For those
//! *enabling* clauses we drop the `when` entirely (bind unconditionally) rather
//! than discarding the binding, so the shortcut still works.

use std::path::PathBuf;

use lapce_core::directory::Directory;

/// A single VS Code binding translated into ide terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedKeymap {
    /// ide key chord string accepted by `KeyMapPress::parse`.
    pub key: String,
    /// ide command id; prefixed with `-` for an unbind.
    pub command: String,
    /// mapped ide `when` clause, if any.
    pub when: Option<String>,
}



/// Path to the VS Code `keybindings.json` in the ide config directory, if it
/// exists. Mirrors [`crate::config::LapceConfig::keymaps_file`] but never
/// creates the file.
pub fn keybindings_file() -> Option<PathBuf> {
    let path = Directory::config_directory()?.join("keybindings.json");
    if path.exists() { Some(path) } else { None }
}

/// Parse a VS Code `keybindings.json` string into ide keymaps.
///
/// Entries whose key chord cannot be translated or whose command is unknown are
/// skipped with a `tracing::debug` and parsing continues.
pub fn import_vscode_keybindings(json: &str) -> Vec<ImportedKeymap> {
    let stripped = strip_trailing_commas(&strip_jsonc(json));
    let value: serde_json::Value = match serde_json::from_str(&stripped) {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!("failed to parse vscode keybindings.json: {err}");
            return Vec::new();
        }
    };
    let Some(entries) = value.as_array() else {
        tracing::debug!("vscode keybindings.json is not a JSON array");
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries {
        let Some(raw_key) = entry.get("key").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(raw_command) = entry.get("command").and_then(|v| v.as_str())
        else {
            continue;
        };

        let Some(key) = translate_chord(raw_key) else {
            tracing::debug!("vscode keybinding: unmappable key {raw_key:?}");
            continue;
        };

        let (vs_command, unbind) = match raw_command.strip_prefix('-') {
            Some(rest) => (rest, true),
            None => (raw_command, false),
        };

        let Some(ide_command) = map_command(vs_command) else {
            tracing::debug!(
                "vscode keybinding: unknown command {raw_command:?}"
            );
            continue;
        };

        let command = if unbind {
            format!("-{ide_command}")
        } else {
            ide_command.to_string()
        };

        let when = entry
            .get("when")
            .and_then(|v| v.as_str())
            .and_then(translate_when);

        out.push(ImportedKeymap { key, command, when });
    }

    out
}

/// Render imported keymaps as an ide keymaps TOML document.
pub fn imported_to_toml(imported: &[ImportedKeymap]) -> String {
    let mut array = toml_edit::ArrayOfTables::new();
    for binding in imported {
        let mut table = toml_edit::Table::new();
        table.insert("key", toml_edit::value(binding.key.clone()));
        table.insert("command", toml_edit::value(binding.command.clone()));
        if let Some(when) = &binding.when {
            table.insert("when", toml_edit::value(when.clone()));
        }
        array.push(table);
    }
    let mut doc = toml_edit::Document::new();
    doc.insert("keymaps", toml_edit::Item::ArrayOfTables(array));
    doc.to_string()
}

/// Strip `//` line comments and `/* */` block comments, respecting strings.
fn strip_jsonc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        if n == '\n' {
                            break;
                        }
                        chars.next();
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev = '\0';
                    for n in chars.by_ref() {
                        if prev == '*' && n == '/' {
                            break;
                        }
                        prev = n;
                    }
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out
}

/// Remove trailing commas before `}` or `]`, respecting strings.
fn strip_trailing_commas(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Translate a VS Code key chord (space-separated `mod+mod+key` parts) into an
/// ide chord string understood by `KeyMapPress::parse`.
fn translate_chord(vscode_key: &str) -> Option<String> {
    let mut parts = Vec::new();
    for part in vscode_key.split_whitespace() {
        let mut mods = Vec::new();
        let mut key = None;
        for token in part.split('+') {
            let token = token.trim().to_lowercase();
            match token.as_str() {
                "cmd" | "command" | "meta" | "win" | "super" => mods.push("meta"),
                "ctrl" | "control" => mods.push("ctrl"),
                "alt" | "option" | "opt" => mods.push("alt"),
                "shift" => mods.push("shift"),
                "" => key = Some("+".to_string()),
                other => key = Some(translate_keyname(other)),
            }
        }
        let key = key?;
        let mut s = String::new();
        for m in &mods {
            s.push_str(m);
            s.push('+');
        }
        s.push_str(&key);
        parts.push(s);
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" "))
}

/// Translate a VS Code key name into the ide spelling. Letters, digits and most
/// punctuation pass through unchanged (already lower-cased).
fn translate_keyname(k: &str) -> String {
    let mapped = match k {
        "oem_1" => ";",
        "oem_plus" => "=",
        "oem_comma" => ",",
        "oem_minus" => "-",
        "oem_period" => ".",
        "oem_2" => "/",
        "oem_3" => "`",
        "oem_4" => "[",
        "oem_5" | "oem_102" => "\\",
        "oem_6" => "]",
        "oem_7" => "'",
        "oem_8" => "`",
        "left" | "leftarrow" => "left",
        "right" | "rightarrow" => "right",
        "up" | "uparrow" => "up",
        "down" | "downarrow" => "down",
        "pageup" => "pageup",
        "pagedown" => "pagedown",
        "home" => "home",
        "end" => "end",
        "delete" | "del" => "delete",
        "insert" | "ins" => "insert",
        "backspace" => "backspace",
        "tab" => "tab",
        "enter" | "return" => "enter",
        "escape" | "esc" => "escape",
        "space" => "space",
        other => other,
    };
    mapped.to_string()
}

/// Translate a VS Code command id into the ide command id, if supported.
fn map_command(command: &str) -> Option<&'static str> {
    let ide = match command {
        // Quick open / palette
        "workbench.action.showCommands" => "palette.command",
        "workbench.action.quickOpen" => "palette",
        "workbench.action.gotoLine" => "palette.line",
        "workbench.action.gotoSymbol" => "palette.symbol",
        "workbench.action.showAllSymbols" => "palette.workspace_symbol",
        // Files / window
        "workbench.action.files.save" => "save",
        "workbench.action.files.saveWithoutFormatting" => "save_without_format",
        "workbench.action.files.saveAll" => "save_all",
        "workbench.action.files.newUntitledFile" => "new_file",
        "workbench.action.files.openFile"
        | "workbench.action.files.openFileFolder" => "open_file",
        "workbench.action.reloadWindow" => "reload_window",
        "workbench.action.newWindow" => "new_window",
        "workbench.action.closeWindow" => "close_window",
        "workbench.action.openSettings" => "open_settings",
        "workbench.action.openGlobalKeybindings" => "open_keyboard_shortcuts",
        // Basic editing
        "undo" => "undo",
        "redo" => "redo",
        "editor.action.clipboardCopyAction" => "clipboard_copy",
        "editor.action.clipboardCutAction" => "clipboard_cut",
        "editor.action.clipboardPasteAction" => "clipboard_paste",
        "editor.action.selectAll" => "select_all",
        "editor.action.commentLine" | "editor.action.addCommentLine" => {
            "toggle_line_comment"
        }
        "editor.action.formatDocument" => "format_document",
        "editor.action.indentLines" => "indent_line",
        "editor.action.outdentLines" => "outdent_line",
        "editor.action.deleteLines" => "delete_line",
        "editor.action.moveLinesUpAction" => "move_line_up",
        "editor.action.moveLinesDownAction" => "move_line_down",
        "editor.action.copyLinesUpAction" => "duplicate_line_up",
        "editor.action.copyLinesDownAction" => "duplicate_line_down",
        "editor.action.insertLineAfter" => "new_line_below",
        "editor.action.insertLineBefore" => "new_line_above",
        "editor.action.joinLines" => "join_lines",
        "editor.action.jumpToBracket" => "match_pairs",
        // Multi cursor
        "editor.action.insertCursorAbove" => "insert_cursor_above",
        "editor.action.insertCursorBelow" => "insert_cursor_below",
        "editor.action.addSelectionToNextFindMatch" => "select_next_current",
        "editor.action.moveSelectionToNextFindMatch" => "select_skip_current",
        "editor.action.selectHighlights" => "select_all_current",
        "cursorUndo" => "select_undo",
        // Rich language editing
        "editor.action.revealDefinition"
        | "editor.action.goToDeclaration" => "goto_definition",
        "editor.action.goToTypeDefinition" => "goto_type_definition",
        "editor.action.goToImplementation" => "go_to_implementation",
        "editor.action.goToReferences"
        | "references-view.findReferences"
        | "editor.action.referenceSearch.trigger" => "find_references",
        "editor.action.rename" => "rename_symbol",
        "editor.action.quickFix" => "show_code_actions",
        "editor.action.showHover" => "show_hover",
        "editor.action.triggerSuggest" => "get_completion",
        "editor.action.triggerParameterHints" => "get_signature",
        // Find / replace
        "actions.find" => "search",
        "editor.action.startFindReplaceAction" => "focus_replace_editor",
        "editor.action.nextMatchFindAction" => "search_forward",
        "editor.action.previousMatchFindAction" => "search_backward",
        "workbench.action.findInFiles" => "toggle_search_focus",
        // Views / panels
        "workbench.action.terminal.toggleTerminal" => "toggle_terminal_focus",
        "workbench.action.terminal.new" => "new_terminal_tab",
        "workbench.action.togglePanel" => "toggle_panel_bottom_visual",
        "workbench.action.toggleSidebarVisibility" => "toggle_panel_left_visual",
        "workbench.view.explorer" => "toggle_file_explorer_focus",
        "workbench.view.scm" => "toggle_source_control_focus",
        "workbench.view.search" => "toggle_search_focus",
        "workbench.view.extensions" => "toggle_plugin_focus",
        "workbench.actions.view.problems" => "toggle_problem_focus",
        // Editor management
        "workbench.action.splitEditor" => "split_vertical",
        "workbench.action.splitEditorDown"
        | "workbench.action.splitEditorOrthogonal" => "split_horizontal",
        "workbench.action.closeActiveEditor" => "split_close",
        "workbench.action.nextEditor" => "next_editor_tab",
        "workbench.action.previousEditor" => "previous_editor_tab",
        "workbench.action.navigateBack" => "jump_location_backward",
        "workbench.action.navigateForward" => "jump_location_forward",
        // Markers
        "editor.action.marker.next" | "editor.action.marker.nextInFiles" => {
            "next_error"
        }
        "editor.action.marker.prev" | "editor.action.marker.prevInFiles" => {
            "previous_error"
        }
        // Cursor movement
        "cursorUp" => "up",
        "cursorDown" => "down",
        "cursorLeft" => "left",
        "cursorRight" => "right",
        "cursorHome" => "line_start",
        "cursorEnd" => "line_end",
        "cursorTop" => "document_start",
        "cursorBottom" => "document_end",
        "cursorWordLeft" | "cursorWordStartLeft" => "word_backward",
        "cursorWordRight" | "cursorWordEndRight" => "word_forward",
        "deleteWordLeft" => "delete_word_backward",
        "deleteWordRight" => "delete_word_forward",
        "deleteAllLeft" => "delete_to_beginning_of_line",
        "deleteAllRight" => "delete_to_end_of_line",
        "tab" => "insert_tab",
        // Zoom
        "workbench.action.zoomIn" => "zoom_in",
        "workbench.action.zoomOut" => "zoom_out",
        "workbench.action.zoomReset" => "zoom_reset",
        _ => return None,
    };
    Some(ide)
}

/// Translate a VS Code `when` expression into the ide equivalent. Returns
/// `None` (drop the clause) when the expression uses parentheses/comparisons or
/// references an unknown context.
fn translate_when(when: &str) -> Option<String> {
    let w = when.trim();
    if w.is_empty() || w.contains('(') || w.contains(')') {
        return None;
    }

    let chars: Vec<char> = w.chars().collect();
    let mut out = String::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len()
            && ((chars[i] == '&' && chars[i + 1] == '&')
                || (chars[i] == '|' && chars[i + 1] == '|'))
        {
            out.push_str(&map_term(&cur)?);
            out.push(' ');
            out.push(chars[i]);
            out.push(chars[i + 1]);
            out.push(' ');
            cur.clear();
            i += 2;
        } else {
            cur.push(chars[i]);
            i += 1;
        }
    }
    out.push_str(&map_term(&cur)?);
    Some(out)
}

fn map_term(term: &str) -> Option<String> {
    let term = term.trim();
    let (neg, name) = match term.strip_prefix('!') {
        Some(rest) => (true, rest.trim()),
        None => (false, term),
    };
    if name.contains("==") || name.contains("=~") || name.contains(' ') {
        return None;
    }
    let ide = map_context(name)?;
    Some(if neg {
        format!("!{ide}")
    } else {
        ide.to_string()
    })
}

fn map_context(context: &str) -> Option<&'static str> {
    let ide = match context {
        "editorTextFocus" | "editorFocus" | "textInputFocus" => "editor_focus",
        "terminalFocus" => "terminal_focus",
        "inQuickOpen" | "inputFocus" | "quickInputFocus" | "inQuickInput" => {
            "palette_focus"
        }
        "listFocus" => "list_focus",
        "filesExplorerFocus" | "explorerViewletFocus" => "file_explorer_focus",
        "findWidgetVisible" | "findInputFocussed" | "findInputFocused" => {
            "search_focus"
        }
        "replaceInputFocussed" | "replaceInputFocused" => "replace_focus",
        "suggestWidgetVisible" => "completion_focus",
        "inSnippetMode" => "in_snippet",
        "inlineSuggestionVisible" => "inline_completion_visible",
        "renameInputVisible" => "rename_focus",
        "scmFocus" | "sourceControlFocus" => "source_control_focus",
        "panelFocus" => "panel_focus",
        _ => return None,
    };
    Some(ide)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keypress::keymap::KeyMapPress;

    #[test]
    fn test_import_chord_simple_when_and_unbind() {
        let json = r#"
        // user keybindings
        [
            { "key": "cmd+k cmd+c", "command": "editor.action.commentLine", "when": "editorTextFocus" },
            { "key": "cmd+s", "command": "workbench.action.files.save" },
            { "key": "cmd+h", "command": "-workbench.action.files.save" },
            { "key": "cmd+x", "command": "some.unknown.command" },
        ]
        "#;

        let imported = import_vscode_keybindings(json);
        // unknown command is dropped
        assert_eq!(imported.len(), 3);

        // chord binding -> toggle_line_comment with mapped when
        let chord = &imported[0];
        assert_eq!(chord.command, "toggle_line_comment");
        assert_eq!(chord.when.as_deref(), Some("editor_focus"));
        assert_eq!(chord.key, "meta+k meta+c");
        assert_eq!(
            KeyMapPress::parse(&chord.key),
            KeyMapPress::parse("meta+k meta+c")
        );
        assert_eq!(KeyMapPress::parse(&chord.key).len(), 2);

        // simple binding
        let save = &imported[1];
        assert_eq!(save.command, "save");
        assert_eq!(save.when, None);
        assert_eq!(save.key, "meta+s");

        // unbind -> '-' prefixed ide command
        let unbind = &imported[2];
        assert_eq!(unbind.command, "-save");
        assert_eq!(unbind.key, "meta+h");
    }

    #[test]
    fn test_when_combined_and_dropped() {
        // combined when with && and negation, all mappable
        let json = r#"[
            { "key": "escape", "command": "editor.action.quickFix", "when": "editorTextFocus && !suggestWidgetVisible" }
        ]"#;
        let imported = import_vscode_keybindings(json);
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].command, "show_code_actions");
        assert_eq!(
            imported[0].when.as_deref(),
            Some("editor_focus && !completion_focus")
        );

        // when referencing an unknown context is dropped (bind unconditionally)
        let json = r#"[
            { "key": "f4", "command": "editor.action.rename", "when": "editorLangId == 'rust'" }
        ]"#;
        let imported = import_vscode_keybindings(json);
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].command, "rename_symbol");
        assert_eq!(imported[0].when, None);
    }

    #[test]
    fn test_key_token_translation() {
        let json = r#"[
            { "key": "ctrl+alt+oem_2", "command": "editor.action.commentLine" },
            { "key": "cmd+shift+left", "command": "cursorWordLeft" }
        ]"#;
        let imported = import_vscode_keybindings(json);
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[0].key, "ctrl+alt+/");
        assert_eq!(imported[1].key, "meta+shift+left");
        assert_eq!(imported[1].command, "word_backward");
    }

    #[test]
    fn test_imported_to_toml_roundtrips_through_loader() {
        use crate::keypress::loader::KeyMapLoader;

        let json = r#"[
            { "key": "cmd+k cmd+c", "command": "editor.action.commentLine", "when": "editorTextFocus" }
        ]"#;
        let imported = import_vscode_keybindings(json);
        let toml = imported_to_toml(&imported);

        let mut loader = KeyMapLoader::new();
        loader.load_from_str(&toml, false).unwrap();
        let (keymaps, command_keymaps) = loader.finalize();

        let keypress = KeyMapPress::parse("meta+k meta+c");
        let maps = keymaps.get(&keypress).expect("chord registered");
        assert!(maps.iter().any(|k| k.command == "toggle_line_comment"));
        assert!(command_keymaps.contains_key("toggle_line_comment"));
    }
}
