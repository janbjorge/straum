//! The heap records that change rows.

use std::ops::Range;

use super::super::{
    Block, Problem, Record, RelationFile, SlotNumber, WalError, read_u16, read_u32,
};
use super::page::{HEADER_HOFF, HEAP_HEADER, TUPLE_HEADER, tuple};
use super::{Heap, HeapChange, HeapTuple, INIT_PAGE, Page, page_id, problem};

pub(super) const SMGR_TRUNCATE_HEAP: u32 = 0x0001;

pub(super) const INSERT_IS_SPECULATIVE: u8 = 0x04;
pub(super) const DELETE_IS_SUPER: u8 = 0x08;
pub(super) const UPDATE_PREFIX_FROM_OLD: u8 = 0x20;
pub(super) const UPDATE_SUFFIX_FROM_OLD: u8 = 0x40;

pub(super) const PRUNE_HAS_FREEZE_PLANS: u8 = 1 << 4;
pub(super) const PRUNE_HAS_REDIRECTIONS: u8 = 1 << 5;
pub(super) const PRUNE_HAS_DEAD_ITEMS: u8 = 1 << 6;
pub(super) const PRUNE_HAS_NOW_UNUSED_ITEMS: u8 = 1 << 7;
pub(super) const FREEZE_PLAN: usize = 12;
/// `xlhp_freeze_plans`: `nplans`, then plans aligned for their xids.
pub(super) const FREEZE_PLANS_HEADER: usize = 4;
/// `xlhp_prune_items`: `ntargets`, then the offset numbers.
pub(super) const PRUNE_ITEMS_COUNT: usize = 2;
/// `OffsetNumber`, a slot on a page.
pub(super) const OFFSET_NUMBER: usize = 2;

/// `xl_heap_insert`: `offnum`, then `flags`.
pub(super) const INSERT_SIZE: usize = 3;
pub(super) const INSERT_OFFNUM: Range<usize> = 0..2;
pub(super) const INSERT_FLAGS: usize = 2;
/// `xl_heap_confirm`: `offnum`.
pub(super) const CONFIRM_SIZE: usize = 2;
pub(super) const CONFIRM_OFFNUM: Range<usize> = 0..2;
/// `xl_heap_delete`: `xmax`, `offnum`, `infobits_set`, `flags`.
pub(super) const DELETE_SIZE: usize = 8;
pub(super) const DELETE_OFFNUM: Range<usize> = 4..6;
pub(super) const DELETE_FLAGS: usize = 7;
/// `xl_heap_update`: `old_xmax`, `old_offnum`, `old_infobits_set`,
/// `flags`, `new_xmax`, `new_offnum`.
pub(super) const UPDATE_SIZE: usize = 14;
pub(super) const UPDATE_OLD_OFFNUM: Range<usize> = 4..6;
pub(super) const UPDATE_FLAGS: usize = 7;
pub(super) const UPDATE_NEW_OFFNUM: Range<usize> = 12..14;
/// The update's prefix and suffix lengths, each a `uint16`.
pub(super) const UPDATE_SHARED_LENGTH: usize = 2;
/// `xl_heap_multi_insert`: `flags`, `ntuples`, then the offsets when the
/// page isn't started afresh.
pub(super) const MULTI_INSERT_SIZE: usize = 4;
pub(super) const MULTI_INSERT_COUNT: Range<usize> = 2..4;
/// `xl_multi_insert_tuple`: `datalen`, then the `xl_heap_header` fields,
/// each at a 2-byte boundary.
pub(super) const MULTI_INSERT_TUPLE: usize = 7;
pub(super) const MULTI_INSERT_TUPLE_LENGTH: Range<usize> = 0..2;
pub(super) const MULTI_INSERT_TUPLE_ALIGNMENT: usize = 2;
/// `xl_smgr_truncate`: `blkno`, `rlocator`, `flags`.
pub(super) const TRUNCATE_SIZE: usize = 20;
pub(super) const TRUNCATE_BLOCK: Range<usize> = 0..4;
pub(super) const TRUNCATE_TABLESPACE: Range<usize> = 4..8;
pub(super) const TRUNCATE_DATABASE: Range<usize> = 8..12;
pub(super) const TRUNCATE_RELATION: Range<usize> = 12..16;
pub(super) const TRUNCATE_FLAGS: Range<usize> = 16..20;

/// One record being applied.
pub(super) struct Edit<'a> {
    pub(super) heap: &'a mut Heap,
    pub(super) record: &'a Record,
    /// Blocks a full-page image already set: the image is the page after
    /// the record, so redo leaves them be.
    pub(super) restored: &'a [u8],
    pub(super) changes: Vec<HeapChange>,
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

    fn row(&mut self, block: &Block, offnum: SlotNumber) -> Result<HeapTuple, WalError> {
        let lsn = self.record.lsn;
        (self.page(block)?.get(offnum).cloned()).ok_or(problem(lsn, Problem::MissingRow))
    }

    fn put(&mut self, block: &Block, offnum: SlotNumber, tuple: HeapTuple) -> Result<(), WalError> {
        let lsn = self.record.lsn;
        (self.page(block)?.set(offnum, tuple)).ok_or(problem(lsn, Problem::RowInSlotZero))
    }

    pub(super) fn insert(&mut self) -> Result<(), WalError> {
        let main = self.main(INSERT_SIZE)?;
        let offnum = read_u16(&main[INSERT_OFFNUM]);
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
    pub(super) fn confirm(&mut self) -> Result<(), WalError> {
        let offnum = read_u16(&self.main(CONFIRM_SIZE)?[CONFIRM_OFFNUM]);
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
    pub(super) fn delete(&mut self) -> Result<(), WalError> {
        let main = self.main(DELETE_SIZE)?;
        let offnum = read_u16(&main[DELETE_OFFNUM]);
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
    pub(super) fn update(&mut self) -> Result<(), WalError> {
        let main = self.main(UPDATE_SIZE)?;
        let old_offnum = read_u16(&main[UPDATE_OLD_OFFNUM]);
        let flags = main[UPDATE_FLAGS];
        let new_offnum = read_u16(&main[UPDATE_NEW_OFFNUM]);
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
    pub(super) fn multi_insert(&mut self) -> Result<(), WalError> {
        let main = self.main(MULTI_INSERT_SIZE)?;
        let count = read_u16(&main[MULTI_INSERT_COUNT]);
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
    pub(super) fn prune(&mut self) -> Result<(), WalError> {
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
    pub(super) fn truncate(&mut self) -> Result<(), WalError> {
        let main = self.main(TRUNCATE_SIZE)?;
        let first_gone = read_u32(&main[TRUNCATE_BLOCK]);
        let file = RelationFile {
            tablespace: read_u32(&main[TRUNCATE_TABLESPACE]),
            database: read_u32(&main[TRUNCATE_DATABASE]),
            relation: read_u32(&main[TRUNCATE_RELATION]),
        };
        if read_u32(&main[TRUNCATE_FLAGS]) & SMGR_TRUNCATE_HEAP != 0 {
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
            gone.extend(list.chunks_exact(item).map(read_u16));
            at = start + list.len();
        }
    }
    Some(gone)
}

/// The `uint16` count at `at`.
fn count_at(data: &[u8], at: usize) -> Option<usize> {
    let count = data.get(at..at + 2)?;
    Some(usize::from(read_u16(count)))
}

/// `xl_heap_header` then the tuple from its null bitmap on.
pub(super) fn header_and_body(data: &[u8]) -> Option<HeapTuple> {
    Some(tuple(data.get(..HEAP_HEADER)?, &data[HEAP_HEADER..]))
}

/// The new row of an update, as `heap_xlog_update` puts it together: the
/// logged bytes, with the prefix and suffix it shares with the old row
/// taken from that.
pub(super) fn updated(data: &[u8], flags: u8, old: &HeapTuple) -> Option<HeapTuple> {
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
pub(super) fn multi_insert_tuples(data: &[u8], count: usize) -> Option<Vec<HeapTuple>> {
    let mut at = 0;
    let mut tuples = Vec::with_capacity(count);
    for _ in 0..count {
        at += at % MULTI_INSERT_TUPLE_ALIGNMENT;
        let header = data.get(at..at + MULTI_INSERT_TUPLE)?;
        let length = usize::from(read_u16(&header[MULTI_INSERT_TUPLE_LENGTH]));
        at += MULTI_INSERT_TUPLE;
        let heap_header = &header[MULTI_INSERT_TUPLE_LENGTH.end..];
        tuples.push(tuple(heap_header, data.get(at..at + length)?));
        at += length;
    }
    (at == data.len()).then_some(tuples)
}
