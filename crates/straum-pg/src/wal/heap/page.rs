//! One heap page as Straum keeps it: the rows by slot, with the bytes they
//! hold, and how to read a page out of a full-page image. A heap page
//! (`src/include/storage/bufpage.h`) opens with a header, then an array
//! of line pointers (`itemid.h`), one per slot, each saying where on the
//! page its row lies; the rows (`htup_details.h`) fill the page from the
//! end. Records bring a page whole as an image; edits then change its
//! rows slot by slot.

use std::ops::Range;

use super::super::{
    BLOCK_SIZE, Compression, Image, Problem, Record, SlotNumber, WalError, compress, read_u16,
};
use super::HeapTuple;
use crate::wal::at;

/// `xl_heap_header`, the part of a row's header the WAL logs:
/// `t_infomask2`, `t_infomask`, `t_hoff`.
pub(super) const HEAP_HEADER: usize = 5;
/// `xl_heap_header.t_infomask2`.
pub(super) const HEADER_INFOMASK2: Range<usize> = 0..2;
/// `xl_heap_header.t_infomask`.
pub(super) const HEADER_INFOMASK: Range<usize> = 2..4;
/// `xl_heap_header.t_hoff`.
pub(super) const HEADER_HOFF: usize = 4;
/// `SizeofHeapTupleHeader`: what comes before the null bitmap.
pub(in crate::wal) const TUPLE_HEADER: usize = 23;
/// Where `t_infomask2`, the first field `xl_heap_header` logs, lies in
/// `HeapTupleHeaderData`.
pub(super) const TUPLE_LOGGED_HEADER: usize = 18;
/// `SizeOfPageHeaderData`: where the line pointers start.
pub(super) const PAGE_HEADER: usize = 24;
/// `PageHeaderData.pd_lower`: where the line pointers end.
pub(super) const PAGE_LOWER: Range<usize> = 12..14;
/// `ItemIdData`: `lp_off` in 15 bits, `lp_flags` in 2, `lp_len` in 15.
pub(super) const LINE_POINTER: usize = 4;
/// `ItemIdData.lp_off`: where the row starts on the page.
pub(super) const LP_OFFSET_MASK: u32 = 0x7FFF;
/// `ItemIdData.lp_flags`, after `lp_off`.
pub(super) const LP_FLAGS_SHIFT: u32 = 15;
/// `ItemIdData.lp_flags`, two bits.
pub(super) const LP_FLAGS_MASK: u32 = 0b11;
/// `ItemIdData.lp_len`, after `lp_flags`: the row's length.
pub(super) const LP_LENGTH_SHIFT: u32 = 17;
/// `LP_NORMAL`: the slot holds a row. The others are unused, redirected
/// or dead.
pub(super) const LP_NORMAL: u32 = 1;

/// The rows of one heap page, by slot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Page {
    /// Slot 1 at index 0: `tuples[slot - 1]`.
    pub(super) tuples: Vec<Option<HeapTuple>>,
    /// The rows' bytes, kept as they change: summing them per record cost
    /// a walk over the page.
    pub(super) rows: usize,
    /// The end of the last record that changed the page: its page LSN.
    pub(super) lsn: u64,
    /// What [`Page::footprint`] was when last counted.
    pub(super) counted: usize,
}

/// What a page costs beyond its rows: its map entry and the page itself.
const PAGE_OVERHEAD: usize = 64 + size_of::<Page>();

/// Where a slot's row sits in `Page::tuples`, if the slot is not 0.
fn slot_index(slot: SlotNumber) -> Option<usize> {
    usize::from(slot).checked_sub(1)
}

impl Page {
    /// The bytes the page holds, near enough to bound memory by.
    pub(super) fn footprint(&self) -> usize {
        let slots = self.tuples.capacity() * size_of::<Option<HeapTuple>>();
        PAGE_OVERHEAD + slots + self.rows
    }

    /// The row in `slot`, if the page holds one there.
    pub(crate) fn get(&self, slot: SlotNumber) -> Option<&HeapTuple> {
        let tuple = self.tuples.get(slot_index(slot)?)?;
        tuple.as_ref()
    }

    /// Every row with its slot, in slot order.
    // `impl Iterator`: callers walk or count the rows; a list underneath
    // keeps it a plain loop.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> impl Iterator<Item = (SlotNumber, &HeapTuple)> {
        let mut rows = Vec::new();
        let mut slot: SlotNumber = 0;
        for tuple in &self.tuples {
            slot += 1;
            if let Some(tuple) = tuple {
                rows.push((slot, tuple));
            }
        }
        rows.into_iter()
    }

    /// Puts `tuple` in `slot`, over any row there: `None` for slot 0.
    pub(super) fn set(&mut self, slot: SlotNumber, tuple: HeapTuple) -> Option<()> {
        let index = slot_index(slot)?;
        if self.tuples.len() <= index {
            self.tuples.resize(index + 1, None);
        }
        self.rows += tuple.body.len();
        if let Some(old) = self.tuples[index].replace(tuple) {
            self.rows -= old.body.len();
        }
        Some(())
    }

    /// Empties `slot`, if it holds a row.
    pub(super) fn remove(&mut self, slot: SlotNumber) {
        let Some(index) = slot_index(slot) else {
            return;
        };
        let Some(entry) = self.tuples.get_mut(index) else {
            return;
        };
        if let Some(old) = entry.take() {
            self.rows -= old.body.len();
        }
    }
}

/// A row from the five header bytes the WAL and the page share,
/// `t_infomask2`, `t_infomask` and `t_hoff`, and what follows them.
pub(super) fn tuple_from_header(header: &[u8], body: &[u8]) -> HeapTuple {
    HeapTuple {
        infomask2: read_u16(&header[HEADER_INFOMASK2]),
        infomask: read_u16(&header[HEADER_INFOMASK]),
        hoff: header[HEADER_HOFF],
        body: body.into(),
    }
}

/// The page an image holds: decompressed, with its hole put back. The hole
/// is the free middle of the page, which PostgreSQL leaves out of the image.
pub(super) fn restore(record: &Record, image: &Image) -> Result<Vec<u8>, WalError> {
    let lsn = record.lsn;
    let stored = record.image_bytes(image);
    let size = BLOCK_SIZE - usize::from(image.hole_length);
    let decompressed = match image.compression {
        None => Some(stored.to_vec()),
        Some(Compression::Pglz) => compress::pglz(stored, size),
        Some(Compression::Lz4) => compress::lz4(stored, size),
        Some(Compression::Zstd) => {
            return Err(at(lsn, Problem::ImageInZstd));
        }
    };
    let hole = usize::from(image.hole_offset);
    let Some(raw) = decompressed else {
        return Err(at(lsn, Problem::ImageNotDecompressing));
    };
    if raw.len() != size || hole > size {
        return Err(at(lsn, Problem::ImageNotDecompressing));
    }
    let mut page = Vec::with_capacity(BLOCK_SIZE);
    page.extend_from_slice(&raw[..hole]);
    page.resize(hole + usize::from(image.hole_length), 0);
    page.extend_from_slice(&raw[hole..]);
    Ok(page)
}

/// The rows of a heap page: each normal line pointer's row.
pub(super) fn page_from_image(bytes: &[u8]) -> Option<Page> {
    let lower = usize::from(read_u16(bytes.get(PAGE_LOWER)?));
    let mut page = Page::default();
    // A new page is all zeros.
    if lower == 0 {
        return Some(page);
    }
    let (pointers, _) = bytes.get(PAGE_HEADER..lower)?.as_chunks::<LINE_POINTER>();
    let mut slot: SlotNumber = 0;
    for pointer in pointers {
        slot += 1;
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
        page.set(slot, tuple_from_header(header, &row[TUPLE_HEADER..]))?;
    }
    Some(page)
}
