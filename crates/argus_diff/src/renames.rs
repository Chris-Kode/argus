//! Rename detection, ported from git's `diffcore-rename.c` and
//! `diffcore-delta.c` so that the pairs match `git diff -M`.
//!
//! Like git:
//! 1. exact renames (same object id), preferring a source with the same
//!    file name;
//! 2. pairs whose file name is unique among the remaining sources and
//!    destinations, when at least halfway between the threshold and 100%
//!    similar;
//! 3. a similarity matrix over the remaining `sources × destinations`,
//!    skipped when it exceeds the candidate-pair cap; the best four sources
//!    per destination are kept and pairs are assigned best-first.
//!
//! Similarity is git's: the share of the larger file made of 64-byte (or
//! line-terminated) chunks also present in the other file.

use std::collections::HashMap;

use argus_git::objects::{EntryKind, ObjectReader, TreeChange, TreeChangeStatus};
use argus_git::{CancelToken, Repo, Sha};

use crate::error::Result;
use crate::model::is_binary;

/// Git's similarity scale: 100% is 60000.
const MAX_SCORE: u64 = 60000;
/// Candidate sources kept per destination in the matrix.
const CANDIDATES_PER_DST: usize = 4;
/// Modulus of git's span hash.
const HASHBASE: u32 = 107_927;

/// Rename detection settings (spec §5.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenameOptions {
    /// Minimum similarity (0.0–1.0) for a delete/add pair to count as a
    /// rename. Git's default is 50%.
    pub min_similarity: f32,
    /// Maximum number of `deleted × added` candidate pairs scored by the
    /// similarity matrix. Above it only exact and same-file-name renames are
    /// detected and the rest stay additions and deletions, which keeps huge
    /// diffs from going quadratic. `0` means unlimited.
    pub max_candidate_pairs: usize,
}

impl Default for RenameOptions {
    fn default() -> Self {
        Self {
            min_similarity: 0.5,
            // Git's default `diff.renameLimit`: 1000 × 1000 pairs.
            max_candidate_pairs: 1000 * 1000,
        }
    }
}

/// A file of the list once renames are paired.
pub(crate) struct Paired {
    pub change: TreeChange,
    /// Source path when `change` is the destination of a rename.
    pub old_path: Option<String>,
}

/// Result of [`detect`].
pub(crate) struct Detected {
    /// Changes in path order (renames sorted by their new path).
    pub changes: Vec<Paired>,
    /// The similarity matrix was skipped because of the pair cap.
    pub limited: bool,
}

/// Pairs deletions and additions of `changes` (sorted by path) into
/// renames.
///
/// The similarity matrix is spread over `workers` threads; the result does
/// not depend on their number.
pub(crate) fn detect(
    repo: &Repo,
    changes: Vec<TreeChange>,
    options: Option<RenameOptions>,
    workers: usize,
    cancel: &CancelToken,
) -> Result<Detected> {
    let Some(options) = options else {
        return Ok(Detected {
            changes: unpaired(changes),
            limited: false,
        });
    };
    let mut sources: Vec<Candidate> = Vec::new();
    let mut dests: Vec<Candidate> = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        match change.status {
            TreeChangeStatus::Deleted => sources.extend(Candidate::new(index, change, change.old)),
            TreeChangeStatus::Added => dests.extend(Candidate::new(index, change, change.new)),
            TreeChangeStatus::Modified => {}
        }
    }
    let mut pairs: Vec<(usize, usize)> = Vec::new(); // (dst, src) positions
    let mut limited = false;
    if !sources.is_empty() && !dests.is_empty() {
        let reader = repo.object_reader();
        let mut tracker = Tracker {
            reader: &reader,
            sources,
            dests,
            used: Vec::new(),
            renamed: Vec::new(),
        };
        tracker.used = vec![false; tracker.sources.len()];
        tracker.renamed = vec![false; tracker.dests.len()];
        let min_score =
            (f64::from(options.min_similarity.clamp(0.0, 1.0)) * MAX_SCORE as f64).round() as u64;
        tracker.exact(&mut pairs);
        if min_score < MAX_SCORE {
            cancel.check()?;
            let basename_score = min_score + (MAX_SCORE - min_score) / 2;
            tracker.by_basename(basename_score, &mut pairs)?;
            let max_pairs = options.max_candidate_pairs;
            limited = tracker.matrix(repo, workers, min_score, max_pairs, cancel, &mut pairs)?;
        }
        pairs = pairs
            .into_iter()
            .map(|(dst, src)| (tracker.dests[dst].index, tracker.sources[src].index))
            .collect();
    }
    Ok(Detected {
        changes: merge(changes, &pairs),
        limited,
    })
}

fn unpaired(changes: Vec<TreeChange>) -> Vec<Paired> {
    changes
        .into_iter()
        .map(|change| Paired {
            change,
            old_path: None,
        })
        .collect()
}

/// Replaces each `(dst, src)` pair of change indices with one renamed change.
fn merge(changes: Vec<TreeChange>, pairs: &[(usize, usize)]) -> Vec<Paired> {
    let mut source_of: HashMap<usize, usize> = HashMap::new();
    let mut consumed = vec![false; changes.len()];
    for &(dst, src) in pairs {
        source_of.insert(dst, src);
        consumed[src] = true;
    }
    let mut slots: Vec<Option<TreeChange>> = changes.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(slots.len() - pairs.len());
    for index in 0..slots.len() {
        if consumed[index] {
            continue;
        }
        let Some(mut change) = slots[index].take() else {
            continue;
        };
        let old_path = match source_of.get(&index).and_then(|&src| slots[src].take()) {
            Some(source) => {
                change.old = source.old;
                Some(source.path)
            }
            None => None,
        };
        out.push(Paired { change, old_path });
    }
    out
}

struct Candidate {
    /// Index in the change list.
    index: usize,
    id: Sha,
    kind: EntryKind,
    path: String,
    /// Lazily read blob size.
    size: Option<u64>,
    /// Lazily computed span hashes.
    spans: Option<Vec<(u32, u32)>>,
}

impl Candidate {
    fn new(
        index: usize,
        change: &TreeChange,
        side: Option<argus_git::objects::TreeEntry>,
    ) -> Option<Self> {
        let side = side?;
        Some(Self {
            index,
            id: side.id,
            kind: side.kind,
            path: change.path.clone(),
            size: None,
            spans: None,
        })
    }

    fn is_regular(&self) -> bool {
        matches!(self.kind, EntryKind::File | EntryKind::Executable)
    }

    fn basename(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

struct Tracker<'a> {
    reader: &'a ObjectReader,
    sources: Vec<Candidate>,
    dests: Vec<Candidate>,
    /// Source already consumed by a rename.
    used: Vec<bool>,
    /// Destination already explained by a rename.
    renamed: Vec<bool>,
}

impl Tracker<'_> {
    fn record(&mut self, dst: usize, src: usize, pairs: &mut Vec<(usize, usize)>) {
        self.renamed[dst] = true;
        self.used[src] = true;
        pairs.push((dst, src));
    }

    /// git: `find_exact_renames`.
    fn exact(&mut self, pairs: &mut Vec<(usize, usize)>) {
        let mut by_id: HashMap<Sha, Vec<usize>> = HashMap::new();
        for (src, source) in self.sources.iter().enumerate() {
            by_id.entry(source.id).or_default().push(src);
        }
        for dst in 0..self.dests.len() {
            let Some(candidates) = by_id.get(&self.dests[dst].id) else {
                continue;
            };
            let target = &self.dests[dst];
            let mut best: Option<(usize, u8)> = None;
            for &src in candidates {
                let source = &self.sources[src];
                if self.used[src] {
                    continue;
                }
                if (!source.is_regular() || !target.is_regular()) && source.kind != target.kind {
                    continue;
                }
                let score = 1 + u8::from(basename_same(&source.path, &target.path));
                if best.is_none_or(|(_, best)| score > best) {
                    best = Some((src, score));
                    if score == 2 {
                        break;
                    }
                }
            }
            if let Some((src, _)) = best {
                self.record(dst, src, pairs);
            }
        }
    }

    /// git: `find_basename_matches`.
    fn by_basename(&mut self, min_score: u64, pairs: &mut Vec<(usize, usize)>) -> Result<()> {
        let unique = |candidates: &[Candidate], skip: &[bool]| {
            let mut seen: HashMap<&str, Option<usize>> = HashMap::new();
            for (at, candidate) in candidates.iter().enumerate() {
                if skip[at] {
                    continue;
                }
                seen.entry(candidate.basename())
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some(at));
            }
            seen.into_iter()
                .filter_map(|(name, at)| Some((name.to_owned(), at?)))
                .collect::<HashMap<String, usize>>()
        };
        let sources = unique(&self.sources, &self.used);
        let dests = unique(&self.dests, &self.renamed);
        for src in 0..self.sources.len() {
            if self.used[src] {
                continue;
            }
            let name = self.sources[src].basename();
            if sources.get(name) != Some(&src) {
                continue;
            }
            let Some(&dst) = dests.get(name) else {
                continue;
            };
            if self.renamed[dst] {
                continue;
            }
            if self.similarity(src, dst, min_score)? >= min_score {
                self.record(dst, src, pairs);
            }
        }
        Ok(())
    }

    /// git: the similarity matrix and `find_renames`. Returns whether it was
    /// skipped because of `max_pairs`.
    fn matrix(
        &mut self,
        repo: &Repo,
        workers: usize,
        min_score: u64,
        max_pairs: usize,
        cancel: &CancelToken,
        pairs: &mut Vec<(usize, usize)>,
    ) -> Result<bool> {
        let sources: Vec<usize> = (0..self.sources.len()).filter(|&s| !self.used[s]).collect();
        let dests: Vec<usize> = (0..self.dests.len())
            .filter(|&d| !self.renamed[d])
            .collect();
        if sources.is_empty() || dests.is_empty() {
            return Ok(false);
        }
        if max_pairs > 0 && sources.len().saturating_mul(dests.len()) > max_pairs {
            return Ok(true);
        }
        self.prepare(repo, workers, &sources, &dests, cancel)?;

        let (all_sources, all_dests) = (&self.sources, &self.dests);
        let rows = parallel_chunks(workers, &dests, |chunk| {
            let mut rows = Vec::with_capacity(chunk.len());
            for &dst in chunk {
                cancel.check()?;
                let mut best = [Score::UNUSED; CANDIDATES_PER_DST];
                for &src in &sources {
                    let (source, target) = (&all_sources[src], &all_dests[dst]);
                    let candidate = Score {
                        score: score(source, target, min_score),
                        name_score: u8::from(basename_same(&source.path, &target.path)),
                        dst: Some(dst),
                        src,
                    };
                    record_if_better(&mut best, candidate);
                }
                rows.push(best);
            }
            Ok(rows)
        })?;
        let mut scores: Vec<Score> = rows.into_iter().flatten().collect();
        // Stable, like git's STABLE_QSORT.
        scores.sort_by(Score::compare);
        for score in scores {
            let Some(dst) = score.dst else { break };
            if score.score < min_score {
                break;
            }
            if self.renamed[dst] || self.used[score.src] {
                continue;
            }
            self.record(dst, score.src, pairs);
        }
        Ok(false)
    }

    /// Reads sizes and span hashes of every matrix candidate, in parallel.
    fn prepare(
        &mut self,
        repo: &Repo,
        workers: usize,
        sources: &[usize],
        dests: &[usize],
        cancel: &CancelToken,
    ) -> Result<()> {
        let todo: Vec<(bool, usize, Sha)> = sources
            .iter()
            .map(|&src| (true, src, &self.sources[src]))
            .chain(dests.iter().map(|&dst| (false, dst, &self.dests[dst])))
            .filter(|(_, _, candidate)| candidate.is_regular() && candidate.spans.is_none())
            .map(|(is_source, at, candidate)| (is_source, at, candidate.id))
            .collect();
        let loaded = parallel_chunks(workers, &todo, |chunk| {
            let reader = repo.object_reader();
            let mut out = Vec::with_capacity(chunk.len());
            for &(_, _, id) in chunk {
                cancel.check()?;
                let bytes = reader.blob(id)?;
                out.push((bytes.len() as u64, span_hashes(&bytes)));
            }
            Ok(out)
        })?;
        for ((is_source, at, _), (size, spans)) in todo.into_iter().zip(loaded) {
            let candidate = if is_source {
                &mut self.sources[at]
            } else {
                &mut self.dests[at]
            };
            candidate.size = Some(size);
            candidate.spans = Some(spans);
        }
        Ok(())
    }

    /// git: `estimate_similarity`, loading what it needs on demand.
    fn similarity(&mut self, src: usize, dst: usize, min_score: u64) -> Result<u64> {
        if !self.sources[src].is_regular() || !self.dests[dst].is_regular() {
            return Ok(0);
        }
        let reader = self.reader;
        let src_size = size(reader, &mut self.sources[src])?;
        let dst_size = size(reader, &mut self.dests[dst])?;
        if !sizes_compatible(src_size, dst_size, min_score) {
            return Ok(0);
        }
        spans(reader, &mut self.sources[src])?;
        spans(reader, &mut self.dests[dst])?;
        Ok(score(&self.sources[src], &self.dests[dst], min_score))
    }
}

/// git: the size check of `estimate_similarity`: files whose sizes differ
/// too much can never reach `min_score` (git compares against the larger
/// size, whatever its comment says).
fn sizes_compatible(src_size: u64, dst_size: u64, min_score: u64) -> bool {
    let max_size = src_size.max(dst_size);
    let delta_size = max_size - src_size.min(dst_size);
    u128::from(max_size) * u128::from(MAX_SCORE - min_score)
        >= u128::from(delta_size) * u128::from(MAX_SCORE)
}

/// git: `estimate_similarity` on loaded candidates, on the 0..=60000 scale.
fn score(source: &Candidate, target: &Candidate, min_score: u64) -> u64 {
    if !source.is_regular() || !target.is_regular() {
        return 0;
    }
    let (Some(src_size), Some(dst_size)) = (source.size, target.size) else {
        return 0;
    };
    if !sizes_compatible(src_size, dst_size, min_score) || dst_size == 0 {
        return 0;
    }
    let (Some(src_spans), Some(dst_spans)) = (&source.spans, &target.spans) else {
        return 0;
    };
    let copied = copied_bytes(src_spans, dst_spans);
    (u128::from(copied) * u128::from(MAX_SCORE) / u128::from(src_size.max(dst_size))) as u64
}

/// Runs `work` over contiguous chunks of `items` on up to `workers` scoped
/// threads and concatenates the results in order.
fn parallel_chunks<I, T, F>(workers: usize, items: &[I], work: F) -> Result<Vec<T>>
where
    I: Sync,
    T: Send,
    F: Fn(&[I]) -> Result<Vec<T>> + Sync,
{
    let workers = workers.clamp(1, items.len().max(1));
    if workers == 1 {
        return work(items);
    }
    let chunk = items.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| scope.spawn(|| work(part)))
            .collect();
        let mut out = Vec::with_capacity(items.len());
        for handle in handles {
            match handle.join() {
                Ok(part) => out.extend(part?),
                Err(_) => {
                    return Err(crate::error::DiffError::Task(
                        "rename detection worker panicked".to_owned(),
                    ));
                }
            }
        }
        Ok(out)
    })
}

fn size(reader: &ObjectReader, candidate: &mut Candidate) -> Result<u64> {
    match candidate.size {
        Some(size) => Ok(size),
        None => {
            let size = reader.size(candidate.id)?;
            candidate.size = Some(size);
            Ok(size)
        }
    }
}

fn spans(reader: &ObjectReader, candidate: &mut Candidate) -> Result<()> {
    if candidate.spans.is_none() {
        candidate.spans = Some(span_hashes(&reader.blob(candidate.id)?));
    }
    Ok(())
}

/// git: `basename_same`.
fn basename_same(src: &str, dst: &str) -> bool {
    let (src, dst) = (src.as_bytes(), dst.as_bytes());
    let (mut s, mut d) = (src.len(), dst.len());
    while s > 0 && d > 0 {
        s -= 1;
        d -= 1;
        if src[s] != dst[d] {
            return false;
        }
        if src[s] == b'/' {
            return true;
        }
    }
    (s == 0 || src[s - 1] == b'/') && (d == 0 || dst[d - 1] == b'/')
}

#[derive(Clone, Copy, Debug)]
struct Score {
    score: u64,
    name_score: u8,
    /// `None` marks an unused slot.
    dst: Option<usize>,
    src: usize,
}

impl Score {
    const UNUSED: Self = Self {
        score: 0,
        name_score: 0,
        dst: None,
        src: 0,
    };

    /// git: `score_compare` (unused slots last, then higher score, then
    /// same file name).
    fn compare(a: &Self, b: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (a.dst, b.dst) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            _ if a.score == b.score => b.name_score.cmp(&a.name_score),
            _ => b.score.cmp(&a.score),
        }
    }
}

/// git: `record_if_better`.
fn record_if_better(best: &mut [Score; CANDIDATES_PER_DST], candidate: Score) {
    let mut worst = 0;
    for at in 1..CANDIDATES_PER_DST {
        if Score::compare(&best[at], &best[worst]).is_gt() {
            worst = at;
        }
    }
    if Score::compare(&best[worst], &candidate).is_gt() {
        best[worst] = candidate;
    }
}

/// git: `hash_chars`. Splits the content into chunks ending at a newline or
/// after 64 bytes and sums the bytes per chunk hash. Sorted by hash.
fn span_hashes(bytes: &[u8]) -> Vec<(u32, u32)> {
    let is_text = !is_binary(bytes);
    let mut counts: HashMap<u32, u32> = HashMap::new();
    let (mut accum1, mut accum2, mut n) = (0u32, 0u32, 0u32);
    let mut at = 0;
    while at < bytes.len() {
        let c = bytes[at];
        at += 1;
        // Ignore the CR of a CRLF in text.
        if is_text && c == b'\r' && bytes.get(at) == Some(&b'\n') {
            continue;
        }
        let old1 = accum1;
        accum1 = (accum1 << 7) ^ (accum2 >> 25);
        accum2 = (accum2 << 7) ^ (old1 >> 25);
        accum1 = accum1.wrapping_add(u32::from(c));
        n += 1;
        if n < 64 && c != b'\n' {
            continue;
        }
        let hash = accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASHBASE;
        *counts.entry(hash).or_default() += n;
        n = 0;
        accum1 = 0;
        accum2 = 0;
    }
    if n > 0 {
        let hash = accum1.wrapping_add(accum2.wrapping_mul(0x61)) % HASHBASE;
        *counts.entry(hash).or_default() += n;
    }
    let mut spans: Vec<(u32, u32)> = counts.into_iter().collect();
    spans.sort_unstable();
    spans
}

/// git: `diffcore_count_changes`, the `src_copied` part: bytes of the
/// source whose chunk also appears in the destination.
fn copied_bytes(src: &[(u32, u32)], dst: &[(u32, u32)]) -> u64 {
    let (mut i, mut j, mut copied) = (0, 0, 0u64);
    while i < src.len() && j < dst.len() {
        match src[i].0.cmp(&dst[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                copied += u64::from(src[i].1.min(dst[j].1));
                i += 1;
                j += 1;
            }
        }
    }
    copied
}
