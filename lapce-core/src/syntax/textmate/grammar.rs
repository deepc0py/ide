//! TextMate grammar model, compilation and the (simplified) tokenization engine.
//!
//! This implements enough of the vscode-textmate tokenization algorithm to
//! correctly tokenize self-contained grammars such as `source.ini`,
//! `source.json` and `source.css`. It is intentionally a pragmatic subset:
//!
//! * regexes are driven by [`fancy_regex`] (pure-Rust, permissive, supports
//!   lookaround/backreferences and the `\G` anchor used by many grammars);
//! * `match` / `begin`+`end` rules with `captures` / `beginCaptures` /
//!   `endCaptures`, nested `patterns`, `include` (`#repo`, `$self`, `$base`;
//!   external `source.x` includes are skipped best-effort), `name` and
//!   `contentName` are supported;
//! * `while` rules and capture sub-`patterns` are not supported (the grammars we
//!   bundle do not use them);
//! * regexes that fail to compile are skipped (logged) rather than crashing.

use std::{collections::HashMap, path::Path};

use fancy_regex::Regex;
use serde::Deserialize;
use thiserror::Error;

use super::scopes::textmate_scope_to_lapce;

/// Errors produced while loading or compiling a TextMate grammar.
#[derive(Debug, Error)]
pub enum TextMateError {
    #[error("failed to parse grammar JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid grammar: {0}")]
    Invalid(String),
}

// ---------------------------------------------------------------------------
// Raw (deserialized) grammar model
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawGrammar {
    #[serde(rename = "scopeName")]
    scope_name: String,
    #[serde(default)]
    patterns: Vec<RawRule>,
    #[serde(default)]
    repository: HashMap<String, RawRule>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRule {
    name: Option<String>,
    #[serde(rename = "contentName")]
    content_name: Option<String>,
    #[serde(rename = "match")]
    match_: Option<String>,
    begin: Option<String>,
    end: Option<String>,
    #[serde(rename = "while")]
    while_: Option<String>,
    include: Option<String>,
    captures: Option<HashMap<String, RawCapture>>,
    #[serde(rename = "beginCaptures")]
    begin_captures: Option<HashMap<String, RawCapture>>,
    #[serde(rename = "endCaptures")]
    end_captures: Option<HashMap<String, RawCapture>>,
    #[serde(default)]
    patterns: Vec<RawRule>,
}

#[derive(Debug, Default, Deserialize)]
struct RawCapture {
    name: Option<String>,
}

// ---------------------------------------------------------------------------
// Compiled grammar model
// ---------------------------------------------------------------------------

/// A reference to another rule within a pattern list.
#[derive(Debug, Clone)]
enum Pattern {
    Rule(usize),
    IncludeSelf,
    IncludeBase,
    /// `source.x` / `source.x#name` include into another grammar (unsupported,
    /// skipped best-effort).
    External,
}

/// A single capture-group scope assignment.
#[derive(Debug, Clone)]
struct Capture {
    index: usize,
    scopes: Vec<String>,
}

/// An anchor-aware compiled regex.
///
/// TextMate's `\G` anchor matches the *anchor position*: the end of the previous
/// match within the current line, reset to "none" at the start of every line
/// (so `\G` never matches at a line start). [`fancy_regex`] instead pins `\G` to
/// the search start position. To reproduce the TextMate semantics we keep two
/// compiled forms: the verbatim pattern (used only when the search position *is*
/// the anchor, where `\G` == search start is correct) and an "unanchored" form
/// with every `\G` rewritten to an always-false assertion `(?!)` (used when the
/// search position is not the anchor, where `\G` can never match).
#[derive(Debug, Clone)]
struct TmRegex {
    anchored: Regex,
    /// Present only when the source contains a `\G` anchor.
    unanchored: Option<Regex>,
}

impl TmRegex {
    fn new(src: &str) -> Result<Self, fancy_regex::Error> {
        let anchored = Regex::new(src)?;
        let unanchored = if src.contains("\\G") {
            Some(Regex::new(&rewrite_g_anchor(src))?)
        } else {
            None
        };
        Ok(TmRegex {
            anchored,
            unanchored,
        })
    }

    /// An empty-pattern regex (matches zero-width anywhere); used as a safe
    /// fallback so a begin/end rule never stays open forever.
    fn empty() -> Self {
        TmRegex {
            anchored: Regex::new("").unwrap(),
            unanchored: None,
        }
    }

    fn search<'t>(
        &self,
        text: &'t str,
        pos: usize,
        at_anchor: bool,
    ) -> Option<fancy_regex::Captures<'t, str>> {
        let re = if at_anchor {
            &self.anchored
        } else {
            self.unanchored.as_ref().unwrap_or(&self.anchored)
        };
        re.captures_from_pos(text, pos).ok().flatten()
    }
}

/// Rewrite every `\G` anchor in a regex source to an always-false assertion.
fn rewrite_g_anchor(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('G') => {
                    chars.next();
                    out.push_str("(?!)");
                }
                Some(&n) => {
                    chars.next();
                    out.push('\\');
                    out.push(n);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Debug)]
enum Rule {
    Match {
        regex: TmRegex,
        scopes: Vec<String>,
        captures: Vec<Capture>,
    },
    BeginEnd {
        begin: TmRegex,
        end_template: String,
        /// Precompiled end regex when the template contains no backreferences.
        end_regex: Option<TmRegex>,
        end_has_backref: bool,
        scopes: Vec<String>,
        content_scopes: Vec<String>,
        begin_captures: Vec<Capture>,
        end_captures: Vec<Capture>,
        patterns: Vec<Pattern>,
    },
    /// A container that only groups patterns (repository group / `$self`).
    Patterns {
        patterns: Vec<Pattern>,
    },
}

/// A compiled TextMate grammar ready for tokenization.
#[derive(Debug)]
pub struct TextMateGrammar {
    scope_name: String,
    rules: Vec<Rule>,
    /// Flattened matchable child rule ids per rule id (only populated for
    /// `BeginEnd` rules; empty otherwise).
    children: Vec<Vec<usize>>,
    /// Flattened matchable rule ids for the grammar root.
    root: Vec<usize>,
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

struct Compiler {
    rules: Vec<Option<Rule>>,
    repo_ids: HashMap<String, usize>,
}

impl Compiler {
    fn split_scopes(name: &Option<String>) -> Vec<String> {
        name.as_deref()
            .map(|n| n.split_whitespace().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    }

    fn compile_captures(raw: &Option<HashMap<String, RawCapture>>) -> Vec<Capture> {
        let mut out = Vec::new();
        if let Some(map) = raw {
            for (k, v) in map {
                if let Ok(index) = k.parse::<usize>() {
                    let scopes = Compiler::split_scopes(&v.name);
                    if !scopes.is_empty() {
                        out.push(Capture { index, scopes });
                    }
                }
            }
        }
        out.sort_by_key(|c| c.index);
        out
    }

    /// Reserve a slot in the arena, returning its id.
    fn reserve(&mut self) -> usize {
        let id = self.rules.len();
        self.rules.push(None);
        id
    }

    /// Compile a raw rule body into `slot`. Returns `false` when the rule has no
    /// usable form (e.g. its regex failed to compile) and the slot was filled
    /// with an inert `Patterns` rule.
    fn compile_into(&mut self, slot: usize, raw: &RawRule) -> bool {
        let rule = self.compile_rule_body(raw);
        let ok = rule.is_some();
        self.rules[slot] = Some(rule.unwrap_or(Rule::Patterns {
            patterns: Vec::new(),
        }));
        ok
    }

    fn compile_rule_body(&mut self, raw: &RawRule) -> Option<Rule> {
        if let Some(m) = &raw.match_ {
            let regex = match TmRegex::new(m) {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("textmate: skipping match rule, bad regex {m:?}: {e}");
                    return None;
                }
            };
            return Some(Rule::Match {
                regex,
                scopes: Compiler::split_scopes(&raw.name),
                captures: Compiler::compile_captures(&raw.captures),
            });
        }

        if let Some(begin) = &raw.begin {
            let begin_re = match TmRegex::new(begin) {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("textmate: skipping begin rule, bad regex {begin:?}: {e}");
                    return None;
                }
            };
            let patterns = self.compile_patterns(&raw.patterns);
            // A `begin` without `end` (possibly `while`) degrades to a plain
            // match of the begin pattern so we never get stuck in an open state.
            let Some(end) = &raw.end else {
                if raw.while_.is_some() {
                    tracing::debug!("textmate: `while` rule unsupported, treating begin as match");
                }
                let mut captures = Compiler::compile_captures(&raw.begin_captures);
                captures.extend(Compiler::compile_captures(&raw.captures));
                captures.sort_by_key(|c| c.index);
                return Some(Rule::Match {
                    regex: begin_re,
                    scopes: Compiler::split_scopes(&raw.name),
                    captures,
                });
            };

            let end_has_backref = end_template_has_backref(end);
            let end_regex = if end_has_backref {
                None
            } else {
                match TmRegex::new(end) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        tracing::debug!(
                            "textmate: begin/end rule end regex {end:?} failed: {e}, \
                             using empty (pops immediately)"
                        );
                        // Empty regex matches zero-width at the cursor, popping
                        // the rule rather than leaving it open forever.
                        Some(TmRegex::empty())
                    }
                }
            };

            return Some(Rule::BeginEnd {
                begin: begin_re,
                end_template: end.clone(),
                end_regex,
                end_has_backref,
                scopes: Compiler::split_scopes(&raw.name),
                content_scopes: Compiler::split_scopes(&raw.content_name),
                begin_captures: Compiler::compile_captures(&raw.begin_captures),
                end_captures: Compiler::compile_captures(&raw.end_captures),
                patterns,
            });
        }

        // A bare pattern container (repository group).
        Some(Rule::Patterns {
            patterns: self.compile_patterns(&raw.patterns),
        })
    }

    fn compile_patterns(&mut self, raws: &[RawRule]) -> Vec<Pattern> {
        let mut out = Vec::new();
        for raw in raws {
            if let Some(inc) = &raw.include {
                match resolve_include(inc, &self.repo_ids) {
                    Some(p) => out.push(p),
                    None => {
                        tracing::debug!("textmate: skipping unresolved include {inc:?}");
                    }
                }
                continue;
            }
            let slot = self.reserve();
            if self.compile_into(slot, raw) {
                out.push(Pattern::Rule(slot));
            }
        }
        out
    }
}

fn resolve_include(inc: &str, repo_ids: &HashMap<String, usize>) -> Option<Pattern> {
    if inc == "$self" {
        return Some(Pattern::IncludeSelf);
    }
    if inc == "$base" {
        return Some(Pattern::IncludeBase);
    }
    if let Some(key) = inc.strip_prefix('#') {
        return repo_ids.get(key).copied().map(Pattern::Rule);
    }
    // `source.x` or `source.x#name` — external grammar include, unsupported.
    Some(Pattern::External)
}

/// Returns true when a `begin`/`end` end template references a begin capture via
/// a `\N` numeric backreference.
fn end_template_has_backref(template: &str) -> bool {
    let bytes = template.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\\' && bytes[i + 1].is_ascii_digit() {
            return true;
        }
        if bytes[i] == b'\\' {
            i += 2;
        } else {
            i += 1;
        }
    }
    false
}

/// Escape a literal string for inclusion in a regex (used for backref
/// substitution in `end` templates).
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Substitute `\N` numeric backreferences in an `end` template with the escaped
/// text captured by the matching `begin`.
fn resolve_end_template(template: &str, begin_caps: &fancy_regex::Captures<'_, str>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    chars.next();
                    let idx = d as usize - '0' as usize;
                    if let Some(m) = begin_caps.get(idx) {
                        out.push_str(&regex_escape(m.as_str()));
                    }
                    continue;
                }
            }
            out.push('\\');
            if let Some(d) = chars.next() {
                out.push(d);
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Flattening (resolving includes into matchable rule lists)
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, Hash)]
enum FlattenKey {
    Rule(usize),
    SelfRoot,
}

fn collect_matchable(
    rules: &[Rule],
    patterns: &[Pattern],
    root: &[Pattern],
    out: &mut Vec<usize>,
    seen: &mut std::collections::HashSet<FlattenKey>,
) {
    for p in patterns {
        match p {
            Pattern::Rule(id) => match &rules[*id] {
                Rule::Patterns { patterns } => {
                    if seen.insert(FlattenKey::Rule(*id)) {
                        collect_matchable(rules, patterns, root, out, seen);
                    }
                }
                _ => out.push(*id),
            },
            Pattern::IncludeSelf | Pattern::IncludeBase => {
                if seen.insert(FlattenKey::SelfRoot) {
                    collect_matchable(rules, root, root, out, seen);
                }
            }
            Pattern::External => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Public grammar API
// ---------------------------------------------------------------------------

impl TextMateGrammar {
    /// Load and compile a grammar from `.tmLanguage.json` text.
    pub fn from_json_str(json: &str) -> Result<Self, TextMateError> {
        let raw: RawGrammar = serde_json::from_str(json)?;
        Ok(Self::from_raw(raw))
    }

    /// Load and compile a grammar from a `.tmLanguage.json` file on disk.
    pub fn from_path(path: &Path) -> Result<Self, TextMateError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json_str(&text)
    }

    pub fn scope_name(&self) -> &str {
        &self.scope_name
    }

    fn from_raw(raw: RawGrammar) -> Self {
        let mut compiler = Compiler {
            rules: Vec::new(),
            repo_ids: HashMap::new(),
        };

        // Reserve ids for every repository entry first so rules can reference
        // each other (including recursively) by key.
        let mut repo_order: Vec<(String, usize)> = Vec::new();
        for key in raw.repository.keys() {
            let id = compiler.reserve();
            compiler.repo_ids.insert(key.clone(), id);
            repo_order.push((key.clone(), id));
        }

        // Compile repository bodies into their reserved slots.
        for (key, id) in &repo_order {
            if let Some(rule) = raw.repository.get(key) {
                compiler.compile_into(*id, rule);
            }
        }

        // Compile the root pattern list.
        let root_patterns = compiler.compile_patterns(&raw.patterns);

        let rules: Vec<Rule> = compiler
            .rules
            .into_iter()
            .map(|r| {
                r.unwrap_or(Rule::Patterns {
                    patterns: Vec::new(),
                })
            })
            .collect();

        // Flatten include chains into matchable rule-id lists.
        let mut root = Vec::new();
        collect_matchable(
            &rules,
            &root_patterns,
            &root_patterns,
            &mut root,
            &mut std::collections::HashSet::new(),
        );

        let mut children = vec![Vec::new(); rules.len()];
        for (id, rule) in rules.iter().enumerate() {
            if let Rule::BeginEnd { patterns, .. } = rule {
                let mut out = Vec::new();
                collect_matchable(
                    &rules,
                    patterns,
                    &root_patterns,
                    &mut out,
                    &mut std::collections::HashSet::new(),
                );
                children[id] = out;
            }
        }

        TextMateGrammar {
            scope_name: raw.scope_name,
            rules,
            children,
            root,
        }
    }

    /// Create a fresh tokenization state (empty rule stack rooted at the
    /// grammar's top-level scope).
    pub fn new_state(&self) -> TokenizeState {
        TokenizeState {
            stack: Vec::new(),
            scopes: vec![self.scope_name.clone()],
        }
    }

    /// Tokenize a single line (which should include its trailing `\n` when
    /// present). `line_offset` is the absolute byte offset of the line within
    /// the document. Emits `(abs_start, abs_end, lapce_scope)` tuples (only for
    /// ranges whose scope maps to a known lapce scope) into `out`.
    pub fn tokenize_line(
        &self,
        line: &str,
        state: &mut TokenizeState,
        line_offset: usize,
        out: &mut Vec<(usize, usize, &'static str)>,
    ) {
        let len = line.len();
        let mut pos = 0usize;
        // The `\G` anchor position: `None` at line start, else the end of the
        // previous match on this line.
        let mut anchor: Option<usize> = None;
        // Safety net against pathological zero-width loops.
        let max_iters = len.saturating_mul(50) + 1000;
        let mut iters = 0;

        while pos <= len {
            iters += 1;
            if iters > max_iters {
                tracing::debug!("textmate: iteration cap hit, bailing out of line");
                self.emit(out, line, line_offset, pos, len, &state.scopes);
                break;
            }

            let at_anchor = anchor == Some(pos);

            // End match of the current top-of-stack rule (if any).
            let end_caps = state
                .stack
                .last()
                .and_then(|entry| entry.end_regex.search(line, pos, at_anchor));

            // Best (left-most, earliest-in-order) child match.
            let active: &[usize] = match state.stack.last() {
                Some(entry) => &self.children[entry.rule_id],
                None => &self.root,
            };
            let mut best_child = None;
            let mut best_start = usize::MAX;
            for &rule_id in active {
                let regex = match &self.rules[rule_id] {
                    Rule::Match { regex, .. } => regex,
                    Rule::BeginEnd { begin, .. } => begin,
                    Rule::Patterns { .. } => continue,
                };
                if let Some(caps) = regex.search(line, pos, at_anchor) {
                    let start = caps.get(0).unwrap().start();
                    if start < best_start {
                        best_start = start;
                        best_child = Some((rule_id, caps));
                    }
                }
            }

            if end_caps.is_none() && best_child.is_none() {
                self.emit(out, line, line_offset, pos, len, &state.scopes);
                break;
            }

            // End wins on ties (default applyEndPatternLast = false); `\G`-anchored
            // ends never tie at the cursor, so children consume first as intended.
            let end_start = end_caps.as_ref().map(|c| c.get(0).unwrap().start());
            let use_end = match (end_start, best_child.as_ref()) {
                (Some(es), Some(_)) => es <= best_start,
                (Some(_), None) => true,
                (None, _) => false,
            };

            if use_end {
                let caps = end_caps.unwrap();
                let m0 = caps.get(0).unwrap();
                let (ms, me) = (m0.start(), m0.end());
                // Content between cursor and end (still inside content scope).
                self.emit(out, line, line_offset, pos, ms, &state.scopes);

                let entry = state.stack.last().unwrap();
                let (end_captures, content_len, name_len) = match &self.rules[entry.rule_id] {
                    Rule::BeginEnd {
                        end_captures,
                        content_scopes,
                        scopes,
                        ..
                    } => (end_captures, content_scopes.len(), scopes.len()),
                    _ => unreachable!("stack entry must be a begin/end rule"),
                };
                // Drop content scopes so the end token is scoped by `name`.
                let new_len = state.scopes.len() - content_len;
                state.scopes.truncate(new_len);
                self.emit_captures(
                    out,
                    line,
                    line_offset,
                    &caps,
                    &state.scopes,
                    &[],
                    end_captures,
                );
                // Drop the rule's own name scopes and pop.
                let new_len = state.scopes.len() - name_len;
                state.scopes.truncate(new_len);
                state.stack.pop();

                // End consumed [ms, me); the anchor follows the match end.
                anchor = Some(me);
                pos = me;
            } else {
                let (rule_id, caps) = best_child.unwrap();
                match &self.rules[rule_id] {
                    Rule::Match {
                        scopes, captures, ..
                    } => {
                        let m0 = caps.get(0).unwrap();
                        let (ms, me) = (m0.start(), m0.end());
                        self.emit(out, line, line_offset, pos, ms, &state.scopes);
                        self.emit_captures(
                            out,
                            line,
                            line_offset,
                            &caps,
                            &state.scopes,
                            scopes,
                            captures,
                        );
                        anchor = Some(me);
                        pos = advance_match(line, ms, me);
                    }
                    Rule::BeginEnd {
                        scopes,
                        content_scopes,
                        begin_captures,
                        end_template,
                        end_regex,
                        end_has_backref,
                        ..
                    } => {
                        let m0 = caps.get(0).unwrap();
                        let (ms, me) = (m0.start(), m0.end());
                        self.emit(out, line, line_offset, pos, ms, &state.scopes);
                        // Begin token is scoped by the rule name (not content).
                        state.scopes.extend(scopes.iter().cloned());
                        self.emit_captures(
                            out,
                            line,
                            line_offset,
                            &caps,
                            &state.scopes,
                            &[],
                            begin_captures,
                        );
                        // Resolve the end regex (substituting begin backrefs).
                        let end = if *end_has_backref {
                            let resolved = resolve_end_template(end_template, &caps);
                            TmRegex::new(&resolved).unwrap_or_else(|_| TmRegex::empty())
                        } else {
                            end_regex.clone().unwrap()
                        };
                        // Enter the rule: content scopes active until end.
                        state.scopes.extend(content_scopes.iter().cloned());
                        state.stack.push(StackEntry {
                            rule_id,
                            end_regex: end,
                        });
                        // Zero-width begins still progress via the stack push.
                        anchor = Some(me);
                        pos = me;
                    }
                    Rule::Patterns { .. } => {
                        // Not directly matchable; advance to avoid a stall.
                        pos = advance_char(line, pos);
                    }
                }
            }
        }
    }

    /// Highlight an entire `&str`, producing lapce-normalized spans.
    pub fn highlight_str(
        &self,
        text: &str,
    ) -> lapce_xi_rope::spans::Spans<lapce_rpc::style::Style> {
        use lapce_rpc::style::Style;
        use lapce_xi_rope::{Interval, spans::SpansBuilder};

        let mut out: Vec<(usize, usize, &'static str)> = Vec::new();
        let mut state = self.new_state();
        let mut offset = 0usize;
        while offset < text.len() {
            let nl = text[offset..]
                .find('\n')
                .map(|i| offset + i + 1)
                .unwrap_or(text.len());
            self.tokenize_line(&text[offset..nl], &mut state, offset, &mut out);
            offset = nl;
        }

        let mut builder = SpansBuilder::new(text.len());
        for (start, end, scope) in out {
            if start < end {
                builder.add_span(
                    Interval::new(start, end),
                    Style {
                        fg_color: Some(scope.to_string()),
                    },
                );
            }
        }
        builder.build()
    }

    /// Highlight a rope, producing lapce-normalized spans.
    pub fn highlight_rope(
        &self,
        text: &lapce_xi_rope::Rope,
    ) -> lapce_xi_rope::spans::Spans<lapce_rpc::style::Style> {
        self.highlight_str(&text.to_string())
    }

    fn emit(
        &self,
        out: &mut Vec<(usize, usize, &'static str)>,
        _line: &str,
        line_offset: usize,
        start: usize,
        end: usize,
        scopes: &[String],
    ) {
        if start >= end {
            return;
        }
        if let Some(scope) = textmate_scope_to_lapce(scopes) {
            out.push((line_offset + start, line_offset + end, scope));
        }
    }

    /// Emit tokens for a match, overlaying capture scopes onto sub-ranges. The
    /// whole match is scoped by `base` (the active scope stack) plus
    /// `name_scopes`; each capture adds its scopes over its sub-range, with more
    /// specific (shorter) captures taking precedence.
    #[allow(clippy::too_many_arguments)]
    fn emit_captures(
        &self,
        out: &mut Vec<(usize, usize, &'static str)>,
        line: &str,
        line_offset: usize,
        caps: &fancy_regex::Captures<'_, str>,
        base: &[String],
        name_scopes: &[String],
        captures: &[Capture],
    ) {
        let m0 = caps.get(0).unwrap();
        let (s0, e0) = (m0.start(), m0.end());
        if s0 >= e0 {
            return;
        }

        // Collect capture ranges that fall within the whole match.
        let mut covers: Vec<(usize, usize, &[String])> = Vec::new();
        for cap in captures {
            if let Some(g) = caps.get(cap.index) {
                let (gs, ge) = (g.start(), g.end());
                if gs < ge && gs >= s0 && ge <= e0 {
                    covers.push((gs, ge, &cap.scopes));
                }
            }
        }

        // No captures: single token over the whole match.
        if covers.is_empty() {
            let mut scopes = Vec::with_capacity(base.len() + name_scopes.len());
            scopes.extend_from_slice(base);
            scopes.extend_from_slice(name_scopes);
            self.emit_slice(out, line, line_offset, s0, e0, &scopes);
            return;
        }

        // Boundary points subdivide the match into minimal intervals.
        let mut points: Vec<usize> = vec![s0, e0];
        for (gs, ge, _) in &covers {
            points.push(*gs);
            points.push(*ge);
        }
        points.sort_unstable();
        points.dedup();

        for win in points.windows(2) {
            let (a, b) = (win[0], win[1]);
            if a >= b {
                continue;
            }
            let mut scopes: Vec<String> =
                Vec::with_capacity(base.len() + name_scopes.len() + 2);
            scopes.extend_from_slice(base);
            scopes.extend_from_slice(name_scopes);
            // Captures covering [a, b); outer (longer) first, inner (shorter)
            // last so the deepest/most-specific scope wins.
            let mut cov: Vec<&(usize, usize, &[String])> = covers
                .iter()
                .filter(|(gs, ge, _)| *gs <= a && *ge >= b)
                .collect();
            cov.sort_by(|x, y| (y.1 - y.0).cmp(&(x.1 - x.0)));
            for (_, _, cs) in cov {
                scopes.extend(cs.iter().cloned());
            }
            self.emit_slice(out, line, line_offset, a, b, &scopes);
        }
    }

    fn emit_slice(
        &self,
        out: &mut Vec<(usize, usize, &'static str)>,
        _line: &str,
        line_offset: usize,
        start: usize,
        end: usize,
        scopes: &[String],
    ) {
        if start >= end {
            return;
        }
        if let Some(scope) = textmate_scope_to_lapce(scopes) {
            out.push((line_offset + start, line_offset + end, scope));
        }
    }
}

/// Advance the cursor past a `match` rule, guaranteeing forward progress even
/// for zero-width matches (which would otherwise match forever at the same
/// position). `match_start >= pos` always holds for `captures_from_pos`.
fn advance_match(line: &str, match_start: usize, match_end: usize) -> usize {
    if match_end > match_start {
        match_end
    } else {
        advance_char(line, match_start)
    }
}

fn advance_char(line: &str, pos: usize) -> usize {
    if pos >= line.len() {
        return line.len() + 1; // force loop exit (pos > len)
    }
    let mut p = pos + 1;
    while p < line.len() && !line.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// A pushed `begin`/`end` rule awaiting its end match.
#[derive(Clone)]
struct StackEntry {
    rule_id: usize,
    end_regex: TmRegex,
}

/// Mutable tokenization state carried line to line.
pub struct TokenizeState {
    stack: Vec<StackEntry>,
    scopes: Vec<String>,
}
