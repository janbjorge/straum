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

use std::collections::{HashMap, HashSet};

use super::compress;
use super::{
    BLOCK_SIZE, Block, Compression, Image, Record, RelationFile, WalError, le_u16, le_u32,
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

/// `SizeofHeapTupleHeader`: what comes before the null bitmap.
const TUPLE_HEADER: usize = 23;
const PAGE_HEADER: usize = 24;
const LINE_POINTER: usize = 4;
const LP_NORMAL: u32 = 1;

/// A row as it is on its page, without the fixed header, whose
/// transaction fields Straum does not need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tuple {
    pub(crate) infomask2: u16,
    pub(crate) infomask: u16,
    /// Where the columns start, counted from the start of the header.
    pub(crate) hoff: u8,
    /// The null bitmap, its padding, then the columns.
    pub(crate) body: Vec<u8>,
}

impl Tuple {
    /// The columns, after the null bitmap and its padding.
    pub(crate) fn columns(&self) -> Option<&[u8]> {
        self.body
            .get(usize::from(self.hoff).checked_sub(TUPLE_HEADER)?..)
    }
}

/// A change to a row of a table Straum reads, in WAL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    Insert {
        file: RelationFile,
        new: Tuple,
    },
    Update {
        file: RelationFile,
        old: Tuple,
        new: Tuple,
    },
    Delete {
        file: RelationFile,
        old: Tuple,
    },
}

impl Change {
    /// The bytes the change holds, near enough to bound memory by.
    pub(crate) fn footprint(&self) -> usize {
        let rows = match self {
            Change::Insert { new, .. } => new.body.capacity(),
            Change::Update { old, new, .. } => old.body.capacity() + new.body.capacity(),
            Change::Delete { old, .. } => old.body.capacity(),
        };
        size_of::<Change>() + rows
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
    tuples: Vec<Option<Tuple>>,
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
        let slots = self.tuples.capacity() * size_of::<Option<Tuple>>();
        let rows: usize = (self.tuples.iter().flatten())
            .map(|t| t.body.capacity())
            .sum();
        PAGE_OVERHEAD + slots + rows
    }

    pub(crate) fn get(&self, offnum: u16) -> Option<&Tuple> {
        self.tuples
            .get(usize::from(offnum).checked_sub(1)?)?
            .as_ref()
    }

    #[cfg(test)]
    pub(crate) fn rows(&self) -> impl Iterator<Item = (u16, &Tuple)> {
        (1u16..)
            .zip(&self.tuples)
            .filter_map(|(offnum, tuple)| Some((offnum, tuple.as_ref()?)))
    }

    fn set(&mut self, offnum: u16, tuple: Tuple) -> Option<()> {
        let slot = usize::from(offnum).checked_sub(1)?;
        if self.tuples.len() <= slot {
            self.tuples.resize(slot + 1, None);
        }
        self.tuples[slot] = Some(tuple);
        Some(())
    }

    fn remove(&mut self, offnum: u16) {
        if let Some(slot) = usize::from(offnum).checked_sub(1)
            && let Some(tuple) = self.tuples.get_mut(slot)
        {
            *tuple = None;
        }
    }
}

/// The pages of the tracked relations, and how to read prune records.
pub(crate) struct Heap {
    tracked: HashSet<RelationFile>,
    pages: HashMap<PageId, Page>,
    /// The pages' footprints, summed.
    bytes: usize,
    /// 19 drops the reason byte before the prune flags and widens them to
    /// two bytes, the second for the visibility map.
    prune_flags_first: bool,
}

impl Heap {
    pub(crate) fn new(tracked: HashSet<RelationFile>, major: u32) -> Heap {
        Heap {
            tracked,
            pages: HashMap::new(),
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
        self.pages.values().map(Page::footprint).sum()
    }

    /// Applies the record to the tracked pages, and returns the changes it
    /// makes to their rows.
    pub(crate) fn apply(&mut self, record: &Record) -> Result<Vec<Change>, WalError> {
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
                        .ok_or(problem(lsn, "a page image Straum can't read"))?;
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
            && let XLOG_CHECKPOINT_SHUTDOWN | XLOG_CHECKPOINT_ONLINE = record.info & 0xF0
        {
            let redo = record
                .main_data()
                .get(..8)
                .ok_or(problem(lsn, "a checkpoint without its redo point"))?;
            self.evict(u64::from_le_bytes(redo.try_into().unwrap_or_default()));
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

    fn edit(&mut self, record: &Record, restored: &[u8]) -> Result<Vec<Change>, WalError> {
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
            RM_SMGR if operation & 0xF0 == SMGR_TRUNCATE => edit.truncate()?,
            _ => {}
        }
        Ok(edit.changes)
    }

    fn is_tracked(&self, block: &Block) -> bool {
        block.fork == 0 && self.tracked.contains(&block.file)
    }
}

fn page_id(block: &Block) -> PageId {
    PageId {
        file: block.file,
        block: block.number,
    }
}

fn problem(lsn: u64, problem: &'static str) -> WalError {
    WalError::At { lsn, problem }
}

/// One record being applied.
struct Edit<'a> {
    heap: &'a mut Heap,
    record: &'a Record,
    /// Blocks a full-page image already set: the image is the page after
    /// the record, so redo leaves them be.
    restored: &'a [u8],
    changes: Vec<Change>,
}

impl<'a> Edit<'a> {
    fn fail(&self, what: &'static str) -> WalError {
        problem(self.record.lsn, what)
    }

    fn short(&self) -> WalError {
        self.fail("a heap record shorter than its kind")
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
            .ok_or_else(|| self.fail("a heap record without its page"))?;
        Ok(self.heap.is_tracked(block).then_some(block))
    }

    fn redo(&self, block: &Block) -> bool {
        !self.restored.contains(&block.id)
    }

    fn page(&mut self, block: &Block) -> Result<&mut Page, WalError> {
        let lsn = self.record.lsn;
        (self.heap.pages.get_mut(&page_id(block)))
            .ok_or(problem(lsn, "a change to a page Straum has no image of"))
    }

    fn row(&mut self, block: &Block, offnum: u16) -> Result<Tuple, WalError> {
        let lsn = self.record.lsn;
        (self.page(block)?.get(offnum).cloned())
            .ok_or(problem(lsn, "a change to a row Straum doesn't have"))
    }

    fn put(&mut self, block: &Block, offnum: u16, tuple: Tuple) -> Result<(), WalError> {
        let lsn = self.record.lsn;
        (self.page(block)?.set(offnum, tuple)).ok_or(problem(lsn, "a row in slot 0"))
    }

    fn insert(&mut self) -> Result<(), WalError> {
        let main = self.main(3)?;
        let offnum = le_u16(&main[0..2]);
        let flags = main[2];
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
            self.changes.push(Change::Insert {
                file: block.file,
                new,
            });
        }
        Ok(())
    }

    /// A speculative insert stands: `INSERT ... ON CONFLICT` found no
    /// conflict, and the row it put in counts.
    fn confirm(&mut self) -> Result<(), WalError> {
        let offnum = le_u16(&self.main(2)?[0..2]);
        let Some(block) = self.block(0)? else {
            return Ok(());
        };
        let new = self.row(block, offnum)?;
        self.changes.push(Change::Insert {
            file: block.file,
            new,
        });
        Ok(())
    }

    /// A delete only marks the row, so it stays on the page until a prune.
    fn delete(&mut self) -> Result<(), WalError> {
        let main = self.main(8)?;
        let offnum = le_u16(&main[4..6]);
        let flags = main[7];
        let Some(block) = self.block(0)? else {
            return Ok(());
        };
        let old = self.row(block, offnum)?;
        // A super-delete takes back a speculative insert no one saw.
        if flags & DELETE_IS_SUPER == 0 {
            self.changes.push(Change::Delete {
                file: block.file,
                old,
            });
        }
        Ok(())
    }

    /// Block 0 is the new row's page, block 1 the old row's when it is
    /// another.
    fn update(&mut self) -> Result<(), WalError> {
        let main = self.main(14)?;
        let old_offnum = le_u16(&main[4..6]);
        let flags = main[7];
        let new_offnum = le_u16(&main[12..14]);
        let Some(new_block) = self.block(0)? else {
            return Ok(());
        };
        let old_block = (self.record.blocks.iter())
            .find(|block| block.id == 1)
            .unwrap_or(new_block);
        let old = self.row(old_block, old_offnum)?;
        if self.redo(new_block) {
            let new = updated(self.record.block_data(new_block), flags, &old)
                .ok_or_else(|| self.fail("an update that doesn't fit its old row"))?;
            self.put(new_block, new_offnum, new)?;
        }
        let new = self.row(new_block, new_offnum)?;
        self.changes.push(Change::Update {
            file: new_block.file,
            old,
            new,
        });
        Ok(())
    }

    /// `COPY` and other bulk inserts: many rows into one page.
    fn multi_insert(&mut self) -> Result<(), WalError> {
        let main = self.main(4)?;
        let count = le_u16(&main[2..4]);
        let offnums: Vec<u16> = if self.record.info & INIT_PAGE != 0 {
            (1..=count).collect()
        } else {
            let listed = main
                .get(4..4 + 2 * usize::from(count))
                .ok_or_else(|| self.short())?;
            listed
                .as_chunks::<2>()
                .0
                .iter()
                .map(|n| u16::from_le_bytes(*n))
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
            self.changes.push(Change::Insert {
                file: block.file,
                new,
            });
        }
        Ok(())
    }

    /// Pruning, vacuuming and freezing: the slots whose rows go.
    fn prune(&mut self) -> Result<(), WalError> {
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
        let main = self.main(20)?;
        let first_gone = le_u32(&main[0..4]);
        let file = RelationFile {
            tablespace: le_u32(&main[4..8]),
            database: le_u32(&main[8..12]),
            relation: le_u32(&main[12..16]),
        };
        if le_u32(&main[16..20]) & SMGR_TRUNCATE_HEAP != 0 {
            (self.heap).forget(|id, _| id.file == file && id.block >= first_gone);
        }
        Ok(())
    }
}

/// The slots a prune record's lists name: freeze plans first, at the
/// alignment of their transaction ids, then redirected, dead and now
/// unused slots, each list there only if its flag says so.
fn prune_slots(data: &[u8], flags: u8) -> Option<Vec<u16>> {
    let count = |at: usize| data.get(at..at + 2).map(|n| usize::from(le_u16(n)));
    let mut at = 0;
    if flags & PRUNE_HAS_FREEZE_PLANS != 0 {
        at += 4 + FREEZE_PLAN * count(at)?;
    }
    let mut gone = Vec::new();
    for (flag, width) in [
        (PRUNE_HAS_REDIRECTIONS, 2),
        (PRUNE_HAS_DEAD_ITEMS, 1),
        (PRUNE_HAS_NOW_UNUSED_ITEMS, 1),
    ] {
        if flags & flag != 0 {
            let list = data.get(at + 2..at + 2 + 2 * width * count(at)?)?;
            gone.extend(list.chunks_exact(2 * width).map(le_u16));
            at += 2 + list.len();
        }
    }
    Some(gone)
}

/// `xl_heap_header` then the tuple from its null bitmap on.
fn header_and_body(data: &[u8]) -> Option<Tuple> {
    Some(tuple(data.get(..5)?, &data[5..]))
}

/// A row from the five header bytes the WAL and the page share,
/// `t_infomask2`, `t_infomask` and `t_hoff`, and what follows them.
fn tuple(header: &[u8], body: &[u8]) -> Tuple {
    Tuple {
        infomask2: le_u16(&header[0..2]),
        infomask: le_u16(&header[2..4]),
        hoff: header[4],
        body: body.to_vec(),
    }
}

/// The new row of an update, as `heap_xlog_update` puts it together: the
/// logged bytes, with the prefix and suffix it shares with the old row
/// taken from that.
fn updated(data: &[u8], flags: u8, old: &Tuple) -> Option<Tuple> {
    let mut at = 0;
    let mut length = |flag: u8| {
        if flags & flag == 0 {
            return Some(0);
        }
        let n = usize::from(le_u16(data.get(at..at + 2)?));
        at += 2;
        Some(n)
    };
    let prefix = length(UPDATE_PREFIX_FROM_OLD)?;
    let suffix = length(UPDATE_SUFFIX_FROM_OLD)?;
    let header = header_and_body(data.get(at..)?)?;
    let logged = &header.body;
    let mut body = Vec::with_capacity(logged.len() + prefix + suffix);
    if prefix > 0 {
        let bitmap = usize::from(header.hoff).checked_sub(TUPLE_HEADER)?;
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
    Some(Tuple { body, ..header })
}

/// `count` rows, each an `xl_multi_insert_tuple` at a 2-byte boundary
/// and then its bytes from the null bitmap on.
fn multi_insert_tuples(data: &[u8], count: usize) -> Option<Vec<Tuple>> {
    let mut at = 0;
    let mut tuples = Vec::with_capacity(count);
    for _ in 0..count {
        at += at % 2;
        let header = data.get(at..at + 7)?;
        let length = usize::from(le_u16(&header[0..2]));
        at += 7;
        tuples.push(tuple(&header[2..], data.get(at..at + length)?));
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
            return Err(problem(
                lsn,
                "a page image in zstd; Straum reads wal_compression off, pglz and lz4",
            ));
        }
    };
    let hole = usize::from(image.hole_offset);
    let raw = raw
        .filter(|raw| raw.len() == size && hole <= size)
        .ok_or(problem(lsn, "a page image that doesn't decompress"))?;
    let mut page = Vec::with_capacity(BLOCK_SIZE);
    page.extend_from_slice(&raw[..hole]);
    page.resize(hole + usize::from(image.hole_length), 0);
    page.extend_from_slice(&raw[hole..]);
    Ok(page)
}

/// The rows of a heap page: each normal line pointer's tuple.
fn page_from_image(bytes: &[u8]) -> Option<Page> {
    let lower = usize::from(le_u16(bytes.get(12..14)?));
    let mut page = Page::default();
    // A new page is all zeros.
    if lower == 0 {
        return Some(page);
    }
    let pointers = bytes.get(PAGE_HEADER..lower)?;
    for (offnum, pointer) in (1u16..).zip(pointers.as_chunks::<LINE_POINTER>().0) {
        let pointer = u32::from_le_bytes(*pointer);
        if (pointer >> 15) & 3 != LP_NORMAL {
            continue;
        }
        let offset = (pointer & 0x7FFF) as usize;
        let length = (pointer >> 17) as usize;
        let row = bytes.get(offset..offset.checked_add(length)?)?;
        if length < TUPLE_HEADER {
            return None;
        }
        page.set(offnum, tuple(&row[18..TUPLE_HEADER], &row[TUPLE_HEADER..]))?;
    }
    Some(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::decode;
    use crate::wal::tests::{BlockSpec, ImageSpec, Spec, assemble};

    const FILE: RelationFile = RelationFile {
        tablespace: 1663,
        database: 5,
        relation: 16_384,
    };
    const OTHER: RelationFile = RelationFile {
        tablespace: 1663,
        database: 5,
        relation: 16_999,
    };

    fn tuple(columns: &[u8]) -> Tuple {
        Tuple {
            infomask2: 2,
            infomask: 0,
            hoff: 24,
            body: [&[0][..], columns].concat(),
        }
    }

    /// A heap page holding `rows` in its slots, as PostgreSQL lays it out.
    fn page_bytes(rows: &[(u16, &Tuple)]) -> Vec<u8> {
        let mut page = vec![0; BLOCK_SIZE];
        let slots = rows.iter().map(|(offnum, _)| *offnum).max().unwrap_or(0);
        let lower = PAGE_HEADER + LINE_POINTER * usize::from(slots);
        page[12..14].copy_from_slice(&(lower as u16).to_le_bytes());
        let mut upper = BLOCK_SIZE;
        for (offnum, tuple) in rows {
            let length = TUPLE_HEADER + tuple.body.len();
            upper -= length;
            page[upper + 18..upper + 20].copy_from_slice(&tuple.infomask2.to_le_bytes());
            page[upper + 20..upper + 22].copy_from_slice(&tuple.infomask.to_le_bytes());
            page[upper + 22] = tuple.hoff;
            page[upper + TUPLE_HEADER..upper + length].copy_from_slice(&tuple.body);
            let pointer = upper as u32 | LP_NORMAL << 15 | (length as u32) << 17;
            let at = PAGE_HEADER + LINE_POINTER * usize::from(offnum - 1);
            page[at..at + 4].copy_from_slice(&pointer.to_le_bytes());
        }
        page
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
    fn logged(tuple: &Tuple) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(tuple.infomask2.to_le_bytes());
        data.extend(tuple.infomask.to_le_bytes());
        data.push(tuple.hoff);
        data.extend(&tuple.body);
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

    fn insert(file: RelationFile, new: Tuple) -> Change {
        Change::Insert { file, new }
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
        let checkpoint = |redo: u64| {
            let mut main = redo.to_le_bytes().to_vec();
            main.extend([0; 80]);
            record(RM_XLOG, XLOG_CHECKPOINT_ONLINE, main, Vec::new())
        };
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
            Ok(vec![Change::Update {
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
            Ok(vec![Change::Delete {
                file: FILE,
                old: second.clone(),
            }])
        );
        let rows: Vec<(u16, &Tuple)> = at(&heap, 0).rows().collect();
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
            Ok(vec![Change::Update {
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
            data.extend((row.body.len() as u16).to_le_bytes());
            data.extend(row.infomask2.to_le_bytes());
            data.extend(row.infomask.to_le_bytes());
            data.push(row.hoff);
            data.extend(&row.body);
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
        let inserts: Vec<Change> = rows.iter().map(|row| insert(FILE, row.clone())).collect();
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

    #[test]
    fn counts_a_speculative_insert_once_confirmed() {
        let mut heap = heap(18);
        let row = tuple(b"maybe");
        let init = |id, number, data| BlockSpec {
            will_init: true,
            ..block(id, number, data, None)
        };
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
        let rows: Vec<Tuple> = (0..5u8).map(|i| tuple(&[i])).collect();
        let page: Vec<(u16, &Tuple)> = (1..).zip(&rows).collect();
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
            &(kept.len() as u32 - 15).to_le_bytes()[..1],
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

    fn error(heap: &mut Heap, record: &Record) -> &'static str {
        match heap.apply(record) {
            Err(WalError::At { problem, .. }) => problem,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn refuses_what_it_can_not_apply() {
        let mut heap = heap(18);
        let row = tuple(b"x");
        let no_page = record(
            RM_HEAP,
            HEAP_INSERT,
            insert_main(1, 0),
            vec![block(0, 0, logged(&row), None)],
        );
        assert_eq!(
            error(&mut heap, &no_page),
            "a change to a page Straum has no image of"
        );
        five_rows(&mut heap);
        let no_row = record(
            RM_HEAP,
            HEAP_DELETE,
            delete_main(9, 0),
            vec![block(0, 0, vec![], None)],
        );
        assert_eq!(
            error(&mut heap, &no_row),
            "a change to a row Straum doesn't have"
        );
        let slot_zero = record(
            RM_HEAP,
            HEAP_INSERT,
            insert_main(0, 0),
            vec![block(0, 0, logged(&row), None)],
        );
        assert_eq!(error(&mut heap, &slot_zero), "a row in slot 0");
        let short = record(
            RM_HEAP,
            HEAP_UPDATE,
            vec![0; 13],
            vec![block(0, 0, vec![], None)],
        );
        assert_eq!(
            error(&mut heap, &short),
            "a heap record shorter than its kind"
        );
        let headless = record(
            RM_HEAP,
            HEAP_INSERT,
            insert_main(6, 0),
            vec![block(0, 0, vec![0; 4], None)],
        );
        assert_eq!(
            error(&mut heap, &headless),
            "a heap record shorter than its kind"
        );
        let no_block = record(RM_HEAP, HEAP_INSERT, insert_main(6, 0), vec![]);
        assert_eq!(
            error(&mut heap, &no_block),
            "a heap record without its page"
        );
        let mut too_long = 200u16.to_le_bytes().to_vec();
        too_long.extend(logged(&row));
        let misfit = record(
            RM_HEAP,
            HEAP_UPDATE,
            update_main(1, UPDATE_PREFIX_FROM_OLD, 6),
            vec![block(0, 0, too_long, None)],
        );
        assert_eq!(
            error(&mut heap, &misfit),
            "an update that doesn't fit its old row"
        );
        let trailing = record(
            RM_HEAP2,
            HEAP2_MULTI_INSERT,
            [&[0, 0][..], &1u16.to_le_bytes(), &7u16.to_le_bytes()].concat(),
            vec![block(0, 0, vec![1, 0, 2, 0, 0, 0, 24, 0, 9], None)],
        );
        assert_eq!(
            error(&mut heap, &trailing),
            "a heap record shorter than its kind"
        );
        let broken_prune = record(
            RM_HEAP2,
            HEAP2_PRUNE,
            vec![1, PRUNE_HAS_DEAD_ITEMS],
            vec![block(0, 0, vec![3, 0, 1, 0], None)],
        );
        assert_eq!(
            error(&mut heap, &broken_prune),
            "a heap record shorter than its kind"
        );
        let page = page_bytes(&[(1, &row)]);
        let zstd = ImageSpec {
            compression: Some(Compression::Zstd),
            ..image(page[..100].to_vec())
        };
        let zstd = record(RM_XLOG, 0xB0, vec![], vec![block(0, 0, vec![], Some(zstd))]);
        assert_eq!(
            error(&mut heap, &zstd),
            "a page image in zstd; Straum reads wal_compression off, pglz and lz4"
        );
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
        assert_eq!(
            error(&mut heap, &garbage),
            "a page image that doesn't decompress"
        );
        let mut bad_pointer = page.clone();
        bad_pointer[24..28].copy_from_slice(&(8190u32 | LP_NORMAL << 15 | 100 << 17).to_le_bytes());
        let bad = record(
            RM_XLOG,
            0xB0,
            vec![],
            vec![block(0, 0, vec![], Some(image(bad_pointer)))],
        );
        assert_eq!(error(&mut heap, &bad), "a page image Straum can't read");
        let mut tiny = page;
        tiny[24..28].copy_from_slice(&(100u32 | LP_NORMAL << 15 | 10 << 17).to_le_bytes());
        let tiny = record(
            RM_XLOG,
            0xB0,
            vec![],
            vec![block(0, 0, vec![], Some(image(tiny)))],
        );
        assert_eq!(error(&mut heap, &tiny), "a page image Straum can't read");
    }

    #[test]
    fn says_what_went_wrong() {
        let error = problem(0x1_0000_0028, "a row in slot 0");
        assert_eq!(error.to_string(), "WAL at 1/28 has a row in slot 0");
    }
}
