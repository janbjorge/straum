//! The rows on the heap pages of the tables Straum reads: a heap is
//! PostgreSQL's table storage, pages of rows. Each record that touches a
//! tracked page comes in and leaves as row changes, held next by
//! transaction. Pages are kept as redo keeps them (`heap_redo` in
//! `heapam_xlog.c`), so a delete or update naming only a slot finds its
//! row. A page comes whole the first time it changes after the redo point,
//! so one Straum lacks is an error. A change shares its row's bytes with
//! the page (`Arc<[u8]>`): a row is stored once.

mod edit;
mod page;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;

use straum_domain::{FastMap, FastSet};

use super::{
    Block, BlockNumber, Problem, RESOURCE_MANAGER_HEAP, RESOURCE_MANAGER_HEAP2,
    RESOURCE_MANAGER_INFO, RESOURCE_MANAGER_STORAGE, RESOURCE_MANAGER_XLOG, Record, RelationFile,
    WalError, at,
};
use edit::{Edit, PruneLayout};
pub(crate) use page::Page;
pub(in crate::wal) use page::TUPLE_HEADER;
use page::{page_from_image, restore};

/// `XLOG_CHECKPOINT_SHUTDOWN`: the checkpoint a clean shutdown writes.
pub(super) const XLOG_CHECKPOINT_SHUTDOWN: u8 = 0x00;
/// `XLOG_CHECKPOINT_ONLINE`: a checkpoint while the server runs.
pub(super) const XLOG_CHECKPOINT_ONLINE: u8 = 0x10;

/// `XLOG_HEAP_OPMASK`: the record kind within `xl_info`.
pub(super) const OPERATION: u8 = 0x70;
/// `XLOG_HEAP_INIT_PAGE`: the record starts its page afresh.
pub(super) const INIT_PAGE: u8 = 0x80;

/// `XLOG_HEAP_INSERT`.
pub(super) const HEAP_INSERT: u8 = 0x00;
/// `XLOG_HEAP_DELETE`.
pub(super) const HEAP_DELETE: u8 = 0x10;
/// `XLOG_HEAP_UPDATE`: the new row may be on another page.
pub(super) const HEAP_UPDATE: u8 = 0x20;
/// `XLOG_HEAP_HOT_UPDATE`: the new row is on the old row's page.
pub(super) const HEAP_HOT_UPDATE: u8 = 0x40;
/// `XLOG_HEAP_CONFIRM`: a speculative insert stands.
pub(super) const HEAP_CONFIRM: u8 = 0x50;

// `PRUNE_ON_ACCESS`, `PRUNE_VACUUM_SCAN` and `PRUNE_VACUUM_CLEANUP`: one
// layout, prune, vacuum and freeze in one record.
/// `XLOG_HEAP2_PRUNE_ON_ACCESS`.
pub(super) const HEAP2_PRUNE: u8 = 0x10;
/// `XLOG_HEAP2_PRUNE_VACUUM_SCAN`.
pub(super) const HEAP2_VACUUM: u8 = 0x20;
/// `XLOG_HEAP2_PRUNE_VACUUM_CLEANUP`.
pub(super) const HEAP2_PRUNE_CLEANUP: u8 = 0x30;
/// `XLOG_HEAP2_MULTI_INSERT`: `COPY` and other bulk inserts.
pub(super) const HEAP2_MULTI_INSERT: u8 = 0x50;

/// `XLOG_SMGR_TRUNCATE`: a relation file cut short.
pub(super) const SMGR_TRUNCATE: u8 = 0x20;

/// `MAIN_FORKNUM`: the table's rows, not its free space or visibility maps.
pub(super) const MAIN_FORK: u8 = 0;

/// A row as it is on its page, without the fixed header, whose
/// transaction fields Straum does not need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeapTuple {
    /// `t_infomask2`: the number of columns, and flags.
    pub(crate) infomask2: u16,
    /// `t_infomask`: flags, such as whether there is a null bitmap.
    pub(crate) infomask: u16,
    /// `t_hoff`: where the columns start, counted from the start of the
    /// header.
    pub(crate) hoff: u8,
    /// The null bitmap, its padding, then the columns. Shared: the page
    /// and the change it makes hold the same row.
    pub(crate) body: Arc<[u8]>,
}

impl HeapTuple {
    /// The columns, after the null bitmap and its padding.
    pub(crate) fn columns(&self) -> Option<&[u8]> {
        let start = usize::from(self.hoff).checked_sub(TUPLE_HEADER)?;
        self.body.get(start..)
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

/// One page of one relation file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PageId {
    pub(crate) file: RelationFile,
    pub(crate) block: BlockNumber,
}

/// The pages of the tracked relations, and how to read prune records.
pub(crate) struct Heap {
    tracked: FastSet<RelationFile>,
    pages: FastMap<PageId, Page>,
    /// The pages' footprints, summed.
    bytes: usize,
    prune_layout: PruneLayout,
}

impl Heap {
    /// No pages yet, for the relation files in `tracked`, read as
    /// PostgreSQL `major` writes them.
    pub(crate) fn new(tracked: HashSet<RelationFile>, major: u32) -> Heap {
        Heap {
            tracked: tracked.into_iter().collect(),
            pages: FastMap::default(),
            bytes: 0,
            prune_layout: PruneLayout::of_major(major),
        }
    }

    /// The page `id`, if Straum keeps it.
    #[cfg(test)]
    pub(crate) fn page(&self, id: PageId) -> Option<&Page> {
        self.pages.get(&id)
    }

    /// Every page kept, in no order.
    // `impl Iterator`: callers filter and count the pages without a copy.
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
        let mut bytes = 0;
        for page in self.pages.values() {
            let mut rows = 0;
            for tuple in page.tuples.iter().flatten() {
                rows += tuple.body.len();
            }
            bytes += page.footprint() - page.rows + rows;
        }
        bytes
    }

    /// Applies the record to the tracked pages, and returns the changes it
    /// makes to their rows.
    pub(crate) fn apply(&mut self, record: &Record) -> Result<Vec<HeapChange>, WalError> {
        let mut edit = Edit {
            restored: Vec::new(),
            changes: Vec::new(),
        };
        self.take_images(record, &mut edit)?;
        self.redo(record, &mut edit)?;
        self.count_changed_pages(record);
        if is_checkpoint(record) {
            // `CheckPoint` opens with `redo`.
            let Some(redo) = record.main_data().first_chunk() else {
                return Err(at(record.lsn, Problem::CheckpointWithoutRedo));
            };
            self.evict(u64::from_le_bytes(*redo));
        }
        Ok(edit.changes)
    }

    /// Stores each tracked page the record brings whole: a full-page image
    /// to restore, or a page it starts afresh.
    fn take_images(&mut self, record: &Record, edit: &mut Edit) -> Result<(), WalError> {
        for block in &record.blocks {
            if !self.is_tracked(block) {
                continue;
            }
            let id = page_id(block);
            match &block.image {
                Some(image) if image.apply => {
                    let Some(page) = page_from_image(&restore(record, image)?) else {
                        return Err(at(record.lsn, Problem::UnreadableImage));
                    };
                    self.store(id, page);
                    edit.restored.push(block.id);
                }
                _ if block.will_init => self.store(id, Page::default()),
                _ => {}
            }
        }
        Ok(())
    }

    /// Every record that names a page sets its LSN, whether or not it
    /// changes a row Straum reads: a lock, a freeze. Counts each such
    /// page's bytes again.
    fn count_changed_pages(&mut self, record: &Record) {
        for block in &record.blocks {
            if !self.is_tracked(block) {
                continue;
            }
            let Some(page) = self.pages.get_mut(&page_id(block)) else {
                continue;
            };
            page.lsn = record.next();
            let footprint = page.footprint();
            self.bytes = self.bytes - page.counted + footprint;
            page.counted = footprint;
        }
    }

    /// Lets go of every page last changed by the redo point: its next
    /// change brings it whole, as a full-page image (`XLogInsert` takes one
    /// when the page LSN is at or before `RedoRecPtr`).
    fn evict(&mut self, redo: u64) {
        let mut gone = Vec::new();
        for (id, page) in &self.pages {
            if page.lsn <= redo {
                gone.push(*id);
            }
        }
        self.forget(&gone);
    }

    fn store(&mut self, id: PageId, mut page: Page) {
        page.counted = page.footprint();
        self.bytes += page.counted;
        if let Some(old) = self.pages.insert(id, page) {
            self.bytes -= old.counted;
        }
    }

    fn forget(&mut self, gone: &[PageId]) {
        for id in gone {
            if let Some(page) = self.pages.remove(id) {
                self.bytes -= page.counted;
            }
        }
    }

    /// Redoes the record's changes to rows, as its resource manager does.
    fn redo(&mut self, record: &Record, edit: &mut Edit) -> Result<(), WalError> {
        let operation = record.info & OPERATION;
        match record.resource_manager {
            RESOURCE_MANAGER_HEAP => match operation {
                HEAP_INSERT => self.insert(record, edit),
                HEAP_DELETE => self.delete(record, edit),
                HEAP_UPDATE | HEAP_HOT_UPDATE => self.update(record, edit),
                HEAP_CONFIRM => self.confirm(record, edit),
                _ => Ok(()),
            },
            RESOURCE_MANAGER_HEAP2 => match operation {
                HEAP2_MULTI_INSERT => self.multi_insert(record, edit),
                HEAP2_PRUNE | HEAP2_VACUUM | HEAP2_PRUNE_CLEANUP => self.prune(record, edit),
                _ => Ok(()),
            },
            RESOURCE_MANAGER_STORAGE if operation & RESOURCE_MANAGER_INFO == SMGR_TRUNCATE => {
                self.truncate(record)
            }
            _ => Ok(()),
        }
    }

    fn is_tracked(&self, block: &Block) -> bool {
        block.fork == MAIN_FORK && self.tracked.contains(&block.file)
    }
}

fn is_checkpoint(record: &Record) -> bool {
    if record.resource_manager != RESOURCE_MANAGER_XLOG {
        return false;
    }
    let kind = record.info & RESOURCE_MANAGER_INFO;
    kind == XLOG_CHECKPOINT_SHUTDOWN || kind == XLOG_CHECKPOINT_ONLINE
}

fn page_id(block: &Block) -> PageId {
    PageId {
        file: block.file,
        block: block.number,
    }
}
