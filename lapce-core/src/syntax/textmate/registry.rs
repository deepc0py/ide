//! A registry of TextMate grammars: bundled built-ins, grammars loaded from a
//! VS Code extensions directory, and lookup by language id / scope name / file
//! extension.

use std::{collections::HashMap, path::Path};

use serde::Deserialize;

use super::grammar::{TextMateError, TextMateGrammar};

/// Bundled grammar JSON embedded at compile time (see `lapce-core/grammars/`).
const INI_GRAMMAR: &str = include_str!("../../../grammars/ini.tmLanguage.json");
const JSON_GRAMMAR: &str = include_str!("../../../grammars/json.tmLanguage.json");
const CSS_GRAMMAR: &str = include_str!("../../../grammars/css.tmLanguage.json");

/// A registered grammar together with its lookup keys.
struct Entry {
    grammar: TextMateGrammar,
}

/// A registry of TextMate grammars.
#[derive(Default)]
pub struct Registry {
    entries: Vec<Entry>,
    by_scope: HashMap<String, usize>,
    by_language: HashMap<String, usize>,
    by_extension: HashMap<String, usize>,
}

// Package.json contribution model (subset).
#[derive(Debug, Deserialize)]
struct PackageJson {
    contributes: Option<Contributes>,
}

#[derive(Debug, Deserialize)]
struct Contributes {
    #[serde(default)]
    grammars: Vec<GrammarContribution>,
    #[serde(default)]
    languages: Vec<LanguageContribution>,
}

#[derive(Debug, Deserialize)]
struct GrammarContribution {
    language: Option<String>,
    #[serde(rename = "scopeName")]
    scope_name: String,
    path: String,
}

#[derive(Debug, Deserialize)]
struct LanguageContribution {
    id: String,
    #[serde(default)]
    extensions: Vec<String>,
}

fn normalize_ext(ext: &str) -> String {
    ext.trim_start_matches('.').to_ascii_lowercase()
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry preloaded with the bundled built-in grammars (ini, json, css).
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.load_builtins();
        registry
    }

    /// Register the bundled built-in grammars. Built-in JSON is known-good, so a
    /// parse failure here is a bug and is logged.
    pub fn load_builtins(&mut self) {
        for (json, language, exts) in [
            (INI_GRAMMAR, "ini", &["ini", "conf", "cfg"][..]),
            (JSON_GRAMMAR, "json", &["json"][..]),
            (CSS_GRAMMAR, "css", &["css"][..]),
        ] {
            match TextMateGrammar::from_json_str(json) {
                Ok(grammar) => {
                    self.register(grammar, Some(language), exts);
                }
                Err(e) => tracing::error!("textmate: failed to load builtin {language}: {e}"),
            }
        }
    }

    /// Register a grammar under its scope name, and optionally a language id and
    /// file extensions.
    pub fn register(
        &mut self,
        grammar: TextMateGrammar,
        language: Option<&str>,
        extensions: &[&str],
    ) -> usize {
        let id = self.entries.len();
        self.by_scope.insert(grammar.scope_name().to_string(), id);
        if let Some(lang) = language {
            self.by_language.insert(lang.to_string(), id);
        }
        for ext in extensions {
            self.by_extension.insert(normalize_ext(ext), id);
        }
        self.entries.push(Entry { grammar });
        id
    }

    /// Scan a VS Code extensions directory, loading every grammar contributed by
    /// each extension's `package.json` (`contributes.grammars`). File extensions
    /// are taken from the matching `contributes.languages` entry. Returns the
    /// number of grammars successfully registered.
    pub fn load_from_extensions_dir(&mut self, dir: &Path) -> Result<usize, TextMateError> {
        let mut count = 0;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ext_dir = entry.path();
            if !ext_dir.is_dir() {
                continue;
            }
            let pkg_path = ext_dir.join("package.json");
            if !pkg_path.exists() {
                continue;
            }
            let text = match std::fs::read_to_string(&pkg_path) {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!("textmate: cannot read {pkg_path:?}: {e}");
                    continue;
                }
            };
            let pkg: PackageJson = match serde_json::from_str(&text) {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!("textmate: cannot parse {pkg_path:?}: {e}");
                    continue;
                }
            };
            let Some(contributes) = pkg.contributes else {
                continue;
            };

            // Map language id -> extensions for this extension.
            let mut lang_exts: HashMap<String, Vec<String>> = HashMap::new();
            for lang in &contributes.languages {
                lang_exts.insert(
                    lang.id.clone(),
                    lang.extensions.iter().map(|e| normalize_ext(e)).collect(),
                );
            }

            for contribution in &contributes.grammars {
                let grammar_path = ext_dir.join(&contribution.path);
                let grammar = match TextMateGrammar::from_path(&grammar_path) {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::debug!("textmate: cannot load {grammar_path:?}: {e}");
                        continue;
                    }
                };
                let exts: Vec<&str> = contribution
                    .language
                    .as_ref()
                    .and_then(|l| lang_exts.get(l))
                    .map(|v| v.iter().map(|s| s.as_str()).collect())
                    .unwrap_or_default();
                let id = self.register(grammar, contribution.language.as_deref(), &exts);
                // Also index under the declared `scopeName` (it may differ from
                // the grammar file's own `scopeName`).
                self.by_scope
                    .entry(contribution.scope_name.clone())
                    .or_insert(id);
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn grammar_for_scope(&self, scope: &str) -> Option<&TextMateGrammar> {
        self.by_scope.get(scope).map(|&id| &self.entries[id].grammar)
    }

    pub fn grammar_for_language(&self, language: &str) -> Option<&TextMateGrammar> {
        self.by_language
            .get(language)
            .map(|&id| &self.entries[id].grammar)
    }

    pub fn grammar_for_extension(&self, extension: &str) -> Option<&TextMateGrammar> {
        self.by_extension
            .get(&normalize_ext(extension))
            .map(|&id| &self.entries[id].grammar)
    }

    /// Resolve a grammar for a file path by its extension.
    pub fn grammar_for_path(&self, path: &Path) -> Option<&TextMateGrammar> {
        let ext = path.extension()?.to_str()?;
        self.grammar_for_extension(ext)
    }

    /// Highlight `text` with `grammar`, producing lapce-normalized spans.
    pub fn highlight_all(
        &self,
        grammar: &TextMateGrammar,
        text: &str,
    ) -> lapce_xi_rope::spans::Spans<lapce_rpc::style::Style> {
        grammar.highlight_str(text)
    }
}
