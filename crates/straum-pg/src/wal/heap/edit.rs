//! The heap records that change rows, redone on the pages Straum keeps:
//! insert, delete, update, confirm of a speculative insert, bulk insert,
//! prune and truncate. Each reads its record's main data, a fixed struct
//! from `src/include/access/heapam_xlog.h` (`xl_heap_insert` and the
//! rest), then its block's data, the logged row, as `heap_xlog_insert`
//! and its siblings in `src/backend/access/heap/heapam_xlog.c` do. What a
//! record does to a row Straum reads becomes a [`HeapChange`].

use std::ops::Range;

use super::super::{
    Block, Problem, Record, RelationFile, SlotNumber, WalError, read_u16, read_u32,
};
use super::page::{HEADER_HOFF, HEAP_HEADER, TUPLE_HEADER, tuple_from_header};
use super::{Heap, HeapChange, HeapTuple, INIT_PAGE, Page, PageId, page_id, problem};

/// `SMGR_TRUNCATE_HEAP` in `xl_smgr_truncate.flags`: the table's own
/// pages were cut, not only its maps.
pub(super) const SMGR_TRUNCATE_HEAP: u32 = 0x0001;

/// `XLH_INSERT_IS_SPECULATIVE`: an `INSERT ... ON CONFLICT` row, not yet
/// known to stand.
pub(super) const INSERT_IS_SPECULATIVE: u8 = 0x04;
/// `XLH_DELETE_IS_SUPER`: the delete takes back a speculative insert.
pub(super) const DELETE_IS_SUPER: u8 = 0x08;
/// `XLH_UPDATE_PREFIX_FROM_OLD`: the new row's first bytes are the old
/// row's, and the record leaves them out.
pub(super) const UPDATE_PREFIX_FROM_OLD: u8 = 0x20;
/// `XLH_UPDATE_SUFFIX_FROM_OLD`: so are its last bytes.
pub(super) const UPDATE_SUFFIX_FROM_OLD: u8 = 0x40;

/// `XLHP_HAS_FREEZE_PLANS`: the prune data opens with freeze plans.
pub(super) const PRUNE_HAS_FREEZE_PLANS: u8 = 1 << 4;
/// `XLHP_HAS_REDIRECTIONS`: a list of redirected slots follows.
pub(super) const PRUNE_HAS_REDIRECTIONS: u8 = 1 << 5;
/// `XLHP_HAS_DEAD_ITEMS`: a list of dead slots follows.
pub(super) const PRUNE_HAS_DEAD_ITEMS: u8 = 1 << 6;
/// `XLHP_HAS_NOW_UNUSED_ITEMS`: a list of now unused slots follows.
pub(super) const PRUNE_HAS_NOW_UNUSED_ITEMS: u8 = 1 << 7;
/// `xlhp_freeze_plan`: `xmax`, `t_infomask2`, `t_infomask`, `frzflags`,
/// `ntuples`.
pub(super) const FREEZE_PLAN: usize = 12;
/// `xlhp_freeze_plans`: `nplans`, then plans aligned for their xids.
pub(super) const FREEZE_PLANS_HEADER: usize = 4;
/// `xlhp_prune_items.ntargets`, before the slots it counts.
pub(super) const PRUNE_ITEMS_COUNT: usize = 2;
/// `OffsetNumber`, a slot on a page.
pub(super) const OFFSET_NUMBER: usize = 2;
/// `xl_heap_prune`: `reason` then `flags`, a byte each, up to 18; from 19
/// `flags` alone, in two bytes.
pub(super) const PRUNE_SIZE: usize = 2;
/// `xl_heap_prune.flags` after `reason`, up to 18.
const PRUNE_FLAGS_AFTER_REASON: usize = 1;
/// `xl_heap_prune.flags` first, from 19; its second byte is for the
/// visibility map.
const PRUNE_FLAGS_FIRST: usize = 0;

/// `xl_heap_insert`: `offnum`, then `flags`.
pub(super) const INSERT_SIZE: usize = 3;
/// `xl_heap_insert.offnum`.
pub(super) const INSERT_SLOT: Range<usize> = 0..2;
/// `xl_heap_insert.flags`.
pub(super) const INSERT_FLAGS: usize = 2;
/// `xl_heap_confirm`: `offnum`.
pub(super) const CONFIRM_SIZE: usize = 2;
/// `xl_heap_confirm.offnum`.
pub(super) const CONFIRM_SLOT: Range<usize> = 0..2;
/// `xl_heap_delete`: `xmax`, `offnum`, `infobits_set`, `flags`.
pub(super) const DELETE_SIZE: usize = 8;
/// `xl_heap_delete.offnum`.
pub(super) const DELETE_SLOT: Range<usize> = 4..6;
/// `xl_heap_delete.flags`.
pub(super) const DELETE_FLAGS: usize = 7;
/// `xl_heap_update`: `old_xmax`, `old_offnum`, `old_infobits_set`,
/// `flags`, `new_xmax`, `new_offnum`.
pub(super) const UPDATE_SIZE: usize = 14;
/// `xl_heap_update.old_offnum`.
pub(super) const UPDATE_OLD_SLOT: Range<usize> = 4..6;
/// `xl_heap_update.flags`.
pub(super) const UPDATE_FLAGS: usize = 7;
/// `xl_heap_update.new_offnum`.
pub(super) const UPDATE_NEW_SLOT: Range<usize> = 12..14;
/// The update's prefix and suffix lengths, each a `uint16`, before its
/// `xl_heap_header`.
pub(super) const UPDATE_SHARED_LENGTH: usize = 2;
/// `xl_heap_multi_insert`: `flags`, `ntuples`, then the offsets when the
/// page isn't started afresh.
pub(super) const MULTI_INSERT_SIZE: usize = 4;
/// `xl_heap_multi_insert.ntuples`.
pub(super) const MULTI_INSERT_COUNT: Range<usize> = 2..4;
/// `xl_multi_insert_tuple`: `datalen`, then the `xl_heap_header` fields,
/// each at a 2-byte boundary.
pub(super) const MULTI_INSERT_TUPLE: usize = 7;
/// `xl_multi_insert_tuple.datalen`.
pub(super) const MULTI_INSERT_TUPLE_LENGTH: Range<usize> = 0..2;
/// `xl_multi_insert_tuple` starts at a 2-byte boundary (`SHORTALIGN`).
pub(super) const MULTI_INSERT_TUPLE_ALIGNMENT: usize = 2;
/// `xl_smgr_truncate`: `blkno`, `rlocator`, `flags`.
pub(super) const TRUNCATE_SIZE: usize = 20;
/// `xl_smgr_truncate.blkno`: the first page cut off.
pub(super) const TRUNCATE_BLOCK: Range<usize> = 0..4;
/// `xl_smgr_truncate.rlocator.spcOid`.
pub(super) const TRUNCATE_TABLESPACE: Range<usize> = 4..8;
/// `xl_smgr_truncate.rlocator.dbOid`.
pub(super) const TRUNCATE_DATABASE: Range<usize> = 8..12;
/// `xl_smgr_truncate.rlocator.relNumber`.
pub(super) const TRUNCATE_RELATION: Range<usize> = 12..16;
/// `xl_smgr_truncate.flags`.
pub(super) const TRUNCATE_FLAGS: Range<usize> = 16..20;

/// The block a heap record changes: for an update, the new row's page.
pub(super) const MAIN_BLOCK: u8 = 0;
/// An update's old row's page, when it is another than the new row's.
const OLD_ROW_BLOCK: u8 = 1;

/// What one record does to the pages: the blocks a full-page image
/// already set, and the row changes so far.
pub(super) struct Edit {
    /// The image is the page after the record, so redo leaves these be.
    pub(super) restored: Vec<u8>,
    /// The changes to rows Straum reads, in record order.
    pub(super) changes: Vec<HeapChange>,
}

impl Edit {
    /// Redo changes the block's page: no image already set it.
    fn redoes(&self, block: &Block) -> bool {
        !self.restored.contains(&block.id)
    }
}

/// Where a prune record keeps its flags, which moved in 19.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PruneLayout {
    /// 17 and 18: a reason byte, then the flags byte.
    ReasonThenFlags,
    /// 19 on: the flags in two bytes, the second for the visibility map.
    FlagsFirst,
}

impl PruneLayout {
    /// The layout PostgreSQL `major` writes.
    pub(super) fn of_major(major: u32) -> PruneLayout {
        if major >= 19 {
            PruneLayout::FlagsFirst
        } else {
            PruneLayout::ReasonThenFlags
        }
    }

    fn flags_at(self) -> usize {
        match self {
            PruneLayout::ReasonThenFlags => PRUNE_FLAGS_AFTER_REASON,
            PruneLayout::FlagsFirst => PRUNE_FLAGS_FIRST,
        }
    }
}

/// The record's main data, if it holds at least `length` bytes.
fn main_data(record: &Record, length: usize) -> Result<&[u8], WalError> {
    let main = record.main_data();
    if main.len() < length {
        return Err(problem(record.lsn, Problem::ShortHeapRecord));
    }
    Ok(main)
}

/// The record's block with this id, if it has one.
fn block_with_id(record: &Record, id: u8) -> Option<&Block> {
    for block in &record.blocks {
        if block.id == id {
            return Some(block);
        }
    }
    None
}

/// The record's main block, which every heap record has.
fn main_block(record: &Record) -> Result<&Block, WalError> {
    let Some(block) = block_with_id(record, MAIN_BLOCK) else {
        return Err(problem(record.lsn, Problem::HeapRecordWithoutPage));
    };
    Ok(block)
}

impl Heap {
    fn page_mut(&mut self, lsn: u64, block: &Block) -> Result<&mut Page, WalError> {
        let Some(page) = self.pages.get_mut(&page_id(block)) else {
            return Err(problem(lsn, Problem::PageWithoutImage));
        };
        Ok(page)
    }

    /// A copy of the row in `slot`; the copy shares the row's bytes.
    fn row(&mut self, lsn: u64, block: &Block, slot: SlotNumber) -> Result<HeapTuple, WalError> {
        let Some(tuple) = self.page_mut(lsn, block)?.get(slot) else {
            return Err(problem(lsn, Problem::MissingRow));
        };
        Ok(tuple.clone())
    }

    fn put(
        &mut self,
        lsn: u64,
        block: &Block,
        slot: SlotNumber,
        tuple: HeapTuple,
    ) -> Result<(), WalError> {
        let page = self.page_mut(lsn, block)?;
        page.set(slot, tuple)
            .ok_or(problem(lsn, Problem::RowInSlotZero))
    }

    pub(super) fn insert(&mut self, record: &Record, edit: &mut Edit) -> Result<(), WalError> {
        let main = main_data(record, INSERT_SIZE)?;
        let slot = read_u16(&main[INSERT_SLOT]);
        let flags = main[INSERT_FLAGS];
        let block = main_block(record)?;
        if !self.is_tracked(block) {
            return Ok(());
        }
        if edit.redoes(block) {
            let Some(tuple) = header_and_body(record.block_data(block)) else {
                return Err(problem(record.lsn, Problem::ShortHeapRecord));
            };
            self.put(record.lsn, block, slot, tuple)?;
        }
        if flags & INSERT_IS_SPECULATIVE == 0 {
            let new = self.row(record.lsn, block, slot)?;
            edit.changes.push(HeapChange::Insert {
                file: block.file,
                new,
            });
        }
        Ok(())
    }

    /// A speculative insert stands: `INSERT ... ON CONFLICT` found no
    /// conflict, and the row it put in counts.
    pub(super) fn confirm(&mut self, record: &Record, edit: &mut Edit) -> Result<(), WalError> {
        let main = main_data(record, CONFIRM_SIZE)?;
        let slot = read_u16(&main[CONFIRM_SLOT]);
        let block = main_block(record)?;
        if !self.is_tracked(block) {
            return Ok(());
        }
        let new = self.row(record.lsn, block, slot)?;
        edit.changes.push(HeapChange::Insert {
            file: block.file,
            new,
        });
        Ok(())
    }

    /// A delete only marks the row, so it stays on the page until a prune.
    pub(super) fn delete(&mut self, record: &Record, edit: &mut Edit) -> Result<(), WalError> {
        let main = main_data(record, DELETE_SIZE)?;
        let slot = read_u16(&main[DELETE_SLOT]);
        let flags = main[DELETE_FLAGS];
        let block = main_block(record)?;
        if !self.is_tracked(block) {
            return Ok(());
        }
        let old = self.row(record.lsn, block, slot)?;
        // A super-delete takes back a speculative insert no one saw.
        if flags & DELETE_IS_SUPER == 0 {
            edit.changes.push(HeapChange::Delete {
                file: block.file,
                old,
            });
        }
        Ok(())
    }

    /// The main block is the new row's page, block 1 the old row's when it
    /// is another.
    pub(super) fn update(&mut self, record: &Record, edit: &mut Edit) -> Result<(), WalError> {
        let main = main_data(record, UPDATE_SIZE)?;
        let old_slot = read_u16(&main[UPDATE_OLD_SLOT]);
        let flags = main[UPDATE_FLAGS];
        let new_slot = read_u16(&main[UPDATE_NEW_SLOT]);
        let new_block = main_block(record)?;
        if !self.is_tracked(new_block) {
            return Ok(());
        }
        let old_block = match block_with_id(record, OLD_ROW_BLOCK) {
            Some(block) => block,
            None => new_block,
        };
        let old = self.row(record.lsn, old_block, old_slot)?;
        if edit.redoes(new_block) {
            let Some(new) = updated(record.block_data(new_block), flags, &old) else {
                return Err(problem(record.lsn, Problem::UpdateMisfit));
            };
            self.put(record.lsn, new_block, new_slot, new)?;
        }
        let new = self.row(record.lsn, new_block, new_slot)?;
        edit.changes.push(HeapChange::Update {
            file: new_block.file,
            old,
            new,
        });
        Ok(())
    }

    /// `COPY` and other bulk inserts: many rows into one page.
    pub(super) fn multi_insert(
        &mut self,
        record: &Record,
        edit: &mut Edit,
    ) -> Result<(), WalError> {
        let main = main_data(record, MULTI_INSERT_SIZE)?;
        let Some(slots) = multi_insert_slots(record, main) else {
            return Err(problem(record.lsn, Problem::ShortHeapRecord));
        };
        let block = main_block(record)?;
        if !self.is_tracked(block) {
            return Ok(());
        }
        if edit.redoes(block) {
            let Some(tuples) = multi_insert_tuples(record.block_data(block), slots.len()) else {
                return Err(problem(record.lsn, Problem::ShortHeapRecord));
            };
            for (slot, tuple) in slots.iter().zip(tuples) {
                self.put(record.lsn, block, *slot, tuple)?;
            }
        }
        for slot in slots {
            let new = self.row(record.lsn, block, slot)?;
            edit.changes.push(HeapChange::Insert {
                file: block.file,
                new,
            });
        }
        Ok(())
    }

    /// Pruning, vacuuming and freezing: the slots whose rows go.
    pub(super) fn prune(&mut self, record: &Record, edit: &Edit) -> Result<(), WalError> {
        let main = main_data(record, PRUNE_SIZE)?;
        let block = main_block(record)?;
        if !self.is_tracked(block) || !edit.redoes(block) {
            return Ok(());
        }
        let flags = main[self.prune_layout.flags_at()];
        let Some(gone) = prune_slots(record.block_data(block), flags) else {
            return Err(problem(record.lsn, Problem::ShortHeapRecord));
        };
        let page = self.page_mut(record.lsn, block)?;
        for slot in gone {
            page.remove(slot);
        }
        Ok(())
    }

    /// `VACUUM` cut empty pages off the end of a relation.
    pub(super) fn truncate(&mut self, record: &Record) -> Result<(), WalError> {
        let main = main_data(record, TRUNCATE_SIZE)?;
        let first_gone = read_u32(&main[TRUNCATE_BLOCK]);
        let file = RelationFile {
            tablespace: read_u32(&main[TRUNCATE_TABLESPACE]),
            database: read_u32(&main[TRUNCATE_DATABASE]),
            relation: read_u32(&main[TRUNCATE_RELATION]),
        };
        if read_u32(&main[TRUNCATE_FLAGS]) & SMGR_TRUNCATE_HEAP == 0 {
            return Ok(());
        }
        let mut gone: Vec<PageId> = Vec::new();
        for id in self.pages.keys() {
            if id.file == file && id.block >= first_gone {
                gone.push(*id);
            }
        }
        self.forget(&gone);
        Ok(())
    }
}

/// The slots a bulk insert fills: 1 on for a page it starts afresh, else
/// the list after `xl_heap_multi_insert`.
fn multi_insert_slots(record: &Record, main: &[u8]) -> Option<Vec<SlotNumber>> {
    let count = read_u16(&main[MULTI_INSERT_COUNT]);
    let mut slots = Vec::with_capacity(usize::from(count));
    if record.info & INIT_PAGE != 0 {
        for slot in 1..=count {
            slots.push(slot);
        }
        return Some(slots);
    }
    let end = MULTI_INSERT_SIZE + OFFSET_NUMBER * usize::from(count);
    let listed = main.get(MULTI_INSERT_SIZE..end)?;
    let (offsets, _) = listed.as_chunks::<OFFSET_NUMBER>();
    for offset in offsets {
        slots.push(u16::from_le_bytes(*offset));
    }
    Some(slots)
}

/// One list a prune record may carry: there if `flag` is set, each item
/// `slots_each` slots long.
struct PruneList {
    flag: u8,
    slots_each: usize,
}

/// The lists after the freeze plans, in order. A redirection is two slots,
/// from and to; Straum keeps the first.
const PRUNE_LISTS: [PruneList; 3] = [
    PruneList {
        flag: PRUNE_HAS_REDIRECTIONS,
        slots_each: 2,
    },
    PruneList {
        flag: PRUNE_HAS_DEAD_ITEMS,
        slots_each: 1,
    },
    PruneList {
        flag: PRUNE_HAS_NOW_UNUSED_ITEMS,
        slots_each: 1,
    },
];

/// The slots a prune record's lists name: freeze plans first, at the
/// alignment of their transaction ids, then redirected, dead and now
/// unused slots, each list there only if its flag says so.
fn prune_slots(data: &[u8], flags: u8) -> Option<Vec<SlotNumber>> {
    let mut at = 0;
    if flags & PRUNE_HAS_FREEZE_PLANS != 0 {
        at += FREEZE_PLANS_HEADER + FREEZE_PLAN * count_at(data, at)?;
    }
    let mut gone = Vec::new();
    for list in &PRUNE_LISTS {
        if flags & list.flag == 0 {
            continue;
        }
        let item = OFFSET_NUMBER * list.slots_each;
        let start = at + PRUNE_ITEMS_COUNT;
        let items = data.get(start..start + item * count_at(data, at)?)?;
        for slots in items.chunks_exact(item) {
            gone.push(read_u16(slots));
        }
        at = start + items.len();
    }
    Some(gone)
}

/// The `uint16` count at `at`.
fn count_at(data: &[u8], at: usize) -> Option<usize> {
    let count = data.get(at..at + 2)?;
    Some(usize::from(read_u16(count)))
}

/// `xl_heap_header` then the row from its null bitmap on.
fn header_and_body(data: &[u8]) -> Option<HeapTuple> {
    let header = data.get(..HEAP_HEADER)?;
    Some(tuple_from_header(header, &data[HEAP_HEADER..]))
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
        ..tuple_from_header(header, &[])
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
        let length = usize::from(read_u16(&header[MULTI_INSERT_TUPLE_LENGTH]));
        at += MULTI_INSERT_TUPLE;
        let heap_header = &header[MULTI_INSERT_TUPLE_LENGTH.end..];
        tuples.push(tuple_from_header(heap_header, data.get(at..at + length)?));
        at += length;
    }
    if at != data.len() {
        return None;
    }
    Some(tuples)
}
