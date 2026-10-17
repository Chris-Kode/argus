//! Language detection by file name, extension and shebang.

/// A language Argus bundles a grammar for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// Bash and POSIX shell scripts.
    Bash,
    /// C.
    C,
    /// C++.
    Cpp,
    /// C#.
    CSharp,
    /// CSS.
    Css,
    /// Go.
    Go,
    /// HTML.
    Html,
    /// Java.
    Java,
    /// JavaScript, JSX included.
    JavaScript,
    /// JSON.
    Json,
    /// Kotlin.
    Kotlin,
    /// Markdown (block structure; inline markup is injected).
    Markdown,
    /// Markdown inline markup; only reached through injections.
    MarkdownInline,
    /// PHP, with the HTML around `<?php` blocks.
    Php,
    /// Python.
    Python,
    /// Ruby.
    Ruby,
    /// Rust.
    Rust,
    /// SQL.
    Sql,
    /// Swift.
    Swift,
    /// TOML.
    Toml,
    /// TypeScript.
    TypeScript,
    /// TypeScript with JSX.
    Tsx,
    /// YAML.
    Yaml,
}

impl Language {
    /// Every language, in [`Language::index`] order.
    pub const ALL: [Language; 23] = [
        Language::Bash,
        Language::C,
        Language::Cpp,
        Language::CSharp,
        Language::Css,
        Language::Go,
        Language::Html,
        Language::Java,
        Language::JavaScript,
        Language::Json,
        Language::Kotlin,
        Language::Markdown,
        Language::MarkdownInline,
        Language::Php,
        Language::Python,
        Language::Ruby,
        Language::Rust,
        Language::Sql,
        Language::Swift,
        Language::Toml,
        Language::TypeScript,
        Language::Tsx,
        Language::Yaml,
    ];

    /// Dense index, for per-language tables.
    pub fn index(self) -> usize {
        self as usize
    }

    /// Display and injection name (`"rust"`, `"tsx"`).
    pub fn name(self) -> &'static str {
        match self {
            Language::Bash => "bash",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::CSharp => "c_sharp",
            Language::Css => "css",
            Language::Go => "go",
            Language::Html => "html",
            Language::Java => "java",
            Language::JavaScript => "javascript",
            Language::Json => "json",
            Language::Kotlin => "kotlin",
            Language::Markdown => "markdown",
            Language::MarkdownInline => "markdown_inline",
            Language::Php => "php",
            Language::Python => "python",
            Language::Ruby => "ruby",
            Language::Rust => "rust",
            Language::Sql => "sql",
            Language::Swift => "swift",
            Language::Toml => "toml",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::Yaml => "yaml",
        }
    }

    /// Detects the language of `path` (a repository-relative path): by file
    /// name first, then extension, then the shebang in `first_line`.
    /// `None` means plain text.
    pub fn detect(path: &str, first_line: Option<&str>) -> Option<Language> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        Self::from_file_name(name)
            .or_else(|| {
                let (stem, extension) = name.rsplit_once('.')?;
                // `.bashrc` has no extension; its name already matched.
                if stem.is_empty() {
                    return None;
                }
                Self::from_extension(&extension.to_ascii_lowercase())
            })
            .or_else(|| first_line.and_then(Self::from_shebang))
    }

    fn from_file_name(name: &str) -> Option<Language> {
        let language = match name {
            ".bashrc" | ".bash_profile" | ".bash_logout" | ".bash_aliases" | ".profile"
            | ".zshrc" | ".zshenv" | ".zprofile" | ".envrc" | "PKGBUILD" | "APKBUILD" => {
                Language::Bash
            }
            "Gemfile" | "Rakefile" | "Guardfile" | "Podfile" | "Fastfile" | "Vagrantfile"
            | "Brewfile" | "Capfile" | ".irbrc" | ".pryrc" | "config.ru" => Language::Ruby,
            "Cargo.lock" | "Pipfile" | "poetry.lock" | "uv.lock" | ".editorconfig" => {
                Language::Toml
            }
            ".clang-format" | ".clang-tidy" | ".clangd" => Language::Yaml,
            "SConstruct" | "SConscript" | "BUILD.bazel" | "WORKSPACE.bazel" | "Tiltfile" => {
                Language::Python
            }
            ".babelrc" | ".eslintrc" | ".prettierrc" | ".swcrc" | "composer.lock"
            | "flake.lock" => Language::Json,
            _ => return None,
        };
        Some(language)
    }

    fn from_extension(extension: &str) -> Option<Language> {
        let language = match extension {
            "sh" | "bash" | "zsh" | "ksh" | "bats" | "command" | "ebuild" => Language::Bash,
            "c" | "h" => Language::C,
            "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" | "inl" | "tpp"
            | "cu" | "cuh" | "ino" | "mm" => Language::Cpp,
            "cs" | "csx" => Language::CSharp,
            "css" => Language::Css,
            "go" => Language::Go,
            "html" | "htm" | "xhtml" | "shtml" | "vue" | "svelte" => Language::Html,
            "java" => Language::Java,
            "js" | "jsx" | "mjs" | "cjs" | "es6" => Language::JavaScript,
            "json" | "jsonc" | "json5" | "jsonl" | "ndjson" | "geojson" | "webmanifest"
            | "ipynb" => Language::Json,
            "kt" | "kts" => Language::Kotlin,
            "md" | "markdown" | "mdx" | "mkd" | "mdown" => Language::Markdown,
            "php" | "phtml" | "php3" | "php4" | "php5" | "php7" | "phps" => Language::Php,
            "py" | "pyi" | "pyw" | "pyx" | "bzl" | "star" | "gyp" => Language::Python,
            "rb" | "rbw" | "rake" | "gemspec" | "podspec" | "ru" | "erb" | "jbuilder" => {
                Language::Ruby
            }
            "rs" => Language::Rust,
            "sql" | "psql" | "pgsql" | "mysql" => Language::Sql,
            "swift" => Language::Swift,
            "toml" => Language::Toml,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "yaml" | "yml" => Language::Yaml,
            _ => return None,
        };
        Some(language)
    }

    /// `#!/usr/bin/env python3`, `#!/bin/bash -e`, …
    fn from_shebang(line: &str) -> Option<Language> {
        let command = line.strip_prefix("#!")?.trim();
        let mut words = command.split_whitespace();
        let mut program = words.next()?.rsplit('/').next()?;
        if program == "env" {
            // Skip `env` options such as `-S`.
            program = words.find(|word| !word.starts_with('-'))?;
        }
        let program = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        let language = match program {
            "sh" | "bash" | "zsh" | "ksh" | "dash" | "ash" => Language::Bash,
            "python" | "pypy" => Language::Python,
            "ruby" => Language::Ruby,
            "node" | "nodejs" | "deno" | "bun" => Language::JavaScript,
            "ts-node" | "tsx" => Language::TypeScript,
            "php" => Language::Php,
            "kotlin" | "kotlinc" => Language::Kotlin,
            "swift" => Language::Swift,
            _ => return None,
        };
        Some(language)
    }

    /// The language an injection names (a Markdown code-fence info string,
    /// an HTML `<script>` type, a grammar's injection query).
    pub fn from_injection_name(name: &str) -> Option<Language> {
        let name = name.trim().to_ascii_lowercase();
        let language = match name.as_str() {
            "bash" | "sh" | "shell" | "zsh" | "console" | "shellscript" => Language::Bash,
            "c" | "h" => Language::C,
            "cpp" | "c++" | "cxx" | "cc" | "hpp" => Language::Cpp,
            "c_sharp" | "csharp" | "c#" | "cs" => Language::CSharp,
            "css" => Language::Css,
            "go" | "golang" => Language::Go,
            "html" | "htm" | "xhtml" => Language::Html,
            "java" => Language::Java,
            "javascript" | "js" | "jsx" | "mjs" | "cjs" | "ecmascript" => Language::JavaScript,
            "json" | "jsonc" | "json5" => Language::Json,
            "kotlin" | "kt" | "kts" => Language::Kotlin,
            "markdown" | "md" => Language::Markdown,
            "markdown_inline" => Language::MarkdownInline,
            "php" => Language::Php,
            "python" | "py" | "python3" => Language::Python,
            "ruby" | "rb" => Language::Ruby,
            "rust" | "rs" => Language::Rust,
            "sql" | "postgresql" | "postgres" | "mysql" | "sqlite" => Language::Sql,
            "swift" => Language::Swift,
            "toml" => Language::Toml,
            "typescript" | "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "yaml" | "yml" => Language::Yaml,
            _ => return None,
        };
        Some(language)
    }
}
