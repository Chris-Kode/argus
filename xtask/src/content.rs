//! Synthetic file contents: source-like text in several languages, edits
//! of it, binaries, and nested directory paths.
//!
//! The text only has to look like code to the diff engine and the
//! highlighter (keywords, brackets, comments, strings); it does not compile.

use crate::rng::Rng;

/// Identifier fragments for names, paths and comments.
const WORDS: &[&str] = &[
    "account", "alpha", "anchor", "batch", "buffer", "cache", "channel", "client", "commit",
    "config", "cursor", "delta", "digest", "engine", "event", "filter", "frame", "gauge", "graph",
    "handle", "header", "index", "kernel", "layout", "ledger", "limit", "loader", "matrix",
    "merge", "metric", "module", "node", "offset", "packet", "parser", "patch", "pixel", "policy",
    "queue", "range", "reader", "record", "region", "render", "router", "sample", "schema",
    "scope", "server", "session", "signal", "socket", "source", "stream", "symbol", "table",
    "target", "thread", "token", "tracker", "vector", "window", "worker", "writer",
];

/// One synthetic language: a file extension and line templates. In the
/// templates, `@` becomes a word and `#` a number.
pub struct Language {
    /// File extension, without the dot.
    pub ext: &'static str,
    /// Opens a block (function, section, …).
    header: &'static [&'static str],
    /// Lines inside a block.
    body: &'static [&'static str],
    /// Closes a block; may be empty.
    footer: &'static str,
}

/// The languages the fixtures mix, all with a bundled grammar.
pub const LANGUAGES: &[Language] = &[
    Language {
        ext: "rs",
        header: &["pub fn @_@(value: u64) -> u64 {", "impl @ for @Store {"],
        body: &[
            "    let @ = value.wrapping_mul(#);",
            "    // Keep the @ in step with the @ #.",
            "    if @ > # { return @ + #; }",
            "    let name = \"@-@\".to_string();",
            "    @.push(#);",
        ],
        footer: "}",
    },
    Language {
        ext: "ts",
        header: &["export function @@(input: number): number {", "class @@ {"],
        body: &[
            "  const @ = input * #;",
            "  // @ handles the @ path",
            "  if (@ > #) { return @ - #; }",
            "  const label = `@-${#}`;",
            "  this.@.set(\"@\", #);",
        ],
        footer: "}",
    },
    Language {
        ext: "py",
        header: &["def @_@(value):", "class @@:"],
        body: &[
            "    @ = value * #",
            "    # the @ keeps the @ warm",
            "    if @ > #:",
            "        return \"@-@\"",
            "    self.@.append(#)",
        ],
        footer: "",
    },
    Language {
        ext: "go",
        header: &["func @@(value int) int {", "func (s *@) @() error {"],
        body: &[
            "\t@ := value * #",
            "\t// @ guards the @",
            "\tif @ > # { return @ }",
            "\tlog.Printf(\"@ %d\", #)",
            "\ts.@ = append(s.@, #)",
        ],
        footer: "}",
    },
    Language {
        ext: "c",
        header: &["static int @_@(int value) {", "void @_@(struct @ *self) {"],
        body: &[
            "    int @ = value * #;",
            "    /* @ before @ */",
            "    if (@ > #) return @;",
            "    printf(\"@ %d\\n\", #);",
            "    self->@ += #;",
        ],
        footer: "}",
    },
    Language {
        ext: "java",
        header: &["public int @@(int value) {", "final class @@ {"],
        body: &[
            "    int @ = value * #;",
            "    // @ owns the @",
            "    if (@ > #) { return @; }",
            "    String label = \"@-\" + #;",
            "    this.@.add(#);",
        ],
        footer: "}",
    },
    Language {
        ext: "md",
        header: &["## @ and @", "### The @ @"],
        body: &[
            "The @ talks to the @ every # ms.",
            "- `@` keeps # entries.",
            "- See the @ section for the @.",
            "",
        ],
        footer: "",
    },
    Language {
        ext: "yaml",
        header: &["@:", "@_@:"],
        body: &[
            "  @: #",
            "  @_@: \"@\"",
            "  # @ for the @",
            "  enabled: true",
        ],
        footer: "",
    },
    Language {
        ext: "json",
        header: &["{"],
        body: &["  \"@\": #,", "  \"@_@\": \"@\","],
        footer: "  \"end\": true\n},",
    },
    Language {
        ext: "toml",
        header: &["[@]", "[@.@]"],
        body: &["@ = #", "@_@ = \"@\"", "# @ of the @"],
        footer: "",
    },
];

/// Fills a template: `@` with a word, `#` with a number.
fn fill(template: &str, rng: &mut Rng) -> String {
    let mut line = String::with_capacity(template.len() + 32);
    for ch in template.chars() {
        match ch {
            '@' => line.push_str(rng.pick(WORDS)),
            '#' => line.push_str(&rng.below(10_000).to_string()),
            other => line.push(other),
        }
    }
    line
}

/// One block body line of `lang`.
pub fn body_line(lang: &Language, rng: &mut Rng) -> String {
    fill(rng.pick(lang.body), rng)
}

/// About `target` lines of `lang`, as blocks of a header, a body and a
/// footer.
pub fn source_lines(lang: &Language, target: usize, rng: &mut Rng) -> Vec<String> {
    let mut lines = Vec::with_capacity(target + 16);
    while lines.len() < target {
        lines.push(fill(rng.pick(lang.header), rng));
        for _ in 0..rng.between(3, 12) {
            lines.push(body_line(lang, rng));
        }
        lines.extend(lang.footer.lines().map(str::to_owned));
        lines.push(String::new());
    }
    lines
}

/// A file length: mostly small, sometimes long, like a real code base.
pub fn file_length(rng: &mut Rng) -> usize {
    match rng.below(100) {
        0..=59 => rng.between(15, 120),
        60..=94 => rng.between(120, 500),
        _ => rng.between(500, 2_500),
    }
}

/// Applies `edits` random edits (replace, insert or delete a few lines),
/// always leaving `lines` different from before.
pub fn edit_lines(lines: &mut Vec<String>, lang: &Language, edits: usize, rng: &mut Rng) {
    let before = lines.clone();
    for _ in 0..edits {
        let at = rng.below(lines.len() + 1);
        match rng.below(3) {
            0 if at < lines.len() => {
                let end = (at + rng.between(1, 5)).min(lines.len());
                for line in &mut lines[at..end] {
                    *line = body_line(lang, rng);
                }
            }
            1 if at < lines.len() && lines.len() > 8 => {
                let end = (at + rng.between(1, 5)).min(lines.len());
                lines.drain(at..end);
            }
            _ => {
                let added: Vec<String> = (0..rng.between(1, 8))
                    .map(|_| body_line(lang, rng))
                    .collect();
                lines.splice(at..at, added);
            }
        }
    }
    if *lines == before {
        lines.push(format!("// edited {}", rng.below(1_000_000)));
    }
}

/// Joins lines into file bytes, with a trailing newline.
pub fn to_bytes(lines: &[String]) -> Vec<u8> {
    let mut out = String::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out.into_bytes()
}

/// A fake PNG: the real signature (so it sniffs as binary), then noise
/// with NUL bytes.
pub fn binary_blob(rng: &mut Rng) -> Vec<u8> {
    let len = rng.between(1_024, 48 * 1_024);
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend(rng.bytes(len));
    bytes
}

/// A changed copy of a binary: a slice overwritten with noise.
pub fn edit_binary(bytes: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let start = rng.between(16, out.len().saturating_sub(1).max(16));
    let len = rng.between(16, 512);
    let noise = rng.bytes(len);
    for (offset, byte) in noise.into_iter().enumerate() {
        match out.get_mut(start + offset) {
            Some(slot) => *slot = byte,
            None => out.push(byte),
        }
    }
    out
}

/// `count` distinct nested directories (depth 1 to 6) under a few roots.
pub fn directories(count: usize, rng: &mut Rng) -> Vec<String> {
    const ROOTS: &[&str] = &["crates", "services", "web", "lib", "tools", "docs", "pkg"];
    let mut dirs: Vec<String> = ROOTS.iter().map(|root| (*root).to_owned()).collect();
    let mut seen: std::collections::HashSet<String> = dirs.iter().cloned().collect();
    while dirs.len() < count {
        let parent = rng.pick(&dirs).clone();
        if parent.matches('/').count() >= 5 {
            continue;
        }
        let dir = format!("{parent}/{}", rng.pick(WORDS));
        if seen.insert(dir.clone()) {
            dirs.push(dir);
        }
    }
    dirs
}

/// A file name of `ext` that is not in `taken` yet; records it.
pub fn unique_path(
    dir: &str,
    ext: &str,
    taken: &mut std::collections::HashSet<String>,
    rng: &mut Rng,
) -> String {
    loop {
        let name = match rng.below(3) {
            0 => rng.pick(WORDS).to_string(),
            1 => format!("{}_{}", rng.pick(WORDS), rng.pick(WORDS)),
            _ => format!("{}_{}", rng.pick(WORDS), rng.below(1_000)),
        };
        let path = format!("{dir}/{name}.{ext}");
        if taken.insert(path.clone()) {
            return path;
        }
    }
}

/// A word, for branch names and commit messages.
pub fn word(rng: &mut Rng) -> &'static str {
    rng.pick(WORDS)
}
