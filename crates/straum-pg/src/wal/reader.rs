//! Cuts the WAL a physical slot streams into records: the first step of
//! the physical stream, before `record.rs` reads each one. WAL comes as
//! 8 KiB pages, each opening with a page header (`XLogPageHeaderData` in
//! `src/include/access/xlog_internal.h`); a record may run on across
//! several pages, past their headers. `Records` takes the bytes as they
//! come, checks each page header and each record's checksum, as
//! `XLogReaderValidatePageHeader` and `ValidXLogRecord` in
//! `src/backend/access/transam/xlogreader.c` do, and gives back whole records.

use std::ops::Range;

use super::record::{
    RECORD_CHECKSUM, RECORD_HEADER, RECORD_LENGTH, Record, align, checksum, decode_into,
};
use super::{PAGE_SIZE, Problem, WalError, at, read_u16, read_u32, read_u64};

/// `SizeOfXLogShortPHD`, `XLogPageHeaderData`: `xlp_magic`, `xlp_info`,
/// `xlp_tli`, `xlp_pageaddr`, `xlp_rem_len`, padded to 8 bytes.
pub(super) const SHORT_HEADER: u64 = 24;
/// `SizeOfXLogLongPHD`, `XLogLongPageHeaderData`: the short header, then
/// `xlp_sysid`, `xlp_seg_size` and `xlp_xlog_blcksz`. The first page of
/// a segment has it.
pub(super) const LONG_HEADER: u64 = 40;
/// `xlp_magic`: the WAL format's version.
pub(super) const PAGE_MAGIC: Range<usize> = 0..2;
/// `xlp_info`: the page's flags.
pub(super) const PAGE_FLAGS_FIELD: Range<usize> = 2..4;
/// `xlp_pageaddr`: the LSN of the page itself.
pub(super) const PAGE_ADDRESS_FIELD: Range<usize> = 8..16;
/// `xlp_rem_len`: the bytes of a record from the page before still to come.
pub(super) const PAGE_REMAINING: Range<usize> = 16..20;
/// `xlp_seg_size` in the long header.
pub(super) const PAGE_SEGMENT_SIZE: Range<usize> = 32..36;
/// `xlp_xlog_blcksz` in the long header.
pub(super) const PAGE_BLOCK_SIZE: Range<usize> = 36..40;

/// `XLogRecordMaxSize`, the longest record PostgreSQL writes.
pub(super) const MAX_RECORD: usize = 1020 * 1024 * 1024;

/// `XLP_FIRST_IS_CONTRECORD`: the page opens with the rest of a record.
pub(super) const FIRST_IS_CONTINUATION: u16 = 0x0001;
/// `XLP_LONG_HEADER`: the page has the long header.
pub(super) const LONG_PAGE_HEADER: u16 = 0x0002;
/// `XLP_BKP_REMOVABLE`: set unless a base backup runs; nothing for a
/// reader.
pub(super) const BACKUP_REMOVABLE: u16 = 0x0004;
/// `XLP_FIRST_IS_OVERWRITE_CONTRECORD`: the page opens where a record
/// cut off by a crash was, written over.
pub(super) const FIRST_IS_OVERWRITE: u16 = 0x0008;
/// `XLP_ALL_FLAGS`: any other bit is a page Straum doesn't know.
pub(super) const ALL_PAGE_FLAGS: u16 =
    FIRST_IS_CONTINUATION | LONG_PAGE_HEADER | BACKUP_REMOVABLE | FIRST_IS_OVERWRITE;

/// The records in a run of WAL bytes, in order. Bytes go in as they come;
/// a record comes out once all of it is in.
pub(crate) struct Records {
    segment_size: u64,
    /// The LSN of `buffer[0]`.
    start: u64,
    buffer: Vec<u8>,
    /// Where the next record starts, or the page header before it.
    next: u64,
    /// The first page's magic; a change means a different WAL format.
    magic: Option<u16>,
    /// The page Straum starts in, when it starts past its header, which
    /// still wants checking.
    unchecked: Option<u64>,
    /// The record last read, its buffers kept for the next: reading
    /// allocates nothing once they have grown to the largest record.
    /// `next_record` lends it out, and a caller that keeps a record
    /// clones it.
    record: Record,
}

/// Where a record lies in the WAL: its first byte, and the byte after
/// its last.
struct RecordSpan {
    start: u64,
    end: u64,
}

/// What a page header says that the reader acts on.
struct PageHeader {
    flags: u16,
    /// `xlp_rem_len`.
    remaining: u32,
    /// The header's own length, short or long.
    length: u64,
}

impl Records {
    /// Records from `start`, which must be where a record begins, such as a
    /// slot's restart LSN. The bytes fed in start at the page holding it.
    pub(crate) fn new(start: u64, segment_size: u64) -> Records {
        let mut unchecked = None;
        if !start.is_multiple_of(PAGE_SIZE) {
            unchecked = Some(page_start(start));
        }
        Records {
            segment_size,
            start: page_start(start),
            buffer: Vec::new(),
            next: start,
            magic: None,
            unchecked,
            record: Record::empty(),
        }
    }

    /// The LSN the next fed bytes must start at.
    pub(crate) fn wanted(&self) -> u64 {
        self.start + self.buffer.len() as u64
    }

    /// Takes the next bytes of WAL, which start at `lsn`.
    pub(crate) fn feed(&mut self, lsn: u64, bytes: &[u8]) -> Result<(), WalError> {
        if lsn != self.wanted() {
            return Err(WalError::Gap {
                wanted: self.wanted(),
                got: lsn,
            });
        }
        // Read bytes go here, not as each record is read: by now only the
        // unread tail of a record is left to move to the front.
        let read = usize::try_from(page_start(self.next).saturating_sub(self.start))
            .unwrap_or(usize::MAX)
            .min(self.buffer.len());
        self.buffer.drain(..read);
        self.start += read as u64;
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// The next whole record, or `None` until more bytes come.
    pub(crate) fn next_record(&mut self) -> Result<Option<&Record>, WalError> {
        // The record's buffer is taken out while `read` borrows the reader,
        // and put back whatever `read` gives.
        let mut bytes = std::mem::take(&mut self.record.bytes);
        let read = self.read(&mut bytes);
        self.record.bytes = bytes;
        let Some(span) = read? else {
            return Ok(None);
        };
        decode_into(span.start, span.end, &mut self.record)?;
        if self.record.is_switch() {
            self.next = span.end.div_ceil(self.segment_size) * self.segment_size;
        } else {
            self.next = align(span.end);
        }
        Ok(Some(&self.record))
    }

    /// The next whole record's bytes into `bytes`, checked.
    fn read(&mut self, bytes: &mut Vec<u8>) -> Result<Option<RecordSpan>, WalError> {
        if let Some(page) = self.unchecked {
            let Some(header) = self.page_header(page)? else {
                return Ok(None);
            };
            if self.next < page + header.length {
                return Err(at(page, Problem::Continuation));
            }
            self.unchecked = None;
        }
        let mut start = self.next;
        if start.is_multiple_of(PAGE_SIZE) {
            let Some(header) = self.page_header(start)? else {
                return Ok(None);
            };
            if header.flags & FIRST_IS_OVERWRITE != 0 {
                return Err(at(start, Problem::Overwritten));
            }
            if header.flags & FIRST_IS_CONTINUATION != 0 {
                return Err(at(start, Problem::Continuation));
            }
            start += header.length;
        }
        let Some(total_length) = self.total_length(start, bytes)? else {
            return Ok(None);
        };
        let Some(end) = self.gather(start, total_length, bytes)? else {
            return Ok(None);
        };
        let stored = read_u32(&bytes[RECORD_CHECKSUM]);
        if stored != checksum(bytes) {
            return Err(at(start, Problem::Checksum));
        }
        Ok(Some(RecordSpan { start, end }))
    }

    /// `xl_tot_len` of the record at `start`, checked: read in place when
    /// the record's header is on one page, else gathered into `bytes`.
    fn total_length(&mut self, start: u64, bytes: &mut Vec<u8>) -> Result<Option<usize>, WalError> {
        let header_on_one_page = start % PAGE_SIZE + RECORD_HEADER as u64 <= PAGE_SIZE;
        let total_length = match self.bytes_at(start, RECORD_HEADER as u64) {
            Some(header) if header_on_one_page => read_u32(&header[RECORD_LENGTH]),
            _ => {
                if self.gather(start, RECORD_HEADER, bytes)?.is_none() {
                    return Ok(None);
                }
                read_u32(&bytes[RECORD_LENGTH])
            }
        };
        let Ok(total_length) = usize::try_from(total_length) else {
            return Err(at(start, Problem::Length));
        };
        if !(RECORD_HEADER..=MAX_RECORD).contains(&total_length) {
            return Err(at(start, Problem::Length));
        }
        Ok(Some(total_length))
    }

    /// The `length` bytes at `lsn`, if they have come.
    fn bytes_at(&self, lsn: u64, length: u64) -> Option<&[u8]> {
        let from = usize::try_from(lsn.checked_sub(self.start)?).ok()?;
        let to = from.checked_add(usize::try_from(length).ok()?)?;
        self.buffer.get(from..to)
    }

    /// The page header at `page`, checked, or `None` until it has come.
    fn page_header(&mut self, page: u64) -> Result<Option<PageHeader>, WalError> {
        let Some(short) = self.bytes_at(page, SHORT_HEADER) else {
            return Ok(None);
        };
        let magic = read_u16(&short[PAGE_MAGIC]);
        let flags = read_u16(&short[PAGE_FLAGS_FIELD]);
        let address = read_u64(&short[PAGE_ADDRESS_FIELD]);
        let remaining = read_u32(&short[PAGE_REMAINING]);
        if address != page {
            return Err(at(page, Problem::PageAddress));
        }
        if flags & !ALL_PAGE_FLAGS != 0 {
            return Err(at(page, Problem::PageFlags));
        }
        match self.magic {
            None => self.magic = Some(magic),
            Some(known) if known != magic => {
                return Err(at(page, Problem::OtherFormat));
            }
            Some(_) => {}
        }
        let mut length = SHORT_HEADER;
        if flags & LONG_PAGE_HEADER != 0 {
            let Some(long) = self.bytes_at(page, LONG_HEADER) else {
                return Ok(None);
            };
            let segment_size = u64::from(read_u32(&long[PAGE_SEGMENT_SIZE]));
            let page_size = u64::from(read_u32(&long[PAGE_BLOCK_SIZE]));
            if segment_size != self.segment_size || page_size != PAGE_SIZE {
                return Err(WalError::Sizes {
                    segment_size,
                    page_size,
                    expected_segment_size: self.segment_size,
                });
            }
            length = LONG_HEADER;
        }
        Ok(Some(PageHeader {
            flags,
            remaining,
            length,
        }))
    }

    /// `length` bytes of the record at `start` into `out`, without the page
    /// headers it runs across: where they end.
    fn gather(
        &mut self,
        start: u64,
        length: usize,
        out: &mut Vec<u8>,
    ) -> Result<Option<u64>, WalError> {
        out.clear();
        let mut position = start;
        while out.len() < length {
            if position.is_multiple_of(PAGE_SIZE) {
                let Some(header) = self.page_header(position)? else {
                    return Ok(None);
                };
                let left = (length - out.len()) as u64;
                let continues = header.flags & FIRST_IS_CONTINUATION != 0;
                // Past the record header, the page knows the whole length.
                let length_known = out.len() >= RECORD_HEADER;
                if !continues || (length_known && u64::from(header.remaining) != left) {
                    return Err(at(position, Problem::Continuation));
                }
                position += header.length;
            }
            let room = PAGE_SIZE - position % PAGE_SIZE;
            let take = room.min((length - out.len()) as u64);
            let Some(bytes) = self.bytes_at(position, take) else {
                return Ok(None);
            };
            out.extend_from_slice(bytes);
            position += take;
        }
        Ok(Some(position))
    }
}

/// The start of the WAL page holding `lsn`.
pub(super) fn page_start(lsn: u64) -> u64 {
    lsn - lsn % PAGE_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::test_support::*;

    #[test]
    fn reads_back_every_record_however_the_bytes_come() {
        for seed in 0..200 {
            let mut rng = Rng(seed);
            let start = SEGMENT * (1 + rng.below(4));
            let mut specs = Vec::new();
            for _ in 0..=rng.below(40) {
                specs.push(random_spec(&mut rng));
            }
            if rng.below(3) == 0 {
                let at = rng.below_usize(specs.len());
                specs.insert(at, switch());
            }
            let assembled: Vec<Vec<u8>> = specs.iter().map(assemble).collect();
            let laid_out = lay_out(start, &assembled);
            let first = rng.below_usize(specs.len());
            let records = read(&mut rng, &laid_out.wal, start, laid_out.lsns[first]).unwrap();
            assert_eq!(records.len(), specs.len() - first, "seed {seed}");
            for (index, record) in records.iter().enumerate() {
                check(record, &specs[first + index], laid_out.lsns[first + index]);
            }
        }
    }

    #[test]
    fn splits_a_record_header_across_pages() {
        let mut rng = Rng(1);
        // A main data length that leaves 8 bytes on the first page, so the
        // second record's header runs across the page boundary.
        let left_on_page = 8;
        let first_room = PAGE_SIZE - LONG_HEADER - left_on_page;
        let mut first = random_spec(&mut rng);
        first.blocks.clear();
        first.toplevel_xid = None;
        first.origin = false;
        // `XLR_BLOCK_ID_DATA_LONG`, then the main data's length.
        let main_data_header = size_of::<u8>() + size_of::<u32>();
        let main_length = usize::try_from(first_room).unwrap() - RECORD_HEADER - main_data_header;
        first.main = rng.bytes(main_length);
        let second = random_spec(&mut rng);
        let assembled = vec![assemble(&first), assemble(&second)];
        let laid_out = lay_out(SEGMENT, &assembled);
        assert_eq!(laid_out.lsns[1] % PAGE_SIZE, PAGE_SIZE - left_on_page);
        let records = read(&mut rng, &laid_out.wal, SEGMENT, laid_out.lsns[0]).unwrap();
        check(&records[1], &second, laid_out.lsns[1]);
    }

    #[test]
    fn waits_for_the_rest_of_a_record() {
        let mut rng = Rng(2);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(20_000);
        let LaidOut { wal, lsns } = lay_out(SEGMENT, &[assemble(&spec)]);
        let mut records = Records::new(lsns[0], SEGMENT);
        records.feed(SEGMENT, &wal[..wal.len() - 1]).unwrap();
        assert_eq!(records.next_record(), Ok(None));
        records
            .feed(records.wanted(), &wal[wal.len() - 1..])
            .unwrap();
        check(records.next_record().unwrap().unwrap(), &spec, lsns[0]);
        assert_eq!(records.next_record(), Ok(None));
    }

    /// One record of 100 bytes of main data, alone in its segment.
    fn one_record() -> LaidOut {
        let mut rng = Rng(3);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(100);
        lay_out(SEGMENT, &[assemble(&spec)])
    }

    /// One record so long it runs on to a second page.
    fn record_across_pages(seed: u64) -> LaidOut {
        let mut rng = Rng(seed);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(10_000);
        lay_out(SEGMENT, &[assemble(&spec)])
    }

    /// Where the second page of a segment laid out by `lay_out` begins.
    fn second_page() -> usize {
        offset(SEGMENT + PAGE_SIZE)
    }

    #[test]
    fn refuses_a_gap() {
        let mut records = Records::new(SEGMENT + LONG_HEADER, SEGMENT);
        assert_eq!(
            records.feed(SEGMENT + 8, &[0; 8]),
            Err(WalError::Gap {
                wanted: SEGMENT,
                got: SEGMENT + 8
            })
        );
    }

    #[test]
    fn refuses_a_flipped_byte() {
        let LaidOut { mut wal, lsns } = one_record();
        // A byte of the main data, which the checksum covers.
        wal[offset(lsns[0]) + RECORD_HEADER + 6] ^= 1;
        assert_eq!(first_error(&wal, lsns[0]), at(lsns[0], Problem::Checksum));
    }

    #[test]
    fn refuses_a_page_in_the_wrong_place() {
        let LaidOut { mut wal, lsns } = one_record();
        wal[PAGE_ADDRESS_FIELD.start] ^= 1;
        assert_eq!(first_problem(&wal, lsns[0]), Some(Problem::PageAddress));
    }

    #[test]
    fn refuses_unknown_page_flags() {
        // No `XLP_` flag uses the eighth bit.
        let unknown_flag = 0x80;
        let LaidOut { mut wal, lsns } = one_record();
        wal[PAGE_FLAGS_FIELD.start] |= unknown_flag;
        assert_eq!(first_problem(&wal, lsns[0]), Some(Problem::PageFlags));
    }

    #[test]
    fn refuses_another_segment_size() {
        let LaidOut { wal, lsns } = one_record();
        let mut records = Records::new(lsns[0], SEGMENT * 2);
        records.feed(SEGMENT, &wal).unwrap();
        assert!(matches!(records.next_record(), Err(WalError::Sizes { .. })));
    }

    #[test]
    fn refuses_a_change_of_format() {
        let LaidOut { mut wal, lsns } = record_across_pages(4);
        wal[second_page() + PAGE_MAGIC.start] ^= 1;
        assert_eq!(first_problem(&wal, lsns[0]), Some(Problem::OtherFormat));
    }

    #[test]
    fn refuses_a_page_that_does_not_carry_on() {
        let LaidOut { mut wal, lsns } = record_across_pages(5);
        let flags = second_page() + PAGE_FLAGS_FIELD.start;
        wal[flags] &= !FIRST_IS_CONTINUATION.to_le_bytes()[0];
        assert_eq!(first_problem(&wal, lsns[0]), Some(Problem::Continuation));
        let LaidOut { mut wal, lsns } = record_across_pages(5);
        wal[second_page() + PAGE_REMAINING.start] ^= 1;
        assert_eq!(first_problem(&wal, lsns[0]), Some(Problem::Continuation));
    }

    #[test]
    fn refuses_a_continuation_where_a_record_should_start() {
        let LaidOut { mut wal, .. } = one_record();
        wal[PAGE_FLAGS_FIELD.start] |= FIRST_IS_CONTINUATION.to_le_bytes()[0];
        assert_eq!(first_problem(&wal, SEGMENT), Some(Problem::Continuation));
    }

    #[test]
    fn refuses_a_start_inside_a_page_header() {
        let LaidOut { wal, .. } = one_record();
        let inside_header = SEGMENT + PAGE_ADDRESS_FIELD.start as u64;
        assert_eq!(
            first_problem(&wal, inside_header),
            Some(Problem::Continuation)
        );
    }

    #[test]
    fn refuses_an_overwritten_record() {
        let LaidOut { mut wal, .. } = one_record();
        wal[PAGE_FLAGS_FIELD.start] |= FIRST_IS_OVERWRITE.to_le_bytes()[0];
        assert_eq!(first_problem(&wal, SEGMENT), Some(Problem::Overwritten));
    }

    #[test]
    fn refuses_an_impossible_length() {
        let LaidOut { mut wal, lsns } = one_record();
        let record = offset(lsns[0]);
        let length = record + RECORD_LENGTH.start..record + RECORD_LENGTH.end;
        let shorter_than_a_header = 3u32;
        wal[length].copy_from_slice(&shorter_than_a_header.to_le_bytes());
        assert_eq!(first_problem(&wal, lsns[0]), Some(Problem::Length));
    }
}
