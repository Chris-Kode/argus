//! The perf fixture repositories (spec §12, issue #12).
//!
//! | Fixture | Content |
//! | --- | --- |
//! | `huge-files` | `base` → `head` changing 5,000 files across nested dirs: modifications, mode changes, adds, deletes, renames (pure and edited), binaries, in ten languages |
//! | `huge-file` | `base` → `head` on one 100,000-line Rust file; the diff is one ~125,000-line hunk |
//! | `many-branches` | 5,000 topic branches off `main`, plus `base` and `head` |
//!
//! Every fixture has `base` and `head` branches, so the app can open it
//! with `--base base --head head`; `head` is checked out.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use anyhow::Context as _;

use crate::content::{self, LANGUAGES};
use crate::fast_import::{self, Change, FastImport};
use crate::rng::Rng;

/// Every fixture, in generation order.
pub const NAMES: &[&str] = &["huge-files", "huge-file", "many-branches"];

/// Changed files in `huge-files`, split by kind (they sum to 5,000).
const MODIFIED_TEXT: usize = 3_990;
const MODE_ONLY: usize = 10;
const MODIFIED_BINARY: usize = 20;
const ADDED_TEXT: usize = 490;
const ADDED_BINARY: usize = 10;
const DELETED: usize = 250;
const RENAMED_PURE: usize = 130;
const RENAMED_EDITED: usize = 100;
/// Base files of `huge-files`; the ones not changed stay as context.
const BASE_TEXT: usize = 6_200;
const BASE_BINARY: usize = 30;
/// Nested directories of `huge-files`.
const DIRECTORIES: usize = 420;

/// Lines of the one file of `huge-file`.
const HUGE_FILE_LINES: usize = 100_000;
/// Topic branches of `many-branches`.
const BRANCHES: usize = 5_000;

/// Generates `names` into `out`, replacing existing copies, and prints
/// their SHAs (also written to `<out>/<name>.manifest`).
pub fn generate(out: &Path, seed: u64, names: &[&str]) -> anyhow::Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("failed to create {}", out.display()))?;
    println!(
        "Generating fixtures into {} (seed {seed:#x})",
        out.display()
    );
    for &name in names {
        let repo = out.join(name);
        if repo.exists() {
            std::fs::remove_dir_all(&repo)
                .with_context(|| format!("failed to remove the old {}", repo.display()))?;
        }
        let started = Instant::now();
        let mut rng = Rng::new(seed, name);
        let mut import = FastImport::start(&repo)?;
        match name {
            "huge-files" => huge_files(&mut import, &mut rng)?,
            "huge-file" => huge_file(&mut import, &mut rng)?,
            "many-branches" => many_branches(&mut import, &mut rng)?,
            other => anyhow::bail!("unknown fixture `{other}`"),
        }
        import.finish()?;
        let generated = started.elapsed();
        check_out_head(&repo)?;
        let manifest = manifest(&repo, name, seed)?;
        std::fs::write(out.join(format!("{name}.manifest")), &manifest)
            .with_context(|| format!("failed to write the manifest of {name}"))?;
        println!(
            "\n{name}: generated in {:.1?}, ready in {:.1?}\n{manifest}",
            generated,
            started.elapsed()
        );
    }
    Ok(())
}

/// Points `HEAD` at `head` and checks it out, so the fixture looks like a
/// normal clone (`git status` clean).
fn check_out_head(repo: &Path) -> anyhow::Result<()> {
    let mut symbolic = fast_import::git(repo);
    symbolic.args(["symbolic-ref", "HEAD", "refs/heads/head"]);
    fast_import::run(symbolic)?;
    let mut reset = fast_import::git(repo);
    reset.args(["reset", "--hard", "--quiet"]);
    fast_import::run(reset)?;
    // Real repositories with thousands of branches keep them packed.
    let mut pack = fast_import::git(repo);
    pack.args(["pack-refs", "--all"]);
    fast_import::run(pack)
}

/// The fixture's SHAs: `base`, `head`, and a fingerprint of every ref, so
/// two generations can be compared with `diff`.
fn manifest(repo: &Path, name: &str, seed: u64) -> anyhow::Result<String> {
    let mut refs = fast_import::git(repo);
    refs.args(["for-each-ref", "--format=%(objectname) %(refname)"]);
    let refs = fast_import::read(refs)?;
    let mut base = fast_import::git(repo);
    base.args(["rev-parse", "refs/heads/base"]);
    let mut head = fast_import::git(repo);
    head.args(["rev-parse", "refs/heads/head"]);
    let mut text = String::new();
    writeln!(text, "fixture   {name}")?;
    writeln!(text, "seed      {seed:#x}")?;
    writeln!(text, "base      {}", fast_import::read(base)?)?;
    writeln!(text, "head      {}", fast_import::read(head)?)?;
    writeln!(text, "refs      {}", refs.lines().count())?;
    writeln!(text, "refs-hash {:016x}", fnv1a(refs.as_bytes()))?;
    Ok(text)
}

/// FNV-1a, to fingerprint the ref list in one line.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A base file of `huge-files`.
struct BaseFile {
    path: String,
    /// Index in [`LANGUAGES`]; `None` for binaries.
    language: Option<usize>,
    content: Vec<u8>,
}

/// `huge-files`: one base commit, one head commit changing 5,000 files.
fn huge_files(import: &mut FastImport, rng: &mut Rng) -> anyhow::Result<()> {
    let dirs = content::directories(DIRECTORIES, rng);
    let mut taken = HashSet::new();

    let mut text = Vec::with_capacity(BASE_TEXT);
    for _ in 0..BASE_TEXT {
        let language = rng.below(LANGUAGES.len());
        let lang = &LANGUAGES[language];
        let path = content::unique_path(rng.pick(&dirs), lang.ext, &mut taken, rng);
        let length = content::file_length(rng);
        let lines = content::source_lines(lang, length, rng);
        text.push(BaseFile {
            path,
            language: Some(language),
            content: content::to_bytes(&lines),
        });
    }
    let mut binaries = Vec::with_capacity(BASE_BINARY);
    for _ in 0..BASE_BINARY {
        let path = content::unique_path(rng.pick(&dirs), "png", &mut taken, rng);
        binaries.push(BaseFile {
            path,
            language: None,
            content: content::binary_blob(rng),
        });
    }

    let base_changes: Vec<Change<'_>> = text
        .iter()
        .chain(&binaries)
        .map(|file| Change::Write {
            path: &file.path,
            content: &file.content,
            executable: false,
        })
        .collect();
    let base = import.commit(
        "base",
        None,
        0,
        "Base of the huge-files fixture\n",
        &base_changes,
    )?;
    drop(base_changes);

    // Partition the base text files into the change kinds; the rest stay.
    let mut order: Vec<usize> = (0..text.len()).collect();
    rng.shuffle(&mut order);
    let mut picks = order.into_iter();
    let mut take = |count: usize| picks.by_ref().take(count).collect::<Vec<_>>();
    let modified = take(MODIFIED_TEXT);
    let mode_only = take(MODE_ONLY);
    let deleted = take(DELETED);
    let renamed_pure = take(RENAMED_PURE);
    let renamed_edited = take(RENAMED_EDITED);

    // Owned head contents and paths first; the change list borrows them.
    let mut writes: Vec<(String, Vec<u8>, bool)> = Vec::new();
    for &index in &modified {
        let file = &text[index];
        let lang = &LANGUAGES[file.language.unwrap_or(0)];
        let mut lines = split_lines(&file.content);
        let edits = rng.between(1, 6);
        content::edit_lines(&mut lines, lang, edits, rng);
        writes.push((file.path.clone(), content::to_bytes(&lines), false));
    }
    for &index in &mode_only {
        let file = &text[index];
        writes.push((file.path.clone(), file.content.clone(), true));
    }
    for file in binaries.iter().take(MODIFIED_BINARY) {
        let changed = content::edit_binary(&file.content, rng);
        writes.push((file.path.clone(), changed, false));
    }
    for _ in 0..ADDED_TEXT {
        let lang = rng.pick(LANGUAGES);
        let path = content::unique_path(rng.pick(&dirs), lang.ext, &mut taken, rng);
        let length = content::file_length(rng);
        let lines = content::source_lines(lang, length, rng);
        writes.push((path, content::to_bytes(&lines), false));
    }
    for _ in 0..ADDED_BINARY {
        let path = content::unique_path(rng.pick(&dirs), "png", &mut taken, rng);
        writes.push((path, content::binary_blob(rng), false));
    }
    // Edited renames: a small edit keeps them well above git's 50%
    // similarity, so they are detected as renames, not delete + add.
    let mut renamed_edited_paths = Vec::new();
    for &index in &renamed_edited {
        let file = &text[index];
        let lang = &LANGUAGES[file.language.unwrap_or(0)];
        let to = content::unique_path(rng.pick(&dirs), lang.ext, &mut taken, rng);
        let mut lines = split_lines(&file.content);
        content::edit_lines(&mut lines, lang, 1, rng);
        writes.push((to, content::to_bytes(&lines), false));
        renamed_edited_paths.push(file.path.clone());
    }
    let mut renames = Vec::new();
    for &index in &renamed_pure {
        let file = &text[index];
        let lang = &LANGUAGES[file.language.unwrap_or(0)];
        let to = content::unique_path(rng.pick(&dirs), lang.ext, &mut taken, rng);
        renames.push((file.path.clone(), to));
    }

    let mut changes: Vec<Change<'_>> = Vec::new();
    for &index in &deleted {
        changes.push(Change::Delete(&text[index].path));
    }
    for path in &renamed_edited_paths {
        changes.push(Change::Delete(path));
    }
    for (from, to) in &renames {
        changes.push(Change::Rename { from, to });
    }
    for (path, content, executable) in &writes {
        changes.push(Change::Write {
            path,
            content,
            executable: *executable,
        });
    }
    import.commit(
        "head",
        Some(base),
        3_600,
        "Head of the huge-files fixture: 5,000 changed files\n",
        &changes,
    )?;
    Ok(())
}

/// `huge-file`: one Rust file of 100,000 lines with every fourth line
/// rewritten, so the whole file is a single hunk.
fn huge_file(import: &mut FastImport, rng: &mut Rng) -> anyhow::Result<()> {
    let lang = LANGUAGES
        .iter()
        .find(|lang| lang.ext == "rs")
        .unwrap_or(&LANGUAGES[0]);
    let mut lines = content::source_lines(lang, HUGE_FILE_LINES, rng);
    lines.truncate(HUGE_FILE_LINES);
    let base_content = content::to_bytes(&lines);
    let readme = b"# huge-file fixture\n\nOne 100,000-line file; see docs/perf.md.\n";
    let base = import.commit(
        "base",
        None,
        0,
        "Base of the huge-file fixture\n",
        &[
            Change::Write {
                path: "README.md",
                content: readme,
                executable: false,
            },
            Change::Write {
                path: "src/huge.rs",
                content: &base_content,
                executable: false,
            },
        ],
    )?;
    for line in lines.iter_mut().skip(1).step_by(4) {
        *line = content::body_line(lang, rng);
    }
    let head_content = content::to_bytes(&lines);
    import.commit(
        "head",
        Some(base),
        3_600,
        "Head of the huge-file fixture: rewrite every fourth line\n",
        &[Change::Write {
            path: "src/huge.rs",
            content: &head_content,
            executable: false,
        }],
    )?;
    Ok(())
}

/// `many-branches`: a small `main`, 5,000 one-commit topic branches off
/// it, and `base`/`head` for opening the repository.
fn many_branches(import: &mut FastImport, rng: &mut Rng) -> anyhow::Result<()> {
    let mut files = Vec::new();
    for index in 0..40 {
        let lang = &LANGUAGES[index % LANGUAGES.len()];
        let lines = content::source_lines(lang, rng.between(20, 200), rng);
        files.push((
            format!("src/{}_{index}.{}", content::word(rng), lang.ext),
            content::to_bytes(&lines),
        ));
    }
    let changes: Vec<Change<'_>> = files
        .iter()
        .map(|(path, content)| Change::Write {
            path,
            content,
            executable: false,
        })
        .collect();
    let main = import.commit(
        "main",
        None,
        0,
        "Base of the many-branches fixture\n",
        &changes,
    )?;
    import.branch("base", main)?;

    const PREFIXES: &[&str] = &["topic", "feature", "fix", "user/dev", "release"];
    for index in 0..BRANCHES {
        let prefix = PREFIXES[index % PREFIXES.len()];
        let branch = format!("{prefix}/{}-{index:04}", content::word(rng));
        let path = format!("notes/{index:04}.md");
        let body = format!(
            "# Note {index}\n\nThe {} keeps the {}.\n",
            content::word(rng),
            content::word(rng)
        );
        import.commit(
            &branch,
            Some(main),
            60 + index as u64 * 60,
            &format!("Note {index}\n"),
            &[Change::Write {
                path: &path,
                content: body.as_bytes(),
                executable: false,
            }],
        )?;
    }

    let (path, content) = &files[0];
    let mut lines = split_lines(content);
    content::edit_lines(&mut lines, &LANGUAGES[0], 4, rng);
    let head_content = content::to_bytes(&lines);
    import.commit(
        "head",
        Some(main),
        3_600,
        "Head of the many-branches fixture\n",
        &[Change::Write {
            path,
            content: &head_content,
            executable: false,
        }],
    )?;
    Ok(())
}

/// The lines of generated (UTF-8, `\n`-terminated) text.
fn split_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_owned)
        .collect()
}
