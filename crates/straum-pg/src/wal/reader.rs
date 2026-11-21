//! Cuts the WAL a physical slot streams into records: the first step of
//! the physical stream, before `record.rs` reads each one. WAL comes as
//! 8 KiB pages, each opening with a page header (`XLogPageHeaderData` in
//! `src/include/access/xlog_internal.h`); a record may run on across
//! several pages, past their headers. `Records` takes the bytes as they
//! come, checks each page header, its flags as the major that wrote the
//! WAL numbers them, and each record's checksum, as
//! `XLogReaderValidatePageHeader` and `ValidXLogRecord` in
//! `src/backend/access/transam/xlogreader.c` do, and gives back whole records.
//! Told which relation files the stream reads, it passes by a record that
//! names only other files' pages, read from its block headers in place:
//! not copied, checksummed or decoded. Most of a busy server's WAL is such
//! records (ADR-0008).

use std::ops::Range;

use straum_domain::FastSet;

use super::record::{
    Interest, RECORD_CHECKSUM, RECORD_HEADER, RECORD_LENGTH, Record, RelationFile, align, checksum,
    decode_into, interest,
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
/// `XLP_BKP_REMOVABLE` in 17 and 18: set unless a base backup runs;
/// nothing for a reader. 19 dropped it.
pub(super) const BACKUP_REMOVABLE: u16 = 0x0004;
/// `XLP_FIRST_IS_OVERWRITE_CONTRECORD` in 17 and 18.
const FIRST_IS_OVERWRITE_UNTIL_18: u16 = 0x0008;
/// `XLP_FIRST_IS_OVERWRITE_CONTRECORD` from 19 on, moved down to the bit
/// `XLP_BKP_REMOVABLE` left.
const FIRST_IS_OVERWRITE_FROM_19: u16 = 0x0004;
/// The first major without `XLP_BKP_REMOVABLE`.
const PAGE_FLAGS_RENUMBERED_MAJOR: u32 = 19;

/// The page flags whose bits depend on the major that wrote the WAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PageFlags {
    /// `XLP_FIRST_IS_OVERWRITE_CONTRECORD`: the page opens where a record
    /// cut off by a crash was, written over.
    first_is_overwrite: u16,
    /// `XLP_ALL_FLAGS`: any other bit is a page Straum doesn't know.
    all: u16,
}

/// The page flags of 17 and 18.
const PAGE_FLAGS_UNTIL_18: PageFlags = PageFlags {
    first_is_overwrite: FIRST_IS_OVERWRITE_UNTIL_18,
    all: FIRST_IS_CONTINUATION | LONG_PAGE_HEADER | BACKUP_REMOVABLE | FIRST_IS_OVERWRITE_UNTIL_18,
};

/// The page flags from 19 on.
const PAGE_FLAGS_FROM_19: PageFlags = PageFlags {
    first_is_overwrite: FIRST_IS_OVERWRITE_FROM_19,
    all: FIRST_IS_CONTINUATION | LONG_PAGE_HEADER | FIRST_IS_OVERWRITE_FROM_19,
};

impl PageFlags {
    /// The page flags PostgreSQL `major` writes.
    fn of_major(major: u32) -> PageFlags {
        if major >= PAGE_FLAGS_RENUMBERED_MAJOR {
            PAGE_FLAGS_FROM_19
        } else {
            PAGE_FLAGS_UNTIL_18
        }
    }
}

/// The records in a run of WAL bytes, in order. Bytes go in as they come;
/// a record comes out once all of it is in.
pub(crate) struct Records {
    segment_size: u64,
    /// The page flags of the major that wrote the WAL, which the setup
    /// check knows. The page magic doesn't name a major: it moves with
    /// each format change during a major's development.
    page_flags: PageFlags,
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
    /// The relation files whose pages the stream reads; `None` reads
    /// every record.
    files: Option<FastSet<RelationFile>>,
    /// Where the record after the last one read or passed by may start.
    passed: u64,
    /// How far the record being read has been gathered, so bytes that
    /// come in pieces are gathered once, not again from its start on
    /// every feed.
    gathering: Option<Gathering>,
}

/// A record gathered up to where the bytes fed so far end.
#[derive(Clone, Copy)]
struct Gathering {
    /// The record's first byte.
    start: u64,
    /// The record's bytes wanted, page headers left out.
    length: usize,
    /// The record's bytes gathered so far.
    gathered: usize,
    /// Where the next bytes to gather lie, page headers counted.
    position: u64,
}

/// Where a record lies in the WAL: its first byte, and the byte after
/// its last.
struct RecordSpan {
    start: u64,
    end: u64,
}

/// The next record, read whole or passed by.
enum Found {
    Whole(RecordSpan),
    PassedBy(RecordSpan),
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
    /// slot's restart LSN, in WAL PostgreSQL `major` wrote. The bytes fed
    /// in start at the page holding it.
    pub(crate) fn new(start: u64, segment_size: u64, major: u32) -> Records {
        let mut unchecked = None;
        if !start.is_multiple_of(PAGE_SIZE) {
            unchecked = Some(page_start(start));
        }
        Records {
            segment_size,
            page_flags: PageFlags::of_major(major),
            start: page_start(start),
            buffer: Vec::new(),
            next: start,
            magic: None,
            unchecked,
            record: Record::empty(),
            files: None,
            passed: start,
            gathering: None,
        }
    }

    /// Passes by, from here on, each record naming only pages of files
    /// other than `files` whose resource manager says nothing else the
    /// stream reads.
    pub(crate) fn pass_by_other_files(&mut self, files: FastSet<RelationFile>) {
        self.files = Some(files);
    }

    /// Where the record after the last one read or passed by may start:
    /// every record before it is done with.
    pub(crate) fn passed(&self) -> u64 {
        self.passed
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
        loop {
            // The record's buffer is taken out while `read` borrows the
            // reader, and put back whatever `read` gives.
            let mut bytes = std::mem::take(&mut self.record.bytes);
            let read = self.read(&mut bytes);
            self.record.bytes = bytes;
            let span = match read? {
                None => return Ok(None),
                Some(Found::PassedBy(span)) => {
                    self.next = align(span.end);
                    self.passed = self.next;
                    continue;
                }
                Some(Found::Whole(span)) => span,
            };
            decode_into(span.start, span.end, &mut self.record)?;
            // The zeros after a switch hold no record, and an idle server
            // writes none after them: the server stands at the segment's
            // end, so the stream must say it has passed them.
            if self.record.is_switch() {
                self.next = span.end.div_ceil(self.segment_size) * self.segment_size;
            } else {
                self.next = align(span.end);
            }
            self.passed = self.next;
            return Ok(Some(&self.record));
        }
    }

    /// The next record: its bytes into `bytes`, checked, or passed by.
    fn read(&mut self, bytes: &mut Vec<u8>) -> Result<Option<Found>, WalError> {
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
            if header.flags & self.page_flags.first_is_overwrite != 0 {
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
        if self.passes_by(start, total_length) {
            let Some(end) = self.gather(start, total_length, None)? else {
                return Ok(None);
            };
            return Ok(Some(Found::PassedBy(RecordSpan { start, end })));
        }
        let Some(end) = self.gather(start, total_length, Some(bytes))? else {
            return Ok(None);
        };
        let stored = read_u32(&bytes[RECORD_CHECKSUM]);
        if stored != checksum(bytes) {
            return Err(at(start, Problem::Checksum));
        }
        Ok(Some(Found::Whole(RecordSpan { start, end })))
    }

    /// Whether the record at `start` names only pages of files the stream
    /// doesn't read, judged from the bytes on its first page.
    fn passes_by(&self, start: u64, total_length: usize) -> bool {
        let Some(files) = &self.files else {
            return false;
        };
        let on_first_page = (PAGE_SIZE - start % PAGE_SIZE).min(total_length as u64);
        let Some(head) = self.bytes_at(start, on_first_page) else {
            return false;
        };
        interest(head, total_length, files) == Interest::PassBy
    }

    /// `xl_tot_len` of the record at `start`, checked: read in place when
    /// the record's header is on one page, else gathered into `bytes`.
    fn total_length(&mut self, start: u64, bytes: &mut Vec<u8>) -> Result<Option<usize>, WalError> {
        let header_on_one_page = start % PAGE_SIZE + RECORD_HEADER as u64 <= PAGE_SIZE;
        let total_length = match self.bytes_at(start, RECORD_HEADER as u64) {
            Some(header) if header_on_one_page => read_u32(&header[RECORD_LENGTH]),
            _ if self.past_header(start) => read_u32(&bytes[RECORD_LENGTH]),
            _ => {
                if self.gather(start, RECORD_HEADER, Some(bytes))?.is_none() {
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

    /// Whether the record at `start` is partly gathered past its header,
    /// which then opens the record's buffer: gathered there by
    /// `total_length`, or by the gathering itself.
    fn past_header(&self, start: u64) -> bool {
        let Some(gathering) = self.gathering else {
            return false;
        };
        gathering.start == start && gathering.gathered >= RECORD_HEADER
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
        if flags & !self.page_flags.all != 0 {
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
    /// headers it runs across: where they end. With no `out`, only the
    /// page headers are checked and nothing is copied.
    fn gather(
        &mut self,
        start: u64,
        length: usize,
        mut out: Option<&mut Vec<u8>>,
    ) -> Result<Option<u64>, WalError> {
        let mut progress = self.resume(start, length, out.as_deref());
        if progress.gathered == 0
            && let Some(out) = out.as_deref_mut()
        {
            out.clear();
        }
        while progress.gathered < length {
            if progress.position.is_multiple_of(PAGE_SIZE) {
                let Some(header) = self.page_header(progress.position)? else {
                    self.gathering = Some(progress);
                    return Ok(None);
                };
                let left = (length - progress.gathered) as u64;
                let continues = header.flags & FIRST_IS_CONTINUATION != 0;
                // Past the record header, the page knows the whole length.
                let length_known = progress.gathered >= RECORD_HEADER;
                if !continues || (length_known && u64::from(header.remaining) != left) {
                    return Err(at(progress.position, Problem::Continuation));
                }
                progress.position += header.length;
            }
            let room = PAGE_SIZE - progress.position % PAGE_SIZE;
            let take = room.min((length - progress.gathered) as u64);
            let Some(bytes) = self.bytes_at(progress.position, take) else {
                self.gathering = Some(progress);
                return Ok(None);
            };
            progress.gathered += bytes.len();
            if let Some(out) = out.as_deref_mut() {
                out.extend_from_slice(bytes);
            }
            progress.position += take;
        }
        self.gathering = None;
        Ok(Some(progress.position))
    }

    /// Where to go on gathering `length` bytes of the record at `start`:
    /// from where the last feed stopped, when that was this record and
    /// `out` still holds what was gathered, else from its start.
    fn resume(&self, start: u64, length: usize, out: Option<&Vec<u8>>) -> Gathering {
        let from_start = Gathering {
            start,
            length,
            gathered: 0,
            position: start,
        };
        let Some(gathering) = self.gathering else {
            return from_start;
        };
        if gathering.start != start || gathering.length != length {
            return from_start;
        }
        if let Some(out) = out
            && out.len() != gathering.gathered
        {
            return from_start;
        }
        gathering
    }
}

/// The start of the WAL page holding `lsn`.
pub(super) fn page_start(lsn: u64) -> u64 {
    lsn - lsn % PAGE_SIZE
}

#[cfg(test)]
mod tests;
