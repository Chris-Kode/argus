//! Object access for the diff engine (spec §5.1): tree-to-tree changes and
//! blob reads.
//!
//! Everything here reads committed objects only; neither the user's index
//! nor their working tree is ever touched (spec §11).
//!
//! The `*_blocking` functions do real I/O and CPU work: call them only from
//! worker threads or tokio's blocking pool, never from the UI thread. The
//! async variants do that for you.

use gix::ObjectId;
use gix::bstr::ByteSlice as _;
use gix::diff::tree::recorder::Change;
use gix::objs::tree::EntryKind as GixEntryKind;

use crate::cancel::CancelToken;
use crate::error::{GitError, Result};
use crate::read::{Repo, Sha, blocking};

/// What a tree entry is, as far as diffing is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// Regular, non-executable file (`100644`).
    File,
    /// Executable file (`100755`).
    Executable,
    /// Symbolic link (`120000`); the blob holds the link target.
    Symlink,
    /// Submodule commit (`160000`); the id is a commit of another repository.
    Submodule,
}

impl EntryKind {
    /// Git's octal mode string, e.g. `100644`.
    pub fn mode(self) -> &'static str {
        match self {
            Self::File => "100644",
            Self::Executable => "100755",
            Self::Symlink => "120000",
            Self::Submodule => "160000",
        }
    }

    /// Whether the entry content is a blob of this repository.
    pub fn is_blob(self) -> bool {
        !matches!(self, Self::Submodule)
    }

    fn from_gix(kind: GixEntryKind) -> Option<Self> {
        match kind {
            GixEntryKind::Tree => None,
            GixEntryKind::Blob => Some(Self::File),
            GixEntryKind::BlobExecutable => Some(Self::Executable),
            GixEntryKind::Link => Some(Self::Symlink),
            GixEntryKind::Commit => Some(Self::Submodule),
        }
    }
}

/// One side of a changed tree entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TreeEntry {
    /// Object id: a blob, or a commit for submodules.
    pub id: Sha,
    /// Entry kind (mode).
    pub kind: EntryKind,
}

/// How an entry changed between the two trees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TreeChangeStatus {
    /// Only in the new tree.
    Added,
    /// Only in the old tree.
    Deleted,
    /// Same path in both trees, different id and/or mode.
    Modified,
}

/// A changed file (never a directory) between two trees. Renames are not
/// detected here: they appear as a deletion plus an addition, and the diff
/// engine pairs them up.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeChange {
    /// What happened.
    pub status: TreeChangeStatus,
    /// Repository-relative path. Non-UTF-8 bytes are replaced with `U+FFFD`.
    pub path: String,
    /// The entry in the old tree, `None` for additions.
    pub old: Option<TreeEntry>,
    /// The entry in the new tree, `None` for deletions.
    pub new: Option<TreeEntry>,
}

/// The two commits of a three-dot diff, fixed at resolution time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThreeDot {
    /// Tip of the base branch at resolution time.
    pub base: Sha,
    /// Tip of the head branch at resolution time.
    pub head: Sha,
    /// `merge-base(base, head)`, the left side of the diff.
    pub merge_base: Sha,
}

/// Resolves `base` and `head` (branch names or SHAs) and their merge-base in
/// one consistent read, for `git diff base...head`.
pub async fn three_dot(repo: &Repo, base: &str, head: &str) -> Result<ThreeDot> {
    let repo = repo.clone();
    let (base, head) = (base.to_owned(), head.to_owned());
    blocking(move |cancel| {
        let local = repo.local();
        let base_id = crate::read::resolve_commit(&local, &base)?;
        let head_id = crate::read::resolve_commit(&local, &head)?;
        cancel.check()?;
        let found = local
            .merge_base(base_id, head_id)
            .map_err(|err| GitError::gix("computing the merge-base", err))?;
        match found {
            Some(merge_base) => Ok(ThreeDot {
                base: base_id.into(),
                head: head_id.into(),
                merge_base: merge_base.detach().into(),
            }),
            None => Err(GitError::UnrelatedHistories { base, head }),
        }
    })
    .await
}

/// Async wrapper of [`tree_diff_blocking`] running on tokio's blocking pool.
/// Dropping the future cancels the job.
pub async fn tree_diff(
    repo: &Repo,
    from: Sha,
    to: Sha,
    cancel: &CancelToken,
) -> Result<Vec<TreeChange>> {
    let repo = repo.clone();
    let guard = cancel.child().drop_guard();
    let token = guard.token().clone();
    blocking(move |_| tree_diff_blocking(&repo, from, to, &token)).await
}

/// Lists the files that differ between the trees of `from` and `to`
/// (like `git diff --no-renames from to`), sorted by path. Each side is a
/// commit or a tree, e.g. one written from a run's worktree by
/// [`WorktreeManager::snapshot_tree`](crate::WorktreeManager::snapshot_tree). Directories are
/// never reported, only the files inside them.
pub fn tree_diff_blocking(
    repo: &Repo,
    from: Sha,
    to: Sha,
    cancel: &CancelToken,
) -> Result<Vec<TreeChange>> {
    let local = repo.local();
    let old_tree = peel_to_tree(&local, from)?;
    let new_tree = peel_to_tree(&local, to)?;
    cancel.check()?;

    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        gix::objs::TreeRefIter::from_bytes(&old_tree.data, local.object_hash()),
        gix::objs::TreeRefIter::from_bytes(&new_tree.data, local.object_hash()),
        &mut gix::diff::tree::State::default(),
        &local.objects,
        &mut recorder,
    )
    .map_err(|err| GitError::gix("diffing trees", err))?;
    cancel.check()?;

    let mut changes: Vec<TreeChange> = recorder.records.into_iter().filter_map(convert).collect();
    // Byte order of full paths is git's diff order.
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// The tree of a commit, or the tree itself.
fn peel_to_tree(repo: &gix::Repository, id: Sha) -> Result<gix::Tree<'_>> {
    repo.find_object(id.object_id())
        .map_err(|err| GitError::gix("reading an object", err))?
        .peel_to_tree()
        .map_err(|err| GitError::gix("reading a tree", err))
}

fn entry(mode: gix::objs::tree::EntryMode, id: ObjectId) -> Option<TreeEntry> {
    EntryKind::from_gix(mode.kind()).map(|kind| TreeEntry {
        id: id.into(),
        kind,
    })
}

fn convert(change: Change) -> Option<TreeChange> {
    let (status, path, old, new) = match change {
        Change::Addition {
            entry_mode,
            oid,
            path,
            ..
        } => (
            TreeChangeStatus::Added,
            path,
            None,
            Some(entry(entry_mode, oid)?),
        ),
        Change::Deletion {
            entry_mode,
            oid,
            path,
            ..
        } => (
            TreeChangeStatus::Deleted,
            path,
            Some(entry(entry_mode, oid)?),
            None,
        ),
        Change::Modification {
            previous_entry_mode,
            previous_oid,
            entry_mode,
            oid,
            path,
        } => {
            let old = entry(previous_entry_mode, previous_oid);
            let new = entry(entry_mode, oid);
            // A directory replaced by a file (or the reverse): keep only the
            // file side, as git does; the directory's files come separately.
            let status = match (&old, &new) {
                (Some(_), Some(_)) => TreeChangeStatus::Modified,
                (Some(_), None) => TreeChangeStatus::Deleted,
                (None, Some(_)) => TreeChangeStatus::Added,
                (None, None) => return None,
            };
            (status, path, old, new)
        }
    };
    Some(TreeChange {
        status,
        path: path.to_str_lossy().into_owned(),
        old,
        new,
    })
}

/// Reads objects from one thread. Create one per worker thread with
/// [`Repo::object_reader`]; it is cheap, but not shareable between threads.
pub struct ObjectReader {
    repo: gix::Repository,
}

impl Repo {
    /// A blob reader bound to the calling thread. Use only off the UI thread.
    pub fn object_reader(&self) -> ObjectReader {
        let mut repo = self.local();
        // Blobs are read once per diff; caching them would only churn memory.
        repo.object_cache_size(None);
        ObjectReader { repo }
    }
}

impl ObjectReader {
    /// Size in bytes of an object, from its header (no decompression of the
    /// content for loose objects; packed objects only decode their header).
    pub fn size(&self, id: Sha) -> Result<u64> {
        self.repo
            .find_header(id.object_id())
            .map(|header| header.size())
            .map_err(|err| GitError::gix("reading an object header", err))
    }

    /// Full content of a blob.
    pub fn blob(&self, id: Sha) -> Result<Vec<u8>> {
        let mut object = self
            .repo
            .find_object(id.object_id())
            .map_err(|err| GitError::gix("reading a blob", err))?;
        if object.kind != gix::objs::Kind::Blob {
            return Err(GitError::gix(
                "reading a blob",
                format!("object {id} is a {}, not a blob", object.kind),
            ));
        }
        Ok(std::mem::take(&mut object.data))
    }
}
