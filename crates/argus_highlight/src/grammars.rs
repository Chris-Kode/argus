//! Bundled grammars and their highlight queries.
//!
//! # Licences
//!
//! Every grammar crate and its bundled queries are MIT-licensed
//! (tree-sitter, tree-sitter-grammars, and the individual grammar authors);
//! that is compatible with Argus' Apache-2.0 licence. Zed's own
//! `highlights.scm` files live in Zed's `languages` crate, which is
//! GPL-3.0-or-later, so they are **not** reused here: the queries are the
//! ones each grammar ships, whose capture names follow the same convention
//! and are mapped onto Zed's theme keys by [`crate::RECOGNIZED_NAMES`]. The
//! Kotlin grammar (`tree-sitter-kotlin-ng`) ships no queries, so
//! `queries/kotlin/highlights.scm` was written for Argus.

use std::sync::OnceLock;

use tree_sitter_highlight::HighlightConfiguration;

use crate::kind::RECOGNIZED_NAMES;
use crate::language::Language;

const KOTLIN_HIGHLIGHTS: &str = include_str!("../queries/kotlin/highlights.scm");
/// Appended to Rust's query: its `SCREAMING_CASE` constant pattern comes
/// before the capitalized-constructor one, which would otherwise win.
const RUST_OVERRIDES: &str = include_str!("../queries/rust/overrides.scm");

/// The highlight configuration of `language`, built on first use and
/// shared by every worker. `None` when its queries fail to compile, which
/// is logged once; the file is then shown as plain text.
pub fn config(language: Language) -> Option<&'static HighlightConfiguration> {
    static CONFIGS: [OnceLock<Option<HighlightConfiguration>>; Language::ALL.len()] =
        [const { OnceLock::new() }; Language::ALL.len()];
    CONFIGS[language.index()]
        .get_or_init(|| build(language))
        .as_ref()
}

fn build(language: Language) -> Option<HighlightConfiguration> {
    let (grammar, highlights, injections, locals) = sources(language);
    let highlights: Vec<String> = highlights
        .iter()
        .map(|query| {
            if first_wins(language) {
                reverse_patterns(query)
            } else {
                (*query).to_owned()
            }
        })
        .collect();
    let highlights = highlights.join("\n");
    let locals = locals.join("\n");
    let configured =
        HighlightConfiguration::new(grammar, language.name(), &highlights, injections, &locals);
    match configured {
        Ok(mut config) => {
            let names: Vec<&str> = RECOGNIZED_NAMES.iter().map(|(name, _)| *name).collect();
            config.configure(&names);
            Some(config)
        }
        Err(error) => {
            tracing::warn!(language = language.name(), %error, "highlight query failed to compile");
            None
        }
    }
}

/// The block grammar's injections, with inline markup parsed over the
/// whole `inline` node: its text is made of hidden child tokens, which
/// tree-sitter-highlight would otherwise cut out of the injected range.
fn markdown_injections() -> &'static str {
    static QUERY: OnceLock<String> = OnceLock::new();
    QUERY.get_or_init(|| {
        tree_sitter_md::INJECTION_QUERY_BLOCK.replace(
            "(#set! injection.language \"markdown_inline\"))",
            "(#set! injection.language \"markdown_inline\")\n  (#set! injection.include-children))",
        )
    })
}

/// Grammar, highlight queries (concatenated in order: base language first,
/// extensions after, since the last matching pattern wins), injection query
/// and locals queries.
type Sources = (
    tree_sitter::Language,
    Vec<&'static str>,
    &'static str,
    Vec<&'static str>,
);

fn sources(language: Language) -> Sources {
    use tree_sitter_javascript as js;
    use tree_sitter_typescript as ts;
    match language {
        Language::Bash => (
            tree_sitter_bash::LANGUAGE.into(),
            vec![tree_sitter_bash::HIGHLIGHT_QUERY],
            "",
            vec![],
        ),
        Language::C => (
            tree_sitter_c::LANGUAGE.into(),
            vec![tree_sitter_c::HIGHLIGHT_QUERY],
            "",
            vec![],
        ),
        // The C++ query only adds to C's, like the grammar extends C.
        Language::Cpp => (
            tree_sitter_cpp::LANGUAGE.into(),
            vec![
                tree_sitter_c::HIGHLIGHT_QUERY,
                tree_sitter_cpp::HIGHLIGHT_QUERY,
            ],
            "",
            vec![],
        ),
        Language::CSharp => (
            tree_sitter_c_sharp::LANGUAGE.into(),
            vec![tree_sitter_c_sharp::HIGHLIGHTS_QUERY],
            "",
            vec![],
        ),
        Language::Css => (
            tree_sitter_css::LANGUAGE.into(),
            vec![tree_sitter_css::HIGHLIGHTS_QUERY],
            "",
            vec![],
        ),
        Language::Go => (
            tree_sitter_go::LANGUAGE.into(),
            vec![tree_sitter_go::HIGHLIGHTS_QUERY],
            "",
            vec![],
        ),
        Language::Html => (
            tree_sitter_html::LANGUAGE.into(),
            vec![tree_sitter_html::HIGHLIGHTS_QUERY],
            tree_sitter_html::INJECTIONS_QUERY,
            vec![],
        ),
        Language::Java => (
            tree_sitter_java::LANGUAGE.into(),
            vec![tree_sitter_java::HIGHLIGHTS_QUERY],
            "",
            vec![],
        ),
        Language::JavaScript => (
            js::LANGUAGE.into(),
            vec![js::HIGHLIGHT_QUERY, js::JSX_HIGHLIGHT_QUERY],
            js::INJECTIONS_QUERY,
            vec![js::LOCALS_QUERY],
        ),
        Language::Json => (
            tree_sitter_json::LANGUAGE.into(),
            vec![tree_sitter_json::HIGHLIGHTS_QUERY],
            "",
            vec![],
        ),
        Language::Kotlin => (
            tree_sitter_kotlin_ng::LANGUAGE.into(),
            vec![KOTLIN_HIGHLIGHTS],
            "",
            vec![],
        ),
        Language::Markdown => (
            tree_sitter_md::LANGUAGE.into(),
            vec![tree_sitter_md::HIGHLIGHT_QUERY_BLOCK],
            markdown_injections(),
            vec![],
        ),
        Language::MarkdownInline => (
            tree_sitter_md::INLINE_LANGUAGE.into(),
            vec![tree_sitter_md::HIGHLIGHT_QUERY_INLINE],
            tree_sitter_md::INJECTION_QUERY_INLINE,
            vec![],
        ),
        Language::Php => (
            tree_sitter_php::LANGUAGE_PHP.into(),
            vec![tree_sitter_php::HIGHLIGHTS_QUERY],
            tree_sitter_php::INJECTIONS_QUERY,
            vec![],
        ),
        Language::Python => (
            tree_sitter_python::LANGUAGE.into(),
            vec![tree_sitter_python::HIGHLIGHTS_QUERY],
            "",
            vec![],
        ),
        Language::Ruby => (
            tree_sitter_ruby::LANGUAGE.into(),
            vec![tree_sitter_ruby::HIGHLIGHTS_QUERY],
            "",
            vec![tree_sitter_ruby::LOCALS_QUERY],
        ),
        Language::Rust => (
            tree_sitter_rust::LANGUAGE.into(),
            vec![tree_sitter_rust::HIGHLIGHTS_QUERY, RUST_OVERRIDES],
            tree_sitter_rust::INJECTIONS_QUERY,
            vec![],
        ),
        Language::Sql => (
            tree_sitter_sequel::LANGUAGE.into(),
            vec![tree_sitter_sequel::HIGHLIGHTS_QUERY],
            "",
            vec![],
        ),
        Language::Swift => (
            tree_sitter_swift::LANGUAGE.into(),
            vec![tree_sitter_swift::HIGHLIGHTS_QUERY],
            tree_sitter_swift::INJECTIONS_QUERY,
            vec![tree_sitter_swift::LOCALS_QUERY],
        ),
        Language::Toml => (
            tree_sitter_toml_ng::LANGUAGE.into(),
            vec![tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
            "",
            vec![],
        ),
        // TypeScript's queries extend JavaScript's.
        Language::TypeScript => (
            ts::LANGUAGE_TYPESCRIPT.into(),
            vec![js::HIGHLIGHT_QUERY, ts::HIGHLIGHTS_QUERY],
            js::INJECTIONS_QUERY,
            vec![ts::LOCALS_QUERY, js::LOCALS_QUERY],
        ),
        Language::Tsx => (
            ts::LANGUAGE_TSX.into(),
            vec![
                js::HIGHLIGHT_QUERY,
                js::JSX_HIGHLIGHT_QUERY,
                ts::HIGHLIGHTS_QUERY,
            ],
            js::INJECTIONS_QUERY,
            vec![ts::LOCALS_QUERY, js::LOCALS_QUERY],
        ),
        Language::Yaml => (
            tree_sitter_yaml::LANGUAGE.into(),
            vec![tree_sitter_yaml::HIGHLIGHTS_QUERY],
            "",
            vec![],
        ),
    }
}

/// Whether the bundled query of `language` was written for the older
/// first-wins rule (specific patterns first, catch-alls such as
/// `(identifier) @variable` last). Checked grammar by grammar against
/// sample files; every other bundled query already puts catch-alls first.
fn first_wins(language: Language) -> bool {
    matches!(language, Language::Go)
}

/// Reverses the order of the top-level patterns of a query.
///
/// tree-sitter-highlight lets the **last** pattern matching a node win.
/// Queries written for the older first-wins rule (generic patterns last)
/// get the intended result once reversed. Comments are dropped.
pub(crate) fn reverse_patterns(query: &str) -> String {
    let mut patterns = top_level_patterns(query);
    patterns.reverse();
    patterns.join("\n")
}

/// Splits a query into its top-level patterns: one parenthesized,
/// bracketed, string or wildcard item plus its captures and quantifiers.
fn top_level_patterns(query: &str) -> Vec<&str> {
    let bytes = query.as_bytes();
    let mut patterns = Vec::new();
    let mut at = skip_trivia(bytes, 0);
    while at < bytes.len() {
        let start = at;
        // Always advance, even over a stray character.
        at = skip_item(bytes, at).max(start + 1);
        // Captures (`@name`) and quantifiers after the item.
        loop {
            let next = skip_trivia(bytes, at);
            match bytes.get(next) {
                Some(b'@') => {
                    at = next + 1;
                    while at < bytes.len()
                        && !bytes[at].is_ascii_whitespace()
                        && !matches!(bytes[at], b'(' | b')' | b'[' | b']' | b';')
                    {
                        at += 1;
                    }
                }
                Some(b'*' | b'+' | b'?') => at = next + 1,
                _ => break,
            }
        }
        patterns.push(&query[start..at]);
        at = skip_trivia(bytes, at);
    }
    patterns
}

/// Skips whitespace and `;` comments.
fn skip_trivia(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() {
        match bytes[at] {
            b';' => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            byte if byte.is_ascii_whitespace() => at += 1,
            _ => break,
        }
    }
    at
}

/// Skips one string literal starting at `at` (on its opening quote).
fn skip_string(bytes: &[u8], mut at: usize) -> usize {
    at += 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    at
}

/// Skips one item: a balanced `(...)` / `[...]`, a string, or a bare word
/// such as `_`.
fn skip_item(bytes: &[u8], at: usize) -> usize {
    match bytes.get(at) {
        Some(b'"') => skip_string(bytes, at),
        Some(b'(' | b'[') => {
            let mut depth = 0usize;
            let mut at = at;
            while at < bytes.len() {
                match bytes[at] {
                    b'"' => {
                        at = skip_string(bytes, at);
                        continue;
                    }
                    b';' => {
                        at = skip_trivia(bytes, at);
                        continue;
                    }
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return at + 1;
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
            at
        }
        Some(_) => {
            let mut at = at;
            while at < bytes.len()
                && !bytes[at].is_ascii_whitespace()
                && !matches!(bytes[at], b'@' | b'(' | b'[')
            {
                at += 1;
            }
            at
        }
        None => at,
    }
}
