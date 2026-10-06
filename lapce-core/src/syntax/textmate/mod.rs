//! A permissive, pure-Rust TextMate grammar engine used as an offline
//! highlighting fallback when a tree-sitter grammar is unavailable.
//!
//! * [`grammar`] — the grammar model, compiler and tokenization engine (built on
//!   the permissive [`fancy_regex`] crate).
//! * [`registry`] — bundled built-in grammars, loading grammars from a VS Code
//!   extensions directory, and lookup by language / scope / extension.
//! * [`scopes`] — normalization of TextMate scope names into the lapce
//!   [`crate::style::SCOPES`] vocabulary.

pub mod grammar;
pub mod registry;
pub mod scopes;

pub use grammar::{TextMateError, TextMateGrammar};
pub use registry::Registry;
pub use scopes::{map_single, textmate_scope_to_lapce};

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Collect `(text, lapce_scope)` tuples for every highlighted span.
    fn tokens(grammar: &TextMateGrammar, text: &str) -> Vec<(String, String)> {
        let spans = grammar.highlight_str(text);
        spans
            .iter_chunks(0..text.len())
            .map(|(iv, style)| {
                (
                    text[iv.start()..iv.end()].to_string(),
                    style.fg_color.clone().unwrap(),
                )
            })
            .collect()
    }

    fn has_token(tokens: &[(String, String)], text: &str, scope: &str) -> bool {
        tokens.iter().any(|(t, s)| t == text && s == scope)
    }

    #[test]
    fn builtins_register() {
        let registry = Registry::with_builtins();
        assert!(registry.grammar_for_language("ini").is_some());
        assert!(registry.grammar_for_language("json").is_some());
        assert!(registry.grammar_for_language("css").is_some());
        assert!(registry.grammar_for_scope("source.ini").is_some());
        assert!(registry.grammar_for_extension("json").is_some());
        assert_eq!(
            registry.grammar_for_scope("source.css").unwrap().scope_name(),
            "source.css"
        );
    }

    #[test]
    fn tokenize_ini() {
        let registry = Registry::with_builtins();
        let grammar = registry.grammar_for_language("ini").unwrap();
        let text = "; a comment\n[section]\nkey = \"value\"\n";
        let toks = tokens(grammar, text);

        // `; a comment` -> the content is a comment.
        assert!(
            toks.iter()
                .any(|(t, s)| s == "comment" && t.contains("comment")),
            "expected a comment token, got {toks:?}"
        );
        // Section name.
        assert!(
            has_token(&toks, "section", "text.title"),
            "expected section title token, got {toks:?}"
        );
        // Key and `=` separator.
        assert!(
            has_token(&toks, "key", "keyword"),
            "expected keyword key token, got {toks:?}"
        );
        assert!(
            has_token(&toks, "=", "punctuation.delimiter"),
            "expected delimiter token, got {toks:?}"
        );
        // Quoted string body.
        assert!(
            has_token(&toks, "value", "string"),
            "expected string token, got {toks:?}"
        );
    }

    #[test]
    fn tokenize_json() {
        let registry = Registry::with_builtins();
        let grammar = registry.grammar_for_language("json").unwrap();
        let text = "{\"name\": \"bob\", \"ok\": true, \"num\": 42}";
        let toks = tokens(grammar, text);

        // Object key -> property.
        assert!(
            has_token(&toks, "name", "property"),
            "expected property key token, got {toks:?}"
        );
        // String value.
        assert!(
            has_token(&toks, "bob", "string"),
            "expected string value token, got {toks:?}"
        );
        // Language constant.
        assert!(
            has_token(&toks, "true", "constant"),
            "expected constant token, got {toks:?}"
        );
        // Numeric constant.
        assert!(
            has_token(&toks, "42", "constant"),
            "expected numeric token, got {toks:?}"
        );
    }

    #[test]
    fn css_loads_and_tokenizes_without_panic() {
        let registry = Registry::with_builtins();
        let grammar = registry.grammar_for_language("css").unwrap();
        // Should not panic and should produce at least one highlighted span.
        let toks = tokens(grammar, "body { color: #fff; /* c */ }\n");
        assert!(!toks.is_empty(), "expected some css tokens");
    }

    #[test]
    fn load_from_extensions_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("tiny-lang");
        let syntaxes = ext_dir.join("syntaxes");
        fs::create_dir_all(&syntaxes).unwrap();

        fs::write(
            ext_dir.join("package.json"),
            r#"{
              "name": "tiny-lang",
              "contributes": {
                "languages": [{ "id": "tiny", "extensions": [".tiny"] }],
                "grammars": [{
                  "language": "tiny",
                  "scopeName": "source.tiny",
                  "path": "./syntaxes/tiny.tmLanguage.json"
                }]
              }
            }"#,
        )
        .unwrap();

        fs::write(
            syntaxes.join("tiny.tmLanguage.json"),
            r#"{
              "scopeName": "source.tiny",
              "patterns": [
                { "match": "\\bhello\\b", "name": "keyword.control.tiny" }
              ]
            }"#,
        )
        .unwrap();

        let mut registry = Registry::new();
        let count = registry.load_from_extensions_dir(dir.path()).unwrap();
        assert_eq!(count, 1);

        assert!(registry.grammar_for_language("tiny").is_some());
        assert!(registry.grammar_for_scope("source.tiny").is_some());
        let grammar = registry.grammar_for_extension("tiny").unwrap();

        let toks = tokens(grammar, "hello world\n");
        assert!(
            has_token(&toks, "hello", "keyword"),
            "expected keyword token, got {toks:?}"
        );
    }
}
