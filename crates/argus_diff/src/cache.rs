//! A small LRU cache of [`FileDiff`]s bounded by memory size.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::FileDiff;

/// Least-recently-used cache keyed by file-list index, evicting until the
/// summed [`FileDiff::memory_size`] fits the budget. One entry larger than
/// the whole budget is still kept (alone), so the open file is never
/// recomputed on every access.
pub(crate) struct DiffCache {
    budget: usize,
    used: usize,
    tick: u64,
    entries: HashMap<usize, Slot>,
    /// `last use tick → key`, oldest first.
    order: BTreeMap<u64, usize>,
}

struct Slot {
    diff: Arc<FileDiff>,
    size: usize,
    tick: u64,
}

impl DiffCache {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            tick: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    pub(crate) fn get(&mut self, key: usize) -> Option<Arc<FileDiff>> {
        self.tick += 1;
        let tick = self.tick;
        let slot = self.entries.get_mut(&key)?;
        self.order.remove(&slot.tick);
        slot.tick = tick;
        self.order.insert(tick, key);
        Some(slot.diff.clone())
    }

    pub(crate) fn insert(&mut self, key: usize, diff: Arc<FileDiff>) {
        if self.budget == 0 {
            return;
        }
        self.remove(key);
        self.tick += 1;
        let size = diff.memory_size();
        self.used += size;
        self.order.insert(self.tick, key);
        self.entries.insert(
            key,
            Slot {
                diff,
                size,
                tick: self.tick,
            },
        );
        while self.used > self.budget && self.entries.len() > 1 {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some(slot) = self.entries.remove(&oldest) {
                self.used -= slot.size;
            }
        }
    }

    fn remove(&mut self, key: usize) {
        if let Some(slot) = self.entries.remove(&key) {
            self.order.remove(&slot.tick);
            self.used -= slot.size;
        }
    }
}
