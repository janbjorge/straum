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

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use straum_domain::{FastMap, FastSet};

use super::compress;
use super::{
    BLOCK_SIZE, Block, Compression, Image, Problem, RESOURCE_MANAGER_INFO, Record, RelationFile,
    WalError, le_u16, le_u32,
};

const RM_XLOG: u8 = 0;
const RM_SMGR: u8 = 2;
const RM_HEAP2: u8 = 9;
const RM_HEAP: u8 = 10;

const XLOG_CHECKPOINT_SHUTDOWN: u8 = 0x00;
const XLOG_CHECKPOINT_ONLINE: u8 = 0x10;

const OPERATION: u8 = 0x70;
const INIT_PAGE: u8 = 0x80;

const HEAP_INSERT: u8 = 0x00;
const HEAP_DELETE: u8 = 0x10;
const HEAP_UPDATE: u8 = 0x20;
const HEAP_HOT_UPDATE: u8 = 0x40;
const HEAP_CONFIRM: u8 = 0x50;

/// `PRUNE_ON_ACCESS`, `PRUNE_VACUUM_SCAN` and `PRUNE_VACUUM_CLEANUP`: one
/// layout, prune, vacuum and freeze in one record.
const HEAP2_PRUNE: u8 = 0x10;
const HEAP2_VACUUM: u8 = 0x20;
const HEAP2_PRUNE_CLEANUP: u8 = 0x30;
const HEAP2_MULTI_INSERT: u8 = 0x50;

const SMGR_TRUNCATE: u8 = 0x20;
const SMGR_TRUNCATE_HEAP: u32 = 0x0001;

const INSERT_IS_SPECULATIVE: u8 = 0x04;
const DELETE_IS_SUPER: u8 = 0x08;
const UPDATE_PREFIX_FROM_OLD: u8 = 0x20;
const UPDATE_SUFFIX_FROM_OLD: u8 = 0x40;

const PRUNE_HAS_FREEZE_PLANS: u8 = 1 << 4;
const PRUNE_HAS_REDIRECTIONS: u8 = 1 << 5;
const PRUNE_HAS_DEAD_ITEMS: u8 = 1 << 6;
const PRUNE_HAS_NOW_UNUSED_ITEMS: u8 = 1 << 7;
const FREEZE_PLAN: usize = 12;
/// `xlhp_freeze_plans`: `nplans`, then plans aligned for their xids.
const FREEZE_PLANS_HEADER: usize = 4;
/// `xlhp_prune_items`: `ntargets`, then the offset numbers.
const PRUNE_ITEMS_COUNT: usize = 2;
/// `OffsetNumber`, a slot on a page.
const OFFSET_NUMBER: usize = 2;

/// `xl_heap_insert`: `offnum`, then `flags`.
const INSERT_SIZE: usize = 3;
const INSERT_OFFNUM: Range<usize> = 0..2;
const INSERT_FLAGS: usize = 2;
/// `xl_heap_confirm`: `offnum`.
const CONFIRM_SIZE: usize = 2;
const CONFIRM_OFFNUM: Range<usize> = 0..2;
/// `xl_heap_delete`: `xmax`, `offnum`, `infobits_set`, `flags`.
const DELETE_SIZE: usize = 8;
const DELETE_OFFNUM: Range<usize> = 4..6;
const DELETE_FLAGS: usize = 7;
/// `xl_heap_update`: `old_xmax`, `old_offnum`, `old_infobits_set`,
/// `flags`, `new_xmax`, `new_offnum`.
const UPDATE_SIZE: usize = 14;
const UPDATE_OLD_OFFNUM: Range<usize> = 4..6;
const UPDATE_FLAGS: usize = 7;
const UPDATE_NEW_OFFNUM: Range<usize> = 12..14;
/// The update's prefix and suffix lengths, each a `uint16`.
const UPDATE_SHARED_LENGTH: usize = 2;
/// `xl_heap_multi_insert`: `flags`, `ntuples`, then the offsets when the
/// page isn't started afresh.
const MULTI_INSERT_SIZE: usize = 4;
const MULTI_INSERT_COUNT: Range<usize> = 2..4;
/// `xl_multi_insert_tuple`: `datalen`, then the `xl_heap_header` fields,
/// each at a 2-byte boundary.
const MULTI_INSERT_TUPLE: usize = 7;
const MULTI_INSERT_TUPLE_LENGTH: Range<usize> = 0..2;
const MULTI_INSERT_TUPLE_ALIGNMENT: usize = 2;
/// `xl_smgr_truncate`: `blkno`, `rlocator`, `flags`.
const TRUNCATE_SIZE: usize = 20;
const TRUNCATE_BLOCK: Range<usize> = 0..4;
const TRUNCATE_TABLESPACE: Range<usize> = 4..8;
const TRUNCATE_DATABASE: Range<usize> = 8..12;
const TRUNCATE_RELATION: Range<usize> = 12..16;
const TRUNCATE_FLAGS: Range<usize> = 16..20;

/// `xl_heap_header`: `t_infomask2`, `t_infomask`, `t_hoff`, the fields of
/// a row's header the WAL logs.
const HEAP_HEADER: usize = 5;
const HEADER_INFOMASK2: Range<usize> = 0..2;
const HEADER_INFOMASK: Range<usize> = 2..4;
const HEADER_HOFF: usize = 4;
/// `SizeofHeapTupleHeader`: what comes before the null bitmap.
const TUPLE_HEADER: usize = 23;
/// Where `t_infomask2`, the first field `xl_heap_header` logs, lies in
/// `HeapTupleHeaderData`.
const TUPLE_LOGGED_HEADER: usize = 18;
/// `SizeOfPageHeaderData`, and its `pd_lower`, where the line pointers end.
const PAGE_HEADER: usize = 24;
const PAGE_LOWER: Range<usize> = 12..14;
/// `ItemIdData`: `lp_off` in 15 bits, `lp_flags` in 2, `lp_len` in 15.
const LINE_POINTER: usize = 4;
const LP_OFFSET_MASK: u32 = 0x7FFF;
const LP_FLAGS_SHIFT: u32 = 15;
const LP_FLAGS_MASK: u32 = 0b11;
const LP_LENGTH_SHIFT: u32 = 17;
const LP_NORMAL: u32 = 1;
/// `MAIN_FORKNUM`: the table's rows, not its free space or visibility maps.
const MAIN_FORK: u8 = 0;

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

/// The rows of a page by slot: `tuples[offnum - 1]`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Page {
    tuples: Vec<Option<HeapTuple>>,
    /// The rows' bytes, kept as they change: summing them per record cost
    /// a walk over the page.
    rows: usize,
    /// The end of the last record that changed the page: its page LSN.
    lsn: u64,
    /// What [`Page::footprint`] was when last counted.
    counted: usize,
}

/// What a page costs beyond its rows: its map entry and the page itself.
const PAGE_OVERHEAD: usize = 64 + size_of::<Page>();

impl Page {
    /// The bytes the page holds, near enough to bound memory by.
    fn footprint(&self) -> usize {
        let slots = self.tuples.capacity() * size_of::<Option<HeapTuple>>();
        PAGE_OVERHEAD + slots + self.rows
    }

    pub(crate) fn get(&self, offnum: u16) -> Option<&HeapTuple> {
        self.tuples
            .get(usize::from(offnum).checked_sub(1)?)?
            .as_ref()
    }

    #[cfg(test)]
    pub(crate) fn rows(&self) -> impl Iterator<Item = (u16, &HeapTuple)> {
        (1u16..)
            .zip(&self.tuples)
            .filter_map(|(offnum, tuple)| Some((offnum, tuple.as_ref()?)))
    }

    fn set(&mut self, offnum: u16, tuple: HeapTuple) -> Option<()> {
        let slot = usize::from(offnum).checked_sub(1)?;
        if self.tuples.len() <= slot {
            self.tuples.resize(slot + 1, None);
        }
        self.rows += tuple.body.len();
        if let Some(old) = self.tuples[slot].replace(tuple) {
            self.rows -= old.body.len();
        }
        Some(())
    }

    fn remove(&mut self, offnum: u16) {
        if let Some(slot) = usize::from(offnum).checked_sub(1)
            && let Some(old) = self.tuples.get_mut(slot).and_then(Option::take)
        {
            self.rows -= old.body.len();
        }
    }
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
        if record.resource_manager == RM_XLOG
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
            RM_HEAP => match operation {
                HEAP_INSERT => edit.insert()?,
                HEAP_DELETE => edit.delete()?,
                HEAP_UPDATE | HEAP_HOT_UPDATE => edit.update()?,
                HEAP_CONFIRM => edit.confirm()?,
                _ => {}
            },
            RM_HEAP2 => match operation {
                HEAP2_MULTI_INSERT => edit.multi_insert()?,
                HEAP2_PRUNE | HEAP2_VACUUM | HEAP2_PRUNE_CLEANUP => edit.prune()?,
                _ => {}
            },
            RM_SMGR if operation & RESOURCE_MANAGER_INFO == SMGR_TRUNCATE => edit.truncate()?,
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

/// One record being applied.
struct Edit<'a> {
    heap: &'a mut Heap,
    record: &'a Record,
    /// Blocks a full-page image already set: the image is the page after
    /// the record, so redo leaves them be.
    restored: &'a [u8],
    changes: Vec<HeapChange>,
}

impl<'a> Edit<'a> {
    fn fail(&self, what: Problem) -> WalError {
        problem(self.record.lsn, what)
    }

    fn short(&self) -> WalError {
        self.fail(Problem::ShortHeapRecord)
    }

    fn main(&self, length: usize) -> Result<&'a [u8], WalError> {
        let record: &'a Record = self.record;
        let main = record.main_data();
        if main.len() < length {
            return Err(self.short());
        }
        Ok(main)
    }

    /// The block with this id, if it is of a tracked relation.
    fn block(&self, id: u8) -> Result<Option<&'a Block>, WalError> {
        let record: &'a Record = self.record;
        let block = (record.blocks.iter())
            .find(|block| block.id == id)
            .ok_or_else(|| self.fail(Problem::HeapRecordWithoutPage))?;
        Ok(self.heap.is_tracked(block).then_some(block))
    }

    fn redo(&self, block: &Block) -> bool {
        !self.restored.contains(&block.id)
    }

    fn page(&mut self, block: &Block) -> Result<&mut Page, WalError> {
        let lsn = self.record.lsn;
        (self.heap.pages.get_mut(&page_id(block))).ok_or(problem(lsn, Problem::PageWithoutImage))
    }

    fn row(&mut self, block: &Block, offnum: u16) -> Result<HeapTuple, WalError> {
        let lsn = self.record.lsn;
        (self.page(block)?.get(offnum).cloned()).ok_or(problem(lsn, Problem::MissingRow))
    }

    fn put(&mut self, block: &Block, offnum: u16, tuple: HeapTuple) -> Result<(), WalError> {
        let lsn = self.record.lsn;
        (self.page(block)?.set(offnum, tuple)).ok_or(problem(lsn, Problem::RowInSlotZero))
    }

    fn insert(&mut self) -> Result<(), WalError> {
        let main = self.main(INSERT_SIZE)?;
        let offnum = le_u16(&main[INSERT_OFFNUM]);
        let flags = main[INSERT_FLAGS];
        let Some(block) = self.block(0)? else {
            return Ok(());
        };
        if self.redo(block) {
            let tuple =
                header_and_body(self.record.block_data(block)).ok_or_else(|| self.short())?;
            self.put(block, offnum, tuple)?;
        }
        if flags & INSERT_IS_SPECULATIVE == 0 {
            let new = self.row(block, offnum)?;
            self.changes.push(HeapChange::Insert {
                file: block.file,
                new,
            });
        }
        Ok(())
    }

    /// A speculative insert stands: `INSERT ... ON CONFLICT` found no
    /// conflict, and the row it put in counts.
    fn confirm(&mut self) -> Result<(), WalError> {
        let offnum = le_u16(&self.main(CONFIRM_SIZE)?[CONFIRM_OFFNUM]);
        let Some(block) = self.block(0)? else {
            return Ok(());
        };
        let new = self.row(block, offnum)?;
        self.changes.push(HeapChange::Insert {
            file: block.file,
            new,
        });
        Ok(())
    }

    /// A delete only marks the row, so it stays on the page until a prune.
    fn delete(&mut self) -> Result<(), WalError> {
        let main = self.main(DELETE_SIZE)?;
        let offnum = le_u16(&main[DELETE_OFFNUM]);
        let flags = main[DELETE_FLAGS];
        let Some(block) = self.block(0)? else {
            return Ok(());
        };
        let old = self.row(block, offnum)?;
        // A super-delete takes back a speculative insert no one saw.
        if flags & DELETE_IS_SUPER == 0 {
            self.changes.push(HeapChange::Delete {
                file: block.file,
                old,
            });
        }
        Ok(())
    }

    /// Block 0 is the new row's page, block 1 the old row's when it is
    /// another.
    fn update(&mut self) -> Result<(), WalError> {
        let main = self.main(UPDATE_SIZE)?;
        let old_offnum = le_u16(&main[UPDATE_OLD_OFFNUM]);
        let flags = main[UPDATE_FLAGS];
        let new_offnum = le_u16(&main[UPDATE_NEW_OFFNUM]);
        let Some(new_block) = self.block(0)? else {
            return Ok(());
        };
        let old_block = (self.record.blocks.iter())
            .find(|block| block.id == 1)
            .unwrap_or(new_block);
        let old = self.row(old_block, old_offnum)?;
        if self.redo(new_block) {
            let new = updated(self.record.block_data(new_block), flags, &old)
                .ok_or_else(|| self.fail(Problem::UpdateMisfit))?;
            self.put(new_block, new_offnum, new)?;
        }
        let new = self.row(new_block, new_offnum)?;
        self.changes.push(HeapChange::Update {
            file: new_block.file,
            old,
            new,
        });
        Ok(())
    }

    /// `COPY` and other bulk inserts: many rows into one page.
    fn multi_insert(&mut self) -> Result<(), WalError> {
        let main = self.main(MULTI_INSERT_SIZE)?;
        let count = le_u16(&main[MULTI_INSERT_COUNT]);
        let offnums: Vec<u16> = if self.record.info & INIT_PAGE != 0 {
            (1..=count).collect()
        } else {
            let end = MULTI_INSERT_SIZE + OFFSET_NUMBER * usize::from(count);
            let Some(listed) = main.get(MULTI_INSERT_SIZE..end) else {
                return Err(self.short());
            };
            let (offsets, _) = listed.as_chunks::<OFFSET_NUMBER>();
            offsets
                .iter()
                .map(|offnum| u16::from_le_bytes(*offnum))
                .collect()
        };
        let Some(block) = self.block(0)? else {
            return Ok(());
        };
        if self.redo(block) {
            let tuples = multi_insert_tuples(self.record.block_data(block), offnums.len())
                .ok_or_else(|| self.short())?;
            for (offnum, tuple) in offnums.iter().zip(tuples) {
                self.put(block, *offnum, tuple)?;
            }
        }
        for offnum in offnums {
            let new = self.row(block, offnum)?;
            self.changes.push(HeapChange::Insert {
                file: block.file,
                new,
            });
        }
        Ok(())
    }

    /// Pruning, vacuuming and freezing: the slots whose rows go.
    fn prune(&mut self) -> Result<(), WalError> {
        // The reason and flags bytes, in either order.
        let main = self.main(2)?;
        let Some(block) = self.block(0)? else {
            return Ok(());
        };
        if !self.redo(block) {
            return Ok(());
        }
        let flags = main[usize::from(!self.heap.prune_flags_first)];
        let gone = prune_slots(self.record.block_data(block), flags).ok_or_else(|| self.short())?;
        let page = self.page(block)?;
        for offnum in gone {
            page.remove(offnum);
        }
        Ok(())
    }

    /// `VACUUM` cut empty pages off the end of a relation.
    fn truncate(&mut self) -> Result<(), WalError> {
        let main = self.main(TRUNCATE_SIZE)?;
        let first_gone = le_u32(&main[TRUNCATE_BLOCK]);
        let file = RelationFile {
            tablespace: le_u32(&main[TRUNCATE_TABLESPACE]),
            database: le_u32(&main[TRUNCATE_DATABASE]),
            relation: le_u32(&main[TRUNCATE_RELATION]),
        };
        if le_u32(&main[TRUNCATE_FLAGS]) & SMGR_TRUNCATE_HEAP != 0 {
            (self.heap).forget(|id, _| id.file == file && id.block >= first_gone);
        }
        Ok(())
    }
}

/// The slots a prune record's lists name: freeze plans first, at the
/// alignment of their transaction ids, then redirected, dead and now
/// unused slots, each list there only if its flag says so.
fn prune_slots(data: &[u8], flags: u8) -> Option<Vec<u16>> {
    let mut at = 0;
    if flags & PRUNE_HAS_FREEZE_PLANS != 0 {
        at += FREEZE_PLANS_HEADER + FREEZE_PLAN * count_at(data, at)?;
    }
    let mut gone = Vec::new();
    // A redirection is two slots, from and to; Straum keeps the first.
    for (flag, slots_each) in [
        (PRUNE_HAS_REDIRECTIONS, 2),
        (PRUNE_HAS_DEAD_ITEMS, 1),
        (PRUNE_HAS_NOW_UNUSED_ITEMS, 1),
    ] {
        if flags & flag != 0 {
            let item = OFFSET_NUMBER * slots_each;
            let start = at + PRUNE_ITEMS_COUNT;
            let list = data.get(start..start + item * count_at(data, at)?)?;
            gone.extend(list.chunks_exact(item).map(le_u16));
            at = start + list.len();
        }
    }
    Some(gone)
}

/// The `uint16` count at `at`.
fn count_at(data: &[u8], at: usize) -> Option<usize> {
    let count = data.get(at..at + 2)?;
    Some(usize::from(le_u16(count)))
}

/// `xl_heap_header` then the tuple from its null bitmap on.
fn header_and_body(data: &[u8]) -> Option<HeapTuple> {
    Some(tuple(data.get(..HEAP_HEADER)?, &data[HEAP_HEADER..]))
}

/// A row from the five header bytes the WAL and the page share,
/// `t_infomask2`, `t_infomask` and `t_hoff`, and what follows them.
fn tuple(header: &[u8], body: &[u8]) -> HeapTuple {
    HeapTuple {
        infomask2: le_u16(&header[HEADER_INFOMASK2]),
        infomask: le_u16(&header[HEADER_INFOMASK]),
        hoff: header[HEADER_HOFF],
        body: body.into(),
    }
}

/// The new row of an update, as `heap_xlog_update` puts it together: the
/// logged bytes, with the prefix and suffix it shares with the old row
/// taken from that.
fn updated(data: &[u8], flags: u8, old: &HeapTuple) -> Option<HeapTuple> {
    let mut at = 0;
    let mut prefix = 0;
    if flags & UPDATE_PREFIX_FROM_OLD != 0 {
        prefix = count_at(data, at)?;
        at += UPDATE_SHARED_LENGTH;
    }
    let mut suffix = 0;
    if flags & UPDATE_SUFFIX_FROM_OLD != 0 {
        suffix = count_at(data, at)?;
        at += UPDATE_SHARED_LENGTH;
    }
    let header = data.get(at..at + HEAP_HEADER)?;
    let hoff = header[HEADER_HOFF];
    let logged = &data[at + HEAP_HEADER..];
    let mut body = Vec::with_capacity(logged.len() + prefix + suffix);
    if prefix > 0 {
        let bitmap = usize::from(hoff).checked_sub(TUPLE_HEADER)?;
        body.extend_from_slice(logged.get(..bitmap)?);
        body.extend_from_slice(old.columns()?.get(..prefix)?);
        body.extend_from_slice(&logged[bitmap..]);
    } else {
        body.extend_from_slice(logged);
    }
    if suffix > 0 {
        let from = old.body.len().checked_sub(suffix)?;
        body.extend_from_slice(&old.body[from..]);
    }
    Some(HeapTuple {
        body: body.into(),
        ..tuple(header, &[])
    })
}

/// `count` rows, each an `xl_multi_insert_tuple` at a 2-byte boundary
/// and then its bytes from the null bitmap on.
fn multi_insert_tuples(data: &[u8], count: usize) -> Option<Vec<HeapTuple>> {
    let mut at = 0;
    let mut tuples = Vec::with_capacity(count);
    for _ in 0..count {
        at += at % MULTI_INSERT_TUPLE_ALIGNMENT;
        let header = data.get(at..at + MULTI_INSERT_TUPLE)?;
        let length = usize::from(le_u16(&header[MULTI_INSERT_TUPLE_LENGTH]));
        at += MULTI_INSERT_TUPLE;
        let heap_header = &header[MULTI_INSERT_TUPLE_LENGTH.end..];
        tuples.push(tuple(heap_header, data.get(at..at + length)?));
        at += length;
    }
    (at == data.len()).then_some(tuples)
}

/// The page an image holds: decompressed, with its hole put back.
fn restore(record: &Record, image: &Image) -> Result<Vec<u8>, WalError> {
    let lsn = record.lsn;
    let stored = record.image_bytes(image);
    let size = BLOCK_SIZE - usize::from(image.hole_length);
    let raw = match image.compression {
        None => Some(stored.to_vec()),
        Some(Compression::Pglz) => compress::pglz(stored, size),
        Some(Compression::Lz4) => compress::lz4(stored, size),
        Some(Compression::Zstd) => {
            return Err(problem(lsn, Problem::ImageInZstd));
        }
    };
    let hole = usize::from(image.hole_offset);
    let raw = raw
        .filter(|raw| raw.len() == size && hole <= size)
        .ok_or(problem(lsn, Problem::ImageNotDecompressing))?;
    let mut page = Vec::with_capacity(BLOCK_SIZE);
    page.extend_from_slice(&raw[..hole]);
    page.resize(hole + usize::from(image.hole_length), 0);
    page.extend_from_slice(&raw[hole..]);
    Ok(page)
}

/// The rows of a heap page: each normal line pointer's tuple.
fn page_from_image(bytes: &[u8]) -> Option<Page> {
    let lower = usize::from(le_u16(bytes.get(PAGE_LOWER)?));
    let mut page = Page::default();
    // A new page is all zeros.
    if lower == 0 {
        return Some(page);
    }
    let pointers = bytes.get(PAGE_HEADER..lower)?;
    for (offnum, pointer) in (1u16..).zip(pointers.as_chunks::<LINE_POINTER>().0) {
        let pointer = u32::from_le_bytes(*pointer);
        if (pointer >> LP_FLAGS_SHIFT) & LP_FLAGS_MASK != LP_NORMAL {
            continue;
        }
        let offset = (pointer & LP_OFFSET_MASK) as usize;
        let length = (pointer >> LP_LENGTH_SHIFT) as usize;
        let row = bytes.get(offset..offset.checked_add(length)?)?;
        if length < TUPLE_HEADER {
            return None;
        }
        let header = &row[TUPLE_LOGGED_HEADER..TUPLE_HEADER];
        page.set(offnum, tuple(header, &row[TUPLE_HEADER..]))?;
    }
    Some(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::test_support::decode;
    use crate::wal::test_support::{BlockSpec, FILE, ImageSpec, Spec, assemble};

    const OTHER: RelationFile = RelationFile {
        tablespace: 1663,
        database: 5,
        relation: 16_999,
    };

    fn tuple(columns: &[u8]) -> HeapTuple {
        HeapTuple {
            infomask2: 2,
            infomask: 0,
            hoff: 24,
            body: [&[0][..], columns].concat().into(),
        }
    }

    /// A heap page holding `rows` in its slots, as PostgreSQL lays it out.
    fn page_bytes(rows: &[(u16, &HeapTuple)]) -> Vec<u8> {
        let mut page = vec![0; BLOCK_SIZE];
        let slots = rows.iter().map(|(offnum, _)| *offnum).max().unwrap_or(0);
        let lower = PAGE_HEADER + LINE_POINTER * usize::from(slots);
        page[PAGE_LOWER].copy_from_slice(&u16::try_from(lower).unwrap().to_le_bytes());
        let mut upper = BLOCK_SIZE;
        for (offnum, tuple) in rows {
            let length = TUPLE_HEADER + tuple.body.len();
            upper -= length;
            let header = upper + TUPLE_LOGGED_HEADER;
            page[header..header + 2].copy_from_slice(&tuple.infomask2.to_le_bytes());
            page[header + 2..header + 4].copy_from_slice(&tuple.infomask.to_le_bytes());
            page[header + 4] = tuple.hoff;
            page[upper + TUPLE_HEADER..upper + length].copy_from_slice(&tuple.body);
            let at = PAGE_HEADER + LINE_POINTER * usize::from(offnum - 1);
            page[at..at + LINE_POINTER].copy_from_slice(&line_pointer(upper, length));
        }
        page
    }

    /// A normal `ItemIdData` for the row at `offset`.
    fn line_pointer(offset: usize, length: usize) -> [u8; LINE_POINTER] {
        let offset = u32::try_from(offset).unwrap();
        let length = u32::try_from(length).unwrap();
        (offset | LP_NORMAL << LP_FLAGS_SHIFT | length << LP_LENGTH_SHIFT).to_le_bytes()
    }

    fn image(bytes: Vec<u8>) -> ImageSpec {
        ImageSpec {
            hole_offset: 0,
            hole_length: 0,
            compression: None,
            apply: true,
            bytes,
        }
    }

    fn block(id: u8, number: u32, data: Vec<u8>, image: Option<ImageSpec>) -> BlockSpec {
        BlockSpec {
            id,
            fork: 0,
            will_init: false,
            file: FILE,
            number,
            image,
            data,
        }
    }

    fn record(resource_manager: u8, info: u8, main: Vec<u8>, blocks: Vec<BlockSpec>) -> Record {
        let spec = Spec {
            xid: 700,
            resource_manager,
            info,
            toplevel_xid: None,
            origin: false,
            blocks,
            main,
        };
        let bytes = assemble(&spec);
        decode(0x100_0028, 0x100_0028 + bytes.len() as u64, bytes).unwrap()
    }

    /// `xl_heap_header` and the tuple from its null bitmap on.
    fn logged(tuple: &HeapTuple) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(tuple.infomask2.to_le_bytes());
        data.extend(tuple.infomask.to_le_bytes());
        data.push(tuple.hoff);
        data.extend_from_slice(&tuple.body);
        data
    }

    fn insert_main(offnum: u16, flags: u8) -> Vec<u8> {
        [&offnum.to_le_bytes()[..], &[flags]].concat()
    }

    fn delete_main(offnum: u16, flags: u8) -> Vec<u8> {
        [&[0; 4][..], &offnum.to_le_bytes(), &[0, flags]].concat()
    }

    fn update_main(old: u16, flags: u8, new: u16) -> Vec<u8> {
        [
            &[0; 4][..],
            &old.to_le_bytes(),
            &[0, flags],
            &[0; 4],
            &new.to_le_bytes(),
        ]
        .concat()
    }

    fn heap(major: u32) -> Heap {
        Heap::new([FILE].into(), major)
    }

    fn at(heap: &Heap, block: u32) -> &Page {
        heap.page(PageId { file: FILE, block }).unwrap()
    }

    fn insert(file: RelationFile, new: HeapTuple) -> HeapChange {
        HeapChange::Insert { file, new }
    }

    fn checkpoint(redo: u64) -> Record {
        let mut main = redo.to_le_bytes().to_vec();
        main.extend([0; 80]);
        record(RM_XLOG, XLOG_CHECKPOINT_ONLINE, main, Vec::new())
    }

    /// A checkpoint lets go of the pages last changed by its redo point,
    /// and keeps those changed after it, whose next change brings no
    /// image.
    #[test]
    fn a_checkpoint_lets_go_of_pages_changed_before_its_redo() {
        let mut heap = heap(18);
        let row = tuple(b"ab");
        for number in 0..3 {
            let init = BlockSpec {
                will_init: true,
                ..block(0, number, logged(&row), None)
            };
            let main = [&1u16.to_le_bytes()[..], &[0]].concat();
            heap.apply(&record(RM_HEAP, HEAP_INSERT | INIT_PAGE, main, vec![init]))
                .unwrap();
        }
        // Every test record has one LSN; a page's is where its record ends.
        let end = at(&heap, 0).lsn;
        assert!(end > 0x100_0028);
        assert!(heap.bytes() > 0);
        assert_eq!(heap.bytes(), heap.recount());
        heap.apply(&checkpoint(end - 8)).unwrap();
        assert_eq!(heap.pages().count(), 3);
        // A page whose LSN is the redo point goes: its next change is
        // logged whole.
        heap.apply(&checkpoint(end)).unwrap();
        assert_eq!((heap.pages().count(), heap.bytes()), (0, 0));
        let cut = record(RM_XLOG, XLOG_CHECKPOINT_SHUTDOWN, vec![0; 4], Vec::new());
        assert!(heap.apply(&cut).is_err());
    }

    #[test]
    fn follows_rows_through_inserts_updates_and_deletes() {
        let mut heap = heap(18);
        let first = tuple(b"first row, long enough to share");
        let record_with_image = record(
            RM_HEAP,
            HEAP_INSERT,
            insert_main(1, 0),
            vec![block(0, 0, vec![], Some(image(page_bytes(&[(1, &first)]))))],
        );
        assert_eq!(
            heap.apply(&record_with_image),
            Ok(vec![insert(FILE, first.clone())])
        );

        let second = tuple(b"second");
        let logged_insert = record(
            RM_HEAP,
            HEAP_INSERT,
            insert_main(2, 0),
            vec![block(0, 0, logged(&second), None)],
        );
        assert_eq!(
            heap.apply(&logged_insert),
            Ok(vec![insert(FILE, second.clone())])
        );

        // "first row," + "FIRST" + " long enough to share": a prefix of 10
        // and a suffix of 21 come from the old row.
        let new = tuple(b"first row,FIRST long enough to share");
        let mut data = Vec::new();
        data.extend(10u16.to_le_bytes());
        data.extend(21u16.to_le_bytes());
        data.extend(new.infomask2.to_le_bytes());
        data.extend(new.infomask.to_le_bytes());
        data.push(new.hoff);
        data.push(0);
        data.extend(b"FIRST");
        let update = record(
            RM_HEAP,
            HEAP_HOT_UPDATE,
            update_main(1, UPDATE_PREFIX_FROM_OLD | UPDATE_SUFFIX_FROM_OLD, 3),
            vec![block(0, 0, data, None)],
        );
        assert_eq!(
            heap.apply(&update),
            Ok(vec![HeapChange::Update {
                file: FILE,
                old: first.clone(),
                new: new.clone(),
            }])
        );

        let delete = record(
            RM_HEAP,
            HEAP_DELETE,
            delete_main(2, 0),
            vec![block(0, 0, vec![], None)],
        );
        assert_eq!(
            heap.apply(&delete),
            Ok(vec![HeapChange::Delete {
                file: FILE,
                old: second.clone(),
            }])
        );
        let rows: Vec<(u16, &HeapTuple)> = at(&heap, 0).rows().collect();
        assert_eq!(rows, vec![(1, &first), (2, &second), (3, &new)]);
    }

    #[test]
    fn moves_a_row_to_another_page() {
        let mut heap = heap(18);
        let old = tuple(b"old");
        let new = tuple(b"new and longer");
        let update = record(
            RM_HEAP,
            HEAP_UPDATE | INIT_PAGE,
            update_main(1, 0, 1),
            vec![
                BlockSpec {
                    will_init: true,
                    ..block(0, 7, logged(&new), None)
                },
                block(1, 3, vec![], Some(image(page_bytes(&[(1, &old)])))),
            ],
        );
        assert_eq!(
            heap.apply(&update),
            Ok(vec![HeapChange::Update {
                file: FILE,
                old,
                new: new.clone(),
            }])
        );
        assert_eq!(at(&heap, 7).get(1), Some(&new));
    }

    #[test]
    fn reads_a_bulk_insert() {
        let mut heap = heap(18);
        let rows = [tuple(b"a"), tuple(b"bb"), tuple(b"ccc")];
        let mut data = Vec::new();
        for row in &rows {
            if data.len() % 2 == 1 {
                data.push(0xEE);
            }
            data.extend(u16::try_from(row.body.len()).unwrap().to_le_bytes());
            data.extend(row.infomask2.to_le_bytes());
            data.extend(row.infomask.to_le_bytes());
            data.push(row.hoff);
            data.extend_from_slice(&row.body);
        }
        let main = [&[0, 0][..], &3u16.to_le_bytes()].concat();
        let fresh = record(
            RM_HEAP2,
            HEAP2_MULTI_INSERT | INIT_PAGE,
            main.clone(),
            vec![BlockSpec {
                will_init: true,
                ..block(0, 0, data.clone(), None)
            }],
        );
        let inserts: Vec<HeapChange> = rows.iter().map(|row| insert(FILE, row.clone())).collect();
        assert_eq!(heap.apply(&fresh), Ok(inserts.clone()));
        // Into slots 4 to 6 of the same page, listed.
        let listed = [
            main,
            4u16.to_le_bytes().to_vec(),
            5u16.to_le_bytes().to_vec(),
            6u16.to_le_bytes().to_vec(),
        ]
        .concat();
        let more = record(
            RM_HEAP2,
            HEAP2_MULTI_INSERT,
            listed,
            vec![block(0, 0, data, None)],
        );
        assert_eq!(heap.apply(&more), Ok(inserts));
        assert_eq!(at(&heap, 0).rows().count(), 6);
    }

    fn init(id: u8, number: u32, data: Vec<u8>) -> BlockSpec {
        BlockSpec {
            will_init: true,
            ..block(id, number, data, None)
        }
    }

    #[test]
    fn counts_a_speculative_insert_once_confirmed() {
        let mut heap = heap(18);
        let row = tuple(b"maybe");
        let speculative = record(
            RM_HEAP,
            HEAP_INSERT | INIT_PAGE,
            insert_main(1, INSERT_IS_SPECULATIVE),
            vec![init(0, 0, logged(&row))],
        );
        assert_eq!(heap.apply(&speculative), Ok(vec![]));
        let confirm = record(
            RM_HEAP,
            HEAP_CONFIRM,
            1u16.to_le_bytes().to_vec(),
            vec![block(0, 0, vec![], None)],
        );
        assert_eq!(heap.apply(&confirm), Ok(vec![insert(FILE, row.clone())]));
        let taken_back = record(
            RM_HEAP,
            HEAP_DELETE,
            delete_main(1, DELETE_IS_SUPER),
            vec![block(0, 0, vec![], None)],
        );
        assert_eq!(heap.apply(&taken_back), Ok(vec![]));
    }

    /// Five rows on page 0, slots 1 to 5.
    fn five_rows(heap: &mut Heap) {
        let rows: Vec<HeapTuple> = (0..5u8).map(|i| tuple(&[i])).collect();
        let page: Vec<(u16, &HeapTuple)> = (1..).zip(&rows).collect();
        let fpi = record(
            RM_XLOG,
            0xB0,
            vec![],
            vec![block(0, 0, vec![], Some(image(page_bytes(&page))))],
        );
        assert_eq!(heap.apply(&fpi), Ok(vec![]));
    }

    fn offnums(list: &[u16]) -> Vec<u8> {
        list.iter().flat_map(|n| n.to_le_bytes()).collect()
    }

    fn slots(heap: &Heap) -> Vec<u16> {
        at(heap, 0).rows().map(|(offnum, _)| offnum).collect()
    }

    /// Two freeze plans, then redirected 1 to 2, 3 dead, 4 unused, then
    /// the frozen slots.
    fn prune_freeze_data() -> Vec<u8> {
        let mut data = vec![2, 0, 0, 0];
        data.extend([0x55; 2 * FREEZE_PLAN]);
        for list in [&[1u16, 1, 2][..], &[1, 3], &[1, 4]] {
            data.extend(offnums(list));
        }
        data.extend(offnums(&[5, 5]));
        data
    }

    const PRUNE_HAS_CONFLICT_HORIZON: u8 = 1 << 3;
    const PRUNE_FLAGS: u8 = PRUNE_HAS_CONFLICT_HORIZON
        | PRUNE_HAS_FREEZE_PLANS
        | PRUNE_HAS_REDIRECTIONS
        | PRUNE_HAS_DEAD_ITEMS
        | PRUNE_HAS_NOW_UNUSED_ITEMS;

    #[test]
    fn prunes_as_17_and_19_log_it() {
        for (major, main) in [
            (17, vec![1, PRUNE_FLAGS, 9, 9, 9, 9]),
            (19, vec![PRUNE_FLAGS, 0x01, 9, 9, 9, 9]),
        ] {
            let mut heap = heap(major);
            five_rows(&mut heap);
            for info in [HEAP2_PRUNE, HEAP2_VACUUM, HEAP2_PRUNE_CLEANUP] {
                let prune = record(
                    RM_HEAP2,
                    info,
                    main.clone(),
                    vec![block(0, 0, prune_freeze_data(), None)],
                );
                assert_eq!(heap.apply(&prune), Ok(vec![]));
                assert_eq!(slots(&heap), vec![2, 5], "{major}");
            }
        }
        // Without lists, nothing goes.
        let mut heap = heap(17);
        five_rows(&mut heap);
        let nothing = record(
            RM_HEAP2,
            HEAP2_PRUNE,
            vec![1, 0],
            vec![block(0, 0, vec![], None)],
        );
        assert_eq!(heap.apply(&nothing), Ok(vec![]));
        assert_eq!(slots(&heap), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn drops_pages_a_truncate_cuts_off() {
        let mut heap = heap(18);
        for number in 0..4 {
            let fpi = record(
                RM_XLOG,
                0xB0,
                vec![],
                vec![block(0, number, vec![], Some(image(page_bytes(&[]))))],
            );
            heap.apply(&fpi).unwrap();
        }
        let mut main = 2u32.to_le_bytes().to_vec();
        for part in [
            FILE.tablespace,
            FILE.database,
            FILE.relation,
            SMGR_TRUNCATE_HEAP,
        ] {
            main.extend(part.to_le_bytes());
        }
        heap.apply(&record(RM_SMGR, SMGR_TRUNCATE, main, vec![]))
            .unwrap();
        let mut kept: Vec<u32> = heap.pages().map(|(id, _)| id.block).collect();
        kept.sort_unstable();
        assert_eq!(kept, vec![0, 1]);
    }

    #[test]
    fn leaves_other_relations_and_forks_alone() {
        let mut heap = heap(18);
        let row = tuple(b"x");
        let other = BlockSpec {
            file: OTHER,
            ..block(0, 0, logged(&row), None)
        };
        let insert = record(RM_HEAP, HEAP_INSERT, insert_main(1, 0), vec![other]);
        assert_eq!(heap.apply(&insert), Ok(vec![]));
        let free_space = BlockSpec {
            fork: 1,
            ..block(0, 0, vec![], Some(image(page_bytes(&[(1, &row)]))))
        };
        heap.apply(&record(RM_XLOG, 0xB0, vec![], vec![free_space]))
            .unwrap();
        assert_eq!(heap.pages().count(), 0);
    }

    #[test]
    fn restores_images_with_holes_and_compression() {
        let row = tuple(b"compressed");
        let page = page_bytes(&[(1, &row)]);
        // The hole is the empty middle of the page.
        let (hole_offset, hole_length) = (28u16, 8000u16);
        let kept = [&page[..28], &page[8028..]].concat();
        let pglz: Vec<u8> = kept
            .chunks(8)
            .flat_map(|chunk| [&[0u8][..], chunk].concat())
            .collect();
        let lz4 = [
            &[0xF0u8][..],
            &[255; 0],
            &[u8::try_from(kept.len() - 15).unwrap()],
            &kept,
        ]
        .concat();
        for (compression, bytes) in [
            (Some(Compression::Pglz), pglz),
            (Some(Compression::Lz4), lz4),
        ] {
            let mut heap = heap(18);
            let image = ImageSpec {
                hole_offset,
                hole_length,
                compression,
                apply: true,
                bytes,
            };
            let fpi = record(
                RM_XLOG,
                0xB0,
                vec![],
                vec![block(0, 0, vec![], Some(image))],
            );
            assert_eq!(heap.apply(&fpi), Ok(vec![]), "{compression:?}");
            assert_eq!(at(&heap, 0).get(1), Some(&row), "{compression:?}");
        }
    }

    fn error(heap: &mut Heap, record: &Record) -> Problem {
        match heap.apply(record) {
            Err(WalError::At { problem, .. }) => problem,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn refuses_changes_to_what_it_lacks() {
        let mut heap = heap(18);
        let row = tuple(b"x");
        let no_page = record(
            RM_HEAP,
            HEAP_INSERT,
            insert_main(1, 0),
            vec![block(0, 0, logged(&row), None)],
        );
        assert_eq!(error(&mut heap, &no_page), Problem::PageWithoutImage);
        five_rows(&mut heap);
        let no_row = record(
            RM_HEAP,
            HEAP_DELETE,
            delete_main(9, 0),
            vec![block(0, 0, vec![], None)],
        );
        assert_eq!(error(&mut heap, &no_row), Problem::MissingRow);
        let slot_zero = record(
            RM_HEAP,
            HEAP_INSERT,
            insert_main(0, 0),
            vec![block(0, 0, logged(&row), None)],
        );
        assert_eq!(error(&mut heap, &slot_zero), Problem::RowInSlotZero);
    }

    #[test]
    fn refuses_heap_records_cut_short() {
        let mut heap = heap(18);
        five_rows(&mut heap);
        let row = tuple(b"x");
        let short = record(
            RM_HEAP,
            HEAP_UPDATE,
            vec![0; 13],
            vec![block(0, 0, vec![], None)],
        );
        assert_eq!(error(&mut heap, &short), Problem::ShortHeapRecord);
        let headless = record(
            RM_HEAP,
            HEAP_INSERT,
            insert_main(6, 0),
            vec![block(0, 0, vec![0; 4], None)],
        );
        assert_eq!(error(&mut heap, &headless), Problem::ShortHeapRecord);
        let no_block = record(RM_HEAP, HEAP_INSERT, insert_main(6, 0), vec![]);
        assert_eq!(error(&mut heap, &no_block), Problem::HeapRecordWithoutPage);
        let mut too_long = 200u16.to_le_bytes().to_vec();
        too_long.extend(logged(&row));
        let misfit = record(
            RM_HEAP,
            HEAP_UPDATE,
            update_main(1, UPDATE_PREFIX_FROM_OLD, 6),
            vec![block(0, 0, too_long, None)],
        );
        assert_eq!(error(&mut heap, &misfit), Problem::UpdateMisfit);
        let trailing = record(
            RM_HEAP2,
            HEAP2_MULTI_INSERT,
            [&[0, 0][..], &1u16.to_le_bytes(), &7u16.to_le_bytes()].concat(),
            vec![block(0, 0, vec![1, 0, 2, 0, 0, 0, 24, 0, 9], None)],
        );
        assert_eq!(error(&mut heap, &trailing), Problem::ShortHeapRecord);
        let broken_prune = record(
            RM_HEAP2,
            HEAP2_PRUNE,
            vec![1, PRUNE_HAS_DEAD_ITEMS],
            vec![block(0, 0, vec![3, 0, 1, 0], None)],
        );
        assert_eq!(error(&mut heap, &broken_prune), Problem::ShortHeapRecord);
    }

    #[test]
    fn refuses_page_images_it_can_not_read() {
        let mut heap = heap(18);
        five_rows(&mut heap);
        let page = page_bytes(&[(1, &tuple(b"x"))]);
        let zstd = ImageSpec {
            compression: Some(Compression::Zstd),
            ..image(page[..100].to_vec())
        };
        let zstd = record(RM_XLOG, 0xB0, vec![], vec![block(0, 0, vec![], Some(zstd))]);
        assert_eq!(error(&mut heap, &zstd), Problem::ImageInZstd);
        let garbage = ImageSpec {
            compression: Some(Compression::Pglz),
            ..image(vec![0xFF, 1, 2])
        };
        let garbage = record(
            RM_XLOG,
            0xB0,
            vec![],
            vec![block(0, 0, vec![], Some(garbage))],
        );
        assert_eq!(error(&mut heap, &garbage), Problem::ImageNotDecompressing);
        let mut bad_pointer = page.clone();
        bad_pointer[PAGE_HEADER..PAGE_HEADER + LINE_POINTER]
            .copy_from_slice(&line_pointer(8190, 100));
        let bad = record(
            RM_XLOG,
            0xB0,
            vec![],
            vec![block(0, 0, vec![], Some(image(bad_pointer)))],
        );
        assert_eq!(error(&mut heap, &bad), Problem::UnreadableImage);
        let mut tiny = page;
        tiny[PAGE_HEADER..PAGE_HEADER + LINE_POINTER].copy_from_slice(&line_pointer(100, 10));
        let tiny = record(
            RM_XLOG,
            0xB0,
            vec![],
            vec![block(0, 0, vec![], Some(image(tiny)))],
        );
        assert_eq!(error(&mut heap, &tiny), Problem::UnreadableImage);
    }

    #[test]
    fn says_what_went_wrong() {
        let error = problem(0x1_0000_0028, Problem::RowInSlotZero);
        assert_eq!(error.to_string(), "WAL at 1/28 has a row in slot 0");
    }
}
