//! Normalization of TextMate scope names into the lapce `SCOPES` vocabulary.
//!
//! TextMate grammars assign deeply-dotted scope names (e.g.
//! `comment.line.semicolon.ini`). lapce renders tokens by resolving a single
//! [`crate::style::SCOPES`] entry (e.g. `comment`) to a theme color. This module
//! maps a full TextMate scope *stack* (least specific first, most specific last)
//! to the best matching lapce scope, preferring the deepest scope that maps.

/// Longest-prefix table: a TextMate scope prefix and the lapce `SCOPES` entry it
/// maps to. Lookup selects the entry whose prefix is the longest scope-prefix of
/// the input, so ordering here is only for readability.
const SCOPE_MAP: &[(&str, &str)] = &[
    // comments
    ("comment", "comment"),
    // escapes (checked before the generic constant.* rules via longest-prefix)
    ("constant.character.escape", "string.escape"),
    ("constant.other.character-class.escape", "string.escape"),
    // constants
    ("constant.numeric", "constant"),
    ("constant.language", "constant"),
    ("constant.character", "constant"),
    ("support.constant", "constant"),
    ("constant", "constant"),
    // entities
    ("entity.name.function", "function"),
    ("entity.name.tag", "tag"),
    ("entity.name.type", "type"),
    ("entity.name.class", "type"),
    ("entity.name.namespace", "type"),
    ("entity.name.section", "text.title"),
    ("entity.name.label", "label"),
    ("entity.name", "function"),
    ("entity.other.attribute-name", "attribute"),
    ("entity.other.inherited-class", "type"),
    // variables
    ("variable.other.member", "variable.other.member"),
    ("variable.other.property", "variable.other.member"),
    ("variable.language", "variable"),
    ("variable.parameter", "variable"),
    ("variable", "variable"),
    // keywords / operators
    ("keyword.operator", "operator"),
    ("keyword", "keyword"),
    // storage
    ("storage.type", "type"),
    ("storage.modifier", "keyword"),
    ("storage", "keyword"),
    // strings
    ("string.escape", "string.escape"),
    ("string", "string"),
    // support
    ("support.function", "function"),
    ("support.class", "type"),
    ("support.type.property-name", "property"),
    ("support.type", "type"),
    ("support.variable", "variable"),
    // punctuation
    ("punctuation.separator", "punctuation.delimiter"),
    ("punctuation.terminator", "punctuation.delimiter"),
    ("punctuation.definition", "punctuation"),
    ("punctuation.section", "punctuation"),
    ("punctuation.whitespace", "punctuation"),
    ("punctuation", "punctuation"),
    // markup
    ("markup.heading", "markup.heading"),
    ("markup.bold", "markup.bold"),
    ("markup.italic", "markup.italic"),
    ("markup.underline.link", "markup.link.url"),
    ("markup.list", "markup.list"),
    ("markup.quote", "markup.quote"),
    ("markup.inline.raw", "text.literal"),
    ("markup.raw", "text.literal"),
    // misc
    ("meta.tag", "tag"),
    ("operator", "operator"),
    ("property", "property"),
    ("tag", "tag"),
];

/// Returns true when `prefix` is a scope-prefix of `scope`, i.e. `scope` equals
/// `prefix` or starts with `prefix` followed by a `.` separator.
fn is_scope_prefix(prefix: &str, scope: &str) -> bool {
    scope == prefix
        || (scope.len() > prefix.len()
            && scope.as_bytes()[prefix.len()] == b'.'
            && scope.starts_with(prefix))
}

/// Maps a single TextMate scope name to a lapce `SCOPES` entry using
/// longest-prefix matching. Returns `None` when nothing matches.
pub fn map_single(scope: &str) -> Option<&'static str> {
    let mut best: Option<(usize, &'static str)> = None;
    for (prefix, lapce) in SCOPE_MAP {
        if is_scope_prefix(prefix, scope) {
            let len = prefix.len();
            if best.is_none_or(|(b, _)| len > b) {
                best = Some((len, lapce));
            }
        }
    }
    best.map(|(_, lapce)| lapce)
}

/// Maps a full TextMate scope stack (most specific last) to the best lapce
/// `SCOPES` entry, preferring the deepest scope that maps. Returns `None` when
/// no scope in the stack maps to a known lapce scope.
pub fn textmate_scope_to_lapce(scope_stack: &[String]) -> Option<&'static str> {
    scope_stack.iter().rev().find_map(|scope| map_single(scope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::SCOPES;

    #[test]
    fn every_mapped_scope_is_a_valid_lapce_scope() {
        for (prefix, lapce) in SCOPE_MAP {
            assert!(
                SCOPES.contains(lapce),
                "mapping for `{prefix}` -> `{lapce}` is not a known lapce scope"
            );
        }
    }

    #[test]
    fn single_scope_longest_prefix() {
        assert_eq!(map_single("comment.line.semicolon.ini"), Some("comment"));
        assert_eq!(
            map_single("constant.character.escape.ini"),
            Some("string.escape")
        );
        assert_eq!(map_single("constant.numeric.json"), Some("constant"));
        assert_eq!(map_single("keyword.operator.css"), Some("operator"));
        assert_eq!(map_single("keyword.other.definition.ini"), Some("keyword"));
        assert_eq!(
            map_single("support.type.property-name.json"),
            Some("property")
        );
        assert_eq!(map_single("storage.type"), Some("type"));
        assert_eq!(map_single("storage.modifier"), Some("keyword"));
        assert_eq!(
            map_single("punctuation.separator.key-value.ini"),
            Some("punctuation.delimiter")
        );
        assert_eq!(
            map_single("punctuation.definition.string.begin.ini"),
            Some("punctuation")
        );
        assert_eq!(map_single("meta.structure.dictionary.json"), None);
        assert_eq!(map_single("invalid.illegal.json"), None);
    }

    #[test]
    fn scope_stack_prefers_deepest_match() {
        // string wins over the outer source scope.
        let stack = vec!["source.ini".to_string(), "string.quoted.double.ini".to_string()];
        assert_eq!(textmate_scope_to_lapce(&stack), Some("string"));

        // escape (deepest) wins over the enclosing string.
        let stack = vec![
            "source.ini".to_string(),
            "string.quoted.single.ini".to_string(),
            "constant.character.escape.ini".to_string(),
        ];
        assert_eq!(textmate_scope_to_lapce(&stack), Some("string.escape"));

        // json property-name key.
        let stack = vec![
            "source.json".to_string(),
            "string.json".to_string(),
            "support.type.property-name.json".to_string(),
        ];
        assert_eq!(textmate_scope_to_lapce(&stack), Some("property"));

        // nothing maps.
        let stack = vec!["source.json".to_string(), "meta.structure.array.json".to_string()];
        assert_eq!(textmate_scope_to_lapce(&stack), None);
    }
}
