//! A heap page as Straum keeps it.

use std::ops::Range;

use super::super::{
    BLOCK_SIZE, Compression, Image, Record, SlotNumber, WalError, compress, read_u16,
};
use super::{HeapTuple, problem};
use crate::wal::Problem;

/// `xl_heap_header`: `t_infomask2`, `t_infomask`, `t_hoff`, the fields of
/// a row's header the WAL logs.
pub(super) const HEAP_HEADER: usize = 5;
pub(super) const HEADER_INFOMASK2: Range<usize> = 0..2;
pub(super) const HEADER_INFOMASK: Range<usize> = 2..4;
pub(super) const HEADER_HOFF: usize = 4;
/// `SizeofHeapTupleHeader`: what comes before the null bitmap.
pub(super) const TUPLE_HEADER: usize = 23;
/// Where `t_infomask2`, the first field `xl_heap_header` logs, lies in
/// `HeapTupleHeaderData`.
pub(super) const TUPLE_LOGGED_HEADER: usize = 18;
/// `SizeOfPageHeaderData`, and its `pd_lower`, where the line pointers end.
pub(super) const PAGE_HEADER: usize = 24;
pub(super) const PAGE_LOWER: Range<usize> = 12..14;
/// `ItemIdData`: `lp_off` in 15 bits, `lp_flags` in 2, `lp_len` in 15.
pub(super) const LINE_POINTER: usize = 4;
pub(super) const LP_OFFSET_MASK: u32 = 0x7FFF;
pub(super) const LP_FLAGS_SHIFT: u32 = 15;
pub(super) const LP_FLAGS_MASK: u32 = 0b11;
pub(super) const LP_LENGTH_SHIFT: u32 = 17;
pub(super) const LP_NORMAL: u32 = 1;

/// The rows of a page by slot: `tuples[offnum - 1]`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Page {
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

impl Page {
    /// The bytes the page holds, near enough to bound memory by.
    pub(super) fn footprint(&self) -> usize {
        let slots = self.tuples.capacity() * size_of::<Option<HeapTuple>>();
        PAGE_OVERHEAD + slots + self.rows
    }

    pub(crate) fn get(&self, offnum: SlotNumber) -> Option<&HeapTuple> {
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

    pub(super) fn set(&mut self, offnum: SlotNumber, tuple: HeapTuple) -> Option<()> {
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

    pub(super) fn remove(&mut self, offnum: SlotNumber) {
        if let Some(slot) = usize::from(offnum).checked_sub(1)
            && let Some(old) = self.tuples.get_mut(slot).and_then(Option::take)
        {
            self.rows -= old.body.len();
        }
    }
}

/// A row from the five header bytes the WAL and the page share,
/// `t_infomask2`, `t_infomask` and `t_hoff`, and what follows them.
pub(super) fn tuple(header: &[u8], body: &[u8]) -> HeapTuple {
    HeapTuple {
        infomask2: read_u16(&header[HEADER_INFOMASK2]),
        infomask: read_u16(&header[HEADER_INFOMASK]),
        hoff: header[HEADER_HOFF],
        body: body.into(),
    }
}

/// The page an image holds: decompressed, with its hole put back.
pub(super) fn restore(record: &Record, image: &Image) -> Result<Vec<u8>, WalError> {
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
pub(super) fn page_from_image(bytes: &[u8]) -> Option<Page> {
    let lower = usize::from(read_u16(bytes.get(PAGE_LOWER)?));
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
