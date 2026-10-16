//! The file tree's data model, free of GPUI views so it stays cheap and easy
//! to reason about.
//!
//! [`TreeIndex`] is built once per file list (off the per-keystroke path):
//! the directory hierarchy, with chains of single-child directories
//! compressed into one node (`src/foo/bar`, like Zed), and every label
//! pre-rendered as a [`SharedString`] so building a visible row allocates
//! nothing. [`TreeIndex::rows`] then flattens it into the visible rows for the
//! current mode, filter and collapsed directories: one linear pass, which
//! keeps each filter keystroke on 5,000 files well under a frame.

use std::collections::{BTreeMap, HashSet};

use argus_diff::{ChangeKind, FileEntry};
use gpui::SharedString;

use crate::fuzzy::FuzzyQuery;

/// The change-kind groups the filter can toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KindGroup {
    /// New files.
    Added,
    /// Content, mode or type changes.
    Modified,
    /// Removed files.
    Deleted,
    /// Moved files.
    Renamed,
}

impl KindGroup {
    /// Every group, in the order the filter chips show them.
    pub const ALL: [KindGroup; 4] = [
        KindGroup::Added,
        KindGroup::Modified,
        KindGroup::Deleted,
        KindGroup::Renamed,
    ];

    /// The group a change kind belongs to.
    pub fn of(kind: ChangeKind) -> Self {
        match kind {
            ChangeKind::Added => KindGroup::Added,
            ChangeKind::Deleted => KindGroup::Deleted,
            ChangeKind::Renamed => KindGroup::Renamed,
            ChangeKind::Modified | ChangeKind::ModeChanged | ChangeKind::TypeChanged => {
                KindGroup::Modified
            }
        }
    }

    /// One-letter label of the filter chip.
    pub fn letter(self) -> &'static str {
        match self {
            KindGroup::Added => "A",
            KindGroup::Modified => "M",
            KindGroup::Deleted => "D",
            KindGroup::Renamed => "R",
        }
    }

    /// Tooltip-style description.
    pub fn label(self) -> &'static str {
        match self {
            KindGroup::Added => "Added",
            KindGroup::Modified => "Modified",
            KindGroup::Deleted => "Deleted",
            KindGroup::Renamed => "Renamed",
        }
    }

    fn bit(self) -> u8 {
        match self {
            KindGroup::Added => 1,
            KindGroup::Modified => 2,
            KindGroup::Deleted => 4,
            KindGroup::Renamed => 8,
        }
    }
}

/// The set of change-kind groups shown; empty means "all".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KindFilter(u8);

impl KindFilter {
    /// Whether no kind restriction applies.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether `group` is explicitly selected.
    pub fn contains(self, group: KindGroup) -> bool {
        self.0 & group.bit() != 0
    }

    /// Adds or removes `group`.
    pub fn toggle(&mut self, group: KindGroup) {
        self.0 ^= group.bit();
    }

    /// Whether a file of `kind` passes.
    pub fn allows(self, kind: ChangeKind) -> bool {
        self.is_empty() || self.contains(KindGroup::of(kind))
    }
}

/// The badge letter of a change kind (`T` for a type change, `M` for a mode
/// change, as `git status` shows them).
pub fn kind_letter(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "A",
        ChangeKind::Deleted => "D",
        ChangeKind::Modified | ChangeKind::ModeChanged => "M",
        ChangeKind::Renamed => "R",
        ChangeKind::TypeChanged => "T",
    }
}

/// Pre-rendered display data of one changed file.
#[derive(Debug)]
pub struct FileNode {
    /// Change kind.
    pub kind: ChangeKind,
    /// Full path in head (merge-base path for deletions).
    pub path: SharedString,
    /// Old path of a rename.
    pub old_path: Option<SharedString>,
    /// Row text in the tree: the file name, or `old → new` for renames
    /// (with the old full path when the file moved to another directory).
    pub tree_label: SharedString,
    /// Row text in the flat list: the full path, or `old → new`.
    pub flat_label: SharedString,
    /// Directory holding the file (index into [`TreeIndex::dir`]).
    pub parent: usize,
}

/// One child of a directory.
#[derive(Clone, Copy, Debug)]
pub enum Child {
    /// A (possibly compressed) directory.
    Dir(usize),
    /// A changed file, by file-list index.
    File(usize),
}

/// A directory, possibly standing for a chain of single-child directories.
#[derive(Debug)]
pub struct DirNode {
    /// Full path without trailing slash; the key of the collapsed state, so
    /// it survives a refresh of the file list.
    pub path: SharedString,
    /// Row text: the compressed segments with a trailing slash (`foo/bar/`).
    pub label: SharedString,
    /// Parent directory, `None` for the root.
    pub parent: Option<usize>,
    /// Directories first, then files, each in natural order (`m2` before
    /// `m10`).
    pub children: Vec<Child>,
}

/// One visible row of the tree or the flat list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A directory row.
    Dir {
        /// Index into [`TreeIndex::dir`].
        dir: usize,
        /// Indentation level.
        depth: usize,
        /// Whether its children are shown.
        expanded: bool,
    },
    /// A changed file.
    File {
        /// File-list index.
        index: usize,
        /// Indentation level.
        depth: usize,
    },
    /// Title of the "Outdated" group.
    OutdatedHeader {
        /// Number of outdated items.
        count: usize,
    },
    /// An outdated comment, by position in the outdated list.
    Outdated(usize),
}

/// What [`TreeIndex::rows`] shows.
pub struct View<'a> {
    /// Flat list instead of a tree.
    pub flat: bool,
    /// Fuzzy path filter.
    pub query: &'a FuzzyQuery,
    /// Change-kind filter.
    pub kinds: KindFilter,
    /// Collapsed directory paths (ignored while a filter is active, which
    /// shows every match).
    pub collapsed: &'a HashSet<SharedString>,
    /// Number of outdated items listed in the bottom group.
    pub outdated: usize,
}

impl View<'_> {
    fn filtering(&self) -> bool {
        !self.query.is_empty() || !self.kinds.is_empty()
    }
}

/// The directory hierarchy of one file list.
#[derive(Debug, Default)]
pub struct TreeIndex {
    files: Vec<FileNode>,
    dirs: Vec<DirNode>,
    root: usize,
    tree_order: Vec<usize>,
}

/// Scratch structure used while building.
#[derive(Default)]
struct Builder {
    dirs: BTreeMap<String, Builder>,
    files: Vec<(String, usize)>,
}

impl TreeIndex {
    /// Builds the index of `entries` (sorted by path, `entries[i].index ==
    /// i`).
    pub fn new(entries: &[FileEntry]) -> Self {
        let mut root = Builder::default();
        for entry in entries {
            let mut node = &mut root;
            let mut segments = entry.path.split('/').peekable();
            while let Some(segment) = segments.next() {
                if segments.peek().is_none() {
                    node.files.push((segment.to_owned(), entry.index));
                } else {
                    node = node.dirs.entry(segment.to_owned()).or_default();
                }
            }
        }

        let mut index = TreeIndex {
            files: entries
                .iter()
                .map(|entry| FileNode {
                    kind: entry.kind,
                    path: entry.path.clone().into(),
                    old_path: entry.old_path.clone().map(Into::into),
                    tree_label: tree_label(entry).into(),
                    flat_label: match &entry.old_path {
                        Some(old) => format!("{old} → {}", entry.path).into(),
                        None => entry.path.clone().into(),
                    },
                    parent: 0,
                })
                .collect(),
            ..Default::default()
        };
        index.root = index.add_dir(root, String::new(), String::new(), true);
        // Parents are known only once the directory is pushed; fix them up.
        for dir in 0..index.dirs.len() {
            for child in index.dirs[dir].children.clone() {
                match child {
                    Child::Dir(child) => index.dirs[child].parent = Some(dir),
                    Child::File(file) => index.files[file].parent = dir,
                }
            }
        }
        let mut order = Vec::with_capacity(index.files.len());
        index.collect_files(index.root, &mut order);
        index.tree_order = order;
        index
    }

    /// Pushes `builder` (and its subtree) and returns its index. Children are
    /// pushed before their parent.
    fn add_dir(
        &mut self,
        mut builder: Builder,
        mut path: String,
        mut label: String,
        is_root: bool,
    ) -> usize {
        // Compress `a/` → `b/` → `c/` chains of single-directory children
        // (never the root, whose label stays empty).
        while !is_root && builder.files.is_empty() && builder.dirs.len() == 1 {
            let Some((name, child)) = builder.dirs.pop_first() else {
                break;
            };
            path = format!("{path}/{name}");
            label = format!("{label}{name}/");
            builder = child;
        }
        let mut children = Vec::with_capacity(builder.dirs.len() + builder.files.len());
        let mut dirs: Vec<(String, Builder)> =
            std::mem::take(&mut builder.dirs).into_iter().collect();
        dirs.sort_by(|(a, _), (b, _)| natural_cmp(a, b));
        for (name, child) in dirs {
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let child_index = self.add_dir(child, child_path, format!("{name}/"), false);
            children.push(Child::Dir(child_index));
        }
        builder.files.sort_by(|(a, _), (b, _)| natural_cmp(a, b));
        children.extend(builder.files.into_iter().map(|(_, file)| Child::File(file)));
        self.dirs.push(DirNode {
            path: path.into(),
            label: label.into(),
            parent: None,
            children,
        });
        self.dirs.len() - 1
    }

    fn collect_files(&self, dir: usize, out: &mut Vec<usize>) {
        for child in &self.dirs[dir].children {
            match *child {
                Child::Dir(child) => self.collect_files(child, out),
                Child::File(file) => out.push(file),
            }
        }
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether there is no file.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The file at file-list index `index`.
    pub fn file(&self, index: usize) -> Option<&FileNode> {
        self.files.get(index)
    }

    /// The directory at `dir`.
    pub fn dir(&self, dir: usize) -> Option<&DirNode> {
        self.dirs.get(dir)
    }

    /// The root directory.
    pub fn root(&self) -> usize {
        self.root
    }

    /// The directory whose path is `path`.
    pub fn dir_by_path(&self, path: &str) -> Option<usize> {
        self.dirs
            .iter()
            .position(|dir| dir.path.as_ref() == path)
            .filter(|dir| *dir != self.root)
    }

    /// Every directory between the root (excluded) and `index`'s file.
    pub fn ancestors_of_file(&self, index: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let Some(file) = self.files.get(index) else {
            return out;
        };
        let mut dir = Some(file.parent);
        while let Some(current) = dir.filter(|dir| *dir != self.root) {
            out.push(current);
            dir = self.dirs[current].parent;
        }
        out
    }

    /// Which files pass the filter, by file-list index, with their score.
    fn matches(&self, view: &View<'_>) -> Vec<Option<i32>> {
        self.files
            .iter()
            .map(|file| {
                if !view.kinds.allows(file.kind) {
                    return None;
                }
                let score = view.query.score(&file.path);
                match &file.old_path {
                    Some(old) => score.max(view.query.score(old)),
                    None => score,
                }
            })
            .collect()
    }

    /// The visible files in display order: what `j`/`k` walk through. In
    /// the tree that is the tree order, ignoring collapsed directories (a
    /// jump reveals its target).
    pub fn file_order(&self, view: &View<'_>) -> Vec<usize> {
        let matches = self.matches(view);
        if view.flat {
            return self.flat_order(view, &matches);
        }
        self.tree_order
            .iter()
            .copied()
            .filter(|file| matches[*file].is_some())
            .collect()
    }

    fn flat_order(&self, view: &View<'_>, matches: &[Option<i32>]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.files.len())
            .filter(|file| matches[*file].is_some())
            .collect();
        if !view.query.is_empty() {
            // Best match first; ties keep path order (the list is sorted).
            order.sort_by_key(|file| std::cmp::Reverse(matches[*file].unwrap_or(i32::MIN)));
        }
        order
    }

    /// The visible rows for `view`.
    pub fn rows(&self, view: &View<'_>) -> Vec<Row> {
        let matches = self.matches(view);
        let mut rows = Vec::new();
        if view.flat {
            rows.extend(
                self.flat_order(view, &matches)
                    .into_iter()
                    .map(|index| Row::File { index, depth: 0 }),
            );
        } else if !self.files.is_empty() {
            let filtering = view.filtering();
            // Children are pushed before parents, so one forward pass knows
            // whether each directory holds a match.
            let mut has_match = vec![false; self.dirs.len()];
            for (dir, node) in self.dirs.iter().enumerate() {
                has_match[dir] = node.children.iter().any(|child| match *child {
                    Child::Dir(child) => has_match[child],
                    Child::File(file) => matches[file].is_some(),
                });
            }
            self.push_rows(
                self.root,
                0,
                &matches,
                &has_match,
                filtering,
                view.collapsed,
                &mut rows,
            );
        }
        if view.outdated > 0 {
            rows.push(Row::OutdatedHeader {
                count: view.outdated,
            });
            rows.extend((0..view.outdated).map(Row::Outdated));
        }
        rows
    }

    #[allow(clippy::too_many_arguments)]
    fn push_rows(
        &self,
        dir: usize,
        depth: usize,
        matches: &[Option<i32>],
        has_match: &[bool],
        filtering: bool,
        collapsed: &HashSet<SharedString>,
        rows: &mut Vec<Row>,
    ) {
        for child in &self.dirs[dir].children {
            match *child {
                Child::Dir(child) => {
                    if !has_match[child] {
                        continue;
                    }
                    let expanded = filtering || !collapsed.contains(&self.dirs[child].path);
                    rows.push(Row::Dir {
                        dir: child,
                        depth,
                        expanded,
                    });
                    if expanded {
                        self.push_rows(
                            child,
                            depth + 1,
                            matches,
                            has_match,
                            filtering,
                            collapsed,
                            rows,
                        );
                    }
                }
                Child::File(file) => {
                    if matches[file].is_some() {
                        rows.push(Row::File { index: file, depth });
                    }
                }
            }
        }
    }
}

/// The tree row text of `entry`.
fn tree_label(entry: &FileEntry) -> String {
    let (dir, name) = split_path(&entry.path);
    match &entry.old_path {
        Some(old) => {
            let (old_dir, old_name) = split_path(old);
            if old_dir == dir {
                format!("{old_name} → {name}")
            } else {
                format!("{old} → {name}")
            }
        }
        None => name.to_owned(),
    }
}

/// `("a/b", "c.rs")` for `a/b/c.rs`; `("", "c.rs")` at the top level.
fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", path),
    }
}

/// Natural order for names: runs of digits compare by value, the rest
/// case-insensitively; exact ties fall back to byte order so the result is
/// total.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    while let (Some(&cx), Some(&cy)) = (x.first(), y.first()) {
        if cx.is_ascii_digit() && cy.is_ascii_digit() {
            let run = |s: &[u8]| s.iter().take_while(|c| c.is_ascii_digit()).count();
            let (lx, ly) = (run(x), run(y));
            let (nx, ny) = (&x[..lx], &y[..ly]);
            // Compare by value without parsing: strip leading zeros, then
            // the longer number is larger, else compare digit by digit.
            let trim = |n: &'_ [u8]| -> usize { n.iter().take_while(|c| **c == b'0').count() };
            let (vx, vy) = (&nx[trim(nx)..], &ny[trim(ny)..]);
            let ordering = vx.len().cmp(&vy.len()).then_with(|| vx.cmp(vy));
            if ordering != Ordering::Equal {
                return ordering;
            }
            x = &x[lx..];
            y = &y[ly..];
        } else {
            let ordering = cx.to_ascii_lowercase().cmp(&cy.to_ascii_lowercase());
            if ordering != Ordering::Equal {
                return ordering;
            }
            x = &x[1..];
            y = &y[1..];
        }
    }
    x.len().cmp(&y.len()).then_with(|| a.cmp(b))
}
