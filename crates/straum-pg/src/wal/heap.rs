//! The rows on the heap pages of the tables Straum reads, kept as redo
//! keeps them, so a delete or an update that names only a slot on a page
//! finds the row it changes. Layouts: `src/include/access/heapam_xlog.h`
//! and `htup_details.h`, applied as `heap_redo` and `heap2_redo` in
//! `src/backend/access/heap/heapam_xlog.c` do.
//!
//! Every page comes in whole the first time it changes after the redo
//! point Straum starts from: as a full-page image, or as a page the record
//! starts afresh. So a page Straum has nothing of is an error, never a
//! guess.

mod edit;
mod page;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;

use straum_domain::{FastMap, FastSet};

use super::{
    Block, Problem, RESOURCE_MANAGER_HEAP, RESOURCE_MANAGER_HEAP2, RESOURCE_MANAGER_INFO,
    RESOURCE_MANAGER_STORAGE, RESOURCE_MANAGER_XLOG, Record, RelationFile, WalError,
};
use edit::Edit;
pub(crate) use page::Page;
use page::{TUPLE_HEADER, page_from_image, restore};

pub(super) const XLOG_CHECKPOINT_SHUTDOWN: u8 = 0x00;
pub(super) const XLOG_CHECKPOINT_ONLINE: u8 = 0x10;

pub(super) const OPERATION: u8 = 0x70;
pub(super) const INIT_PAGE: u8 = 0x80;

pub(super) const HEAP_INSERT: u8 = 0x00;
pub(super) const HEAP_DELETE: u8 = 0x10;
pub(super) const HEAP_UPDATE: u8 = 0x20;
pub(super) const HEAP_HOT_UPDATE: u8 = 0x40;
pub(super) const HEAP_CONFIRM: u8 = 0x50;

/// `PRUNE_ON_ACCESS`, `PRUNE_VACUUM_SCAN` and `PRUNE_VACUUM_CLEANUP`: one
/// layout, prune, vacuum and freeze in one record.
pub(super) const HEAP2_PRUNE: u8 = 0x10;
pub(super) const HEAP2_VACUUM: u8 = 0x20;
pub(super) const HEAP2_PRUNE_CLEANUP: u8 = 0x30;
pub(super) const HEAP2_MULTI_INSERT: u8 = 0x50;

pub(super) const SMGR_TRUNCATE: u8 = 0x20;

/// `MAIN_FORKNUM`: the table's rows, not its free space or visibility maps.
pub(super) const MAIN_FORK: u8 = 0;

/// A row as it is on its page, without the fixed header, whose
/// transaction fields Straum does not need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeapTuple {
    pub(crate) infomask2: u16,
    pub(crate) infomask: u16,
    /// Where the columns start, counted from the start of the header.
    pub(crate) hoff: u8,
    /// The null bitmap, its padding, then the columns. Shared: the page
    /// and the change it makes hold the same row.
    pub(crate) body: Arc<[u8]>,
}

impl HeapTuple {
    /// The columns, after the null bitmap and its padding.
    pub(crate) fn columns(&self) -> Option<&[u8]> {
        self.body
            .get(usize::from(self.hoff).checked_sub(TUPLE_HEADER)?..)
    }
}

/// A change to a row of a table Straum reads, in WAL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeapChange {
    Insert {
        file: RelationFile,
        new: HeapTuple,
    },
    Update {
        file: RelationFile,
        old: HeapTuple,
        new: HeapTuple,
    },
    Delete {
        file: RelationFile,
        old: HeapTuple,
    },
}

impl HeapChange {
    /// The bytes the change holds, near enough to bound memory by.
    pub(crate) fn footprint(&self) -> usize {
        let rows = match self {
            HeapChange::Insert { new, .. } => new.body.len(),
            HeapChange::Update { old, new, .. } => old.body.len() + new.body.len(),
            HeapChange::Delete { old, .. } => old.body.len(),
        };
        size_of::<HeapChange>() + rows
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PageId {
    pub(crate) file: RelationFile,
    pub(crate) block: u32,
}

/// The pages of the tracked relations, and how to read prune records.
pub(crate) struct Heap {
    tracked: FastSet<RelationFile>,
    pages: FastMap<PageId, Page>,
    /// The pages' footprints, summed.
    bytes: usize,
    /// 19 drops the reason byte before the prune flags and widens them to
    /// two bytes, the second for the visibility map.
    prune_flags_first: bool,
}

impl Heap {
    pub(crate) fn new(tracked: HashSet<RelationFile>, major: u32) -> Heap {
        Heap {
            tracked: tracked.into_iter().collect(),
            pages: FastMap::default(),
            bytes: 0,
            prune_flags_first: major >= 19,
        }
    }

    #[cfg(test)]
    pub(crate) fn page(&self, id: PageId) -> Option<&Page> {
        self.pages.get(&id)
    }

    #[cfg(test)]
    pub(crate) fn pages(&self) -> impl Iterator<Item = (&PageId, &Page)> {
        self.pages.iter()
    }

    /// The bytes the pages hold.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// The bytes the pages hold, counted afresh.
    #[cfg(test)]
    pub(crate) fn recount(&self) -> usize {
        (self.pages.values())
            .map(|page| page.footprint() - page.rows + rows_counted(page))
            .sum()
    }

    /// Applies the record to the tracked pages, and returns the changes it
    /// makes to their rows.
    pub(crate) fn apply(&mut self, record: &Record) -> Result<Vec<HeapChange>, WalError> {
        let lsn = record.lsn;
        let mut restored = Vec::new();
        for block in &record.blocks {
            if !self.is_tracked(block) {
                continue;
            }
            let id = page_id(block);
            match &block.image {
                Some(image) if image.apply => {
                    let page = page_from_image(&restore(record, image)?)
                        .ok_or(problem(lsn, Problem::UnreadableImage))?;
                    self.store(id, page);
                    restored.push(block.id);
                }
                _ if block.will_init => self.store(id, Page::default()),
                _ => {}
            }
        }
        let changes = self.edit(record, &restored)?;
        // Every record that names a page sets its LSN, whether or not it
        // changes a row Straum reads: a lock, a freeze.
        for block in &record.blocks {
            if self.is_tracked(block)
                && let Some(page) = self.pages.get_mut(&page_id(block))
            {
                page.lsn = record.next();
                let footprint = page.footprint();
                self.bytes = self.bytes - page.counted + footprint;
                page.counted = footprint;
            }
        }
        if record.resource_manager == RESOURCE_MANAGER_XLOG
            && let XLOG_CHECKPOINT_SHUTDOWN | XLOG_CHECKPOINT_ONLINE =
                record.info & RESOURCE_MANAGER_INFO
        {
            // `CheckPoint` opens with `redo`.
            let Some(redo) = record.main_data().first_chunk() else {
                return Err(problem(lsn, Problem::CheckpointWithoutRedo));
            };
            self.evict(u64::from_le_bytes(*redo));
        }
        Ok(changes)
    }

    /// Lets go of every page last changed by the redo point: its next
    /// change brings it whole, as a full-page image (`XLogInsert` takes one
    /// when the page LSN is at or before `RedoRecPtr`).
    fn evict(&mut self, redo: u64) {
        self.forget(|_, page| page.lsn <= redo);
    }

    fn store(&mut self, id: PageId, mut page: Page) {
        page.counted = page.footprint();
        self.bytes += page.counted;
        if let Some(old) = self.pages.insert(id, page) {
            self.bytes -= old.counted;
        }
    }

    fn forget(&mut self, gone: impl Fn(&PageId, &Page) -> bool) {
        let mut freed = 0;
        self.pages.retain(|id, page| {
            let go = gone(id, page);
            if go {
                freed += page.counted;
            }
            !go
        });
        self.bytes -= freed;
    }

    fn edit(&mut self, record: &Record, restored: &[u8]) -> Result<Vec<HeapChange>, WalError> {
        let mut edit = Edit {
            heap: self,
            record,
            restored,
            changes: Vec::new(),
        };
        let operation = record.info & OPERATION;
        match record.resource_manager {
            RESOURCE_MANAGER_HEAP => match operation {
                HEAP_INSERT => edit.insert()?,
                HEAP_DELETE => edit.delete()?,
                HEAP_UPDATE | HEAP_HOT_UPDATE => edit.update()?,
                HEAP_CONFIRM => edit.confirm()?,
                _ => {}
            },
            RESOURCE_MANAGER_HEAP2 => match operation {
                HEAP2_MULTI_INSERT => edit.multi_insert()?,
                HEAP2_PRUNE | HEAP2_VACUUM | HEAP2_PRUNE_CLEANUP => edit.prune()?,
                _ => {}
            },
            RESOURCE_MANAGER_STORAGE if operation & RESOURCE_MANAGER_INFO == SMGR_TRUNCATE => {
                edit.truncate()?;
            }
            _ => {}
        }
        Ok(edit.changes)
    }

    fn is_tracked(&self, block: &Block) -> bool {
        block.fork == MAIN_FORK && self.tracked.contains(&block.file)
    }
}

#[cfg(test)]
fn rows_counted(page: &Page) -> usize {
    (page.tuples.iter().flatten())
        .map(|tuple| tuple.body.len())
        .sum()
}

fn page_id(block: &Block) -> PageId {
    PageId {
        file: block.file,
        block: block.number,
    }
}

fn problem(lsn: u64, problem: Problem) -> WalError {
    WalError::At { lsn, problem }
}
