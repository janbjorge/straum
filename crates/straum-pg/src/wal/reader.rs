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
        let (mut gathered, mut position) = self.resume(start, length, out.as_deref());
        if gathered == 0
            && let Some(out) = out.as_deref_mut()
        {
            out.clear();
        }
        while gathered < length {
            if position.is_multiple_of(PAGE_SIZE) {
                let Some(header) = self.page_header(position)? else {
                    return Ok(self.wait(start, length, gathered, position));
                };
                let left = (length - gathered) as u64;
                let continues = header.flags & FIRST_IS_CONTINUATION != 0;
                // Past the record header, the page knows the whole length.
                let length_known = gathered >= RECORD_HEADER;
                if !continues || (length_known && u64::from(header.remaining) != left) {
                    return Err(at(position, Problem::Continuation));
                }
                position += header.length;
            }
            let room = PAGE_SIZE - position % PAGE_SIZE;
            let take = room.min((length - gathered) as u64);
            let Some(bytes) = self.bytes_at(position, take) else {
                return Ok(self.wait(start, length, gathered, position));
            };
            gathered += bytes.len();
            if let Some(out) = out.as_deref_mut() {
                out.extend_from_slice(bytes);
            }
            position += take;
        }
        self.gathering = None;
        Ok(Some(position))
    }

    /// Where to go on gathering `length` bytes of the record at `start`:
    /// from where the last feed stopped, when that was this record and
    /// `out` still holds what was gathered, else from its start.
    fn resume(&self, start: u64, length: usize, out: Option<&Vec<u8>>) -> (usize, u64) {
        let Some(gathering) = self.gathering else {
            return (0, start);
        };
        if gathering.start != start || gathering.length != length {
            return (0, start);
        }
        if let Some(out) = out
            && out.len() != gathering.gathered
        {
            return (0, start);
        }
        (gathering.gathered, gathering.position)
    }

    /// Keeps how far the record at `start` is gathered, for the next feed.
    fn wait(&mut self, start: u64, length: usize, gathered: usize, position: u64) -> Option<u64> {
        self.gathering = Some(Gathering {
            start,
            length,
            gathered,
            position,
        });
        None
    }
}

/// The start of the WAL page holding `lsn`.
pub(super) fn page_start(lsn: u64) -> u64 {
    lsn - lsn % PAGE_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::heap::MAIN_FORK;
    use crate::wal::test_support::*;
    use crate::wal::{
        Oid, RESOURCE_MANAGER_HEAP, RESOURCE_MANAGER_STANDBY, RESOURCE_MANAGER_STORAGE,
        RESOURCE_MANAGER_TRANSACTION, RESOURCE_MANAGER_XLOG, record_end,
    };

    /// The major a seeded test lays its WAL out as: each in turn, so the
    /// seeds cover every major without running once per major.
    fn major_of_seed(seed: u64) -> u32 {
        let index = usize::try_from(seed).unwrap() % MAJORS.len();
        MAJORS[index]
    }

    #[test]
    fn reads_back_every_record_however_the_bytes_come() {
        for seed in 0..200 {
            let mut rng = Rng(seed);
            let major = major_of_seed(seed);
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
            let laid_out = lay_out(major, start, &assembled);
            let first = rng.below_usize(specs.len());
            let records =
                read(&mut rng, major, &laid_out.wal, start, laid_out.lsns[first]).unwrap();
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
        let laid_out = lay_out(MAJOR, SEGMENT, &assembled);
        assert_eq!(laid_out.lsns[1] % PAGE_SIZE, PAGE_SIZE - left_on_page);
        let records = read(&mut rng, MAJOR, &laid_out.wal, SEGMENT, laid_out.lsns[0]).unwrap();
        check(&records[1], &second, laid_out.lsns[1]);
    }

    #[test]
    fn waits_for_the_rest_of_a_record() {
        let mut rng = Rng(2);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(20_000);
        let LaidOut { wal, lsns } = lay_out(MAJOR, SEGMENT, &[assemble(&spec)]);
        let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
        records.feed(SEGMENT, &wal[..wal.len() - 1]).unwrap();
        assert_eq!(records.next_record(), Ok(None));
        records
            .feed(records.wanted(), &wal[wal.len() - 1..])
            .unwrap();
        check(records.next_record().unwrap().unwrap(), &spec, lsns[0]);
        assert_eq!(records.next_record(), Ok(None));
    }

    /// The records of `specs` laid out from the first segment, all fed
    /// and read: where the reader has passed.
    fn passed_after(specs: &[Spec]) -> (u64, LaidOut) {
        let assembled: Vec<Vec<u8>> = specs.iter().map(assemble).collect();
        let laid_out = lay_out(MAJOR, SEGMENT, &assembled);
        let mut records = Records::new(laid_out.lsns[0], SEGMENT, MAJOR);
        records.feed(SEGMENT, &laid_out.wal).unwrap();
        for _ in specs {
            assert!(records.next_record().unwrap().is_some());
        }
        assert_eq!(records.next_record(), Ok(None));
        (records.passed(), laid_out)
    }

    #[test]
    fn passes_the_padding_a_switch_leaves() {
        let mut rng = Rng(5);
        let (passed, laid_out) = passed_after(&[random_spec(&mut rng), switch()]);
        let segment_end = 2 * SEGMENT;
        assert_eq!(SEGMENT + laid_out.wal.len() as u64, segment_end);
        assert_eq!(passed, segment_end);
        // An idle server's insert position after the switch, past the
        // next segment's long header.
        let inserted = straum_app::Lsn(segment_end + LONG_HEADER);
        assert_eq!(record_end(inserted, SEGMENT), straum_app::Lsn(passed));
    }

    #[test]
    fn passes_no_further_than_a_record_mid_segment() {
        let mut rng = Rng(5);
        let (passed, laid_out) = passed_after(&[random_spec(&mut rng), random_spec(&mut rng)]);
        assert!(passed < 2 * SEGMENT);
        assert_eq!(passed, SEGMENT + laid_out.wal.len() as u64);
    }

    #[test]
    fn gathers_a_record_fed_in_pieces_once() {
        let mut rng = Rng(4);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(200_000);
        let LaidOut { wal, lsns } = lay_out(MAJOR, SEGMENT, &[assemble(&spec)]);
        let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
        let piece = 1000;
        let mut at = 0;
        while at + piece < wal.len() {
            records
                .feed(records.wanted(), &wal[at..at + piece])
                .unwrap();
            at += piece;
            assert_eq!(records.next_record(), Ok(None));
            let gathering = records.gathering.unwrap();
            assert!(records.wanted() - gathering.position < PAGE_SIZE, "at {at}");
        }
        records.feed(records.wanted(), &wal[at..]).unwrap();
        check(records.next_record().unwrap().unwrap(), &spec, lsns[0]);
        assert!(records.gathering.is_none());
    }

    #[test]
    fn goes_on_with_a_record_whose_header_runs_across_pages() {
        let mut rng = Rng(5);
        let left_on_page = 8;
        let mut first = random_spec(&mut rng);
        first.blocks.clear();
        first.toplevel_xid = None;
        first.origin = false;
        let main_data_header = size_of::<u8>() + size_of::<u32>();
        let first_room = usize::try_from(PAGE_SIZE - LONG_HEADER - left_on_page).unwrap();
        first.main = rng.bytes(first_room - RECORD_HEADER - main_data_header);
        let mut second = random_spec(&mut rng);
        second.main = rng.bytes(30_000);
        let LaidOut { wal, lsns } = lay_out(MAJOR, SEGMENT, &[assemble(&first), assemble(&second)]);
        assert_eq!(lsns[1] % PAGE_SIZE, PAGE_SIZE - left_on_page);
        let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
        let mut read = Vec::new();
        for byte in wal.chunks(100) {
            records.feed(records.wanted(), byte).unwrap();
            while let Some(record) = records.next_record().unwrap() {
                read.push(record.clone());
            }
        }
        assert_eq!(read.len(), 2);
        check(&read[1], &second, lsns[1]);
    }

    /// Whether a stream reading `files` must read `record`: its resource
    /// manager says something it reads, or it names one of their pages.
    fn concerns(record: &Record, files: &FastSet<RelationFile>) -> bool {
        let always = [
            RESOURCE_MANAGER_XLOG,
            RESOURCE_MANAGER_TRANSACTION,
            RESOURCE_MANAGER_STORAGE,
            RESOURCE_MANAGER_STANDBY,
        ];
        let page_image = record.resource_manager == RESOURCE_MANAGER_XLOG
            && matches!(record.info & crate::wal::RESOURCE_MANAGER_INFO, 0xA0 | 0xB0);
        if always.contains(&record.resource_manager) && !page_image {
            return true;
        }
        for block in &record.blocks {
            if block.fork == MAIN_FORK && files.contains(&block.file) {
                return true;
            }
        }
        false
    }

    #[test]
    fn passes_by_exactly_the_records_of_other_files() {
        let mut passed_by = 0;
        for seed in 0..300 {
            let mut rng = Rng(seed);
            let mut chosen = Vec::new();
            for relation in 0..3 {
                if rng.below(2) == 0 {
                    chosen.push(RelationFile {
                        relation: FIRST_USER_OID + relation,
                        ..FILE
                    });
                }
            }
            let files: FastSet<RelationFile> = chosen.iter().copied().collect();
            let mut specs = Vec::new();
            for _ in 0..=rng.below(40) {
                specs.push(random_spec(&mut rng));
            }
            let assembled: Vec<Vec<u8>> = specs.iter().map(assemble).collect();
            let major = major_of_seed(seed);
            let laid_out = lay_out(major, SEGMENT, &assembled);
            let every = read(&mut rng, major, &laid_out.wal, SEGMENT, laid_out.lsns[0]).unwrap();
            let wanted = Some(chosen.into_iter().collect());
            let read = read_passing_by(
                &mut rng,
                major,
                &laid_out.wal,
                SEGMENT,
                laid_out.lsns[0],
                wanted,
            );
            let mut read = read.unwrap().into_iter().peekable();
            for record in every {
                let was_read = read.next_if_eq(&record).is_some();
                if concerns(&record, &files) {
                    assert!(was_read, "seed {seed}: passed by {}", record.lsn);
                } else if headers_on_first_page(&record) {
                    assert!(!was_read, "seed {seed}: read {}", record.lsn);
                    passed_by += 1;
                }
            }
            assert_eq!(read.next(), None, "seed {seed}");
        }
        assert!(passed_by > 1000, "{passed_by}");
    }

    /// Whether `record`'s header and block headers end on the page it
    /// starts on, where the reader judges it in place. One whose headers
    /// run on is read whole.
    fn headers_on_first_page(record: &Record) -> bool {
        let mut payload = record.main_data().len();
        for block in &record.blocks {
            payload += record.block_data(block).len();
            if let Some(image) = &block.image {
                payload += record.image_bytes(image).len();
            }
        }
        let headers = (record.total_length() - payload) as u64;
        record.lsn % PAGE_SIZE + headers <= PAGE_SIZE
    }

    /// A record of 100 bytes of main data naming one page of `relation`,
    /// written by the heap, alone in its segment.
    fn heap_record(relation: Oid) -> LaidOut {
        let mut spec = plain_block(None, vec![7; 20]);
        spec.resource_manager = RESOURCE_MANAGER_HEAP;
        spec.blocks[0].file.relation = relation;
        spec.main = vec![1; 100];
        lay_out(MAJOR, SEGMENT, &[assemble(&spec)])
    }

    /// The first record past `wal` to a reader of `files`.
    fn first_read(
        wal: &[u8],
        start: u64,
        files: &[RelationFile],
    ) -> Result<Option<Record>, WalError> {
        let mut records = Records::new(start, SEGMENT, MAJOR);
        records.pass_by_other_files(files.iter().copied().collect());
        records.feed(SEGMENT, wal).unwrap();
        Ok(records.next_record()?.cloned())
    }

    #[test]
    fn checks_only_the_records_it_reads() {
        let LaidOut { mut wal, lsns } = heap_record(FIRST_USER_OID);
        // A byte of the block's data, past its headers, which the checksum
        // covers.
        wal[offset(lsns[0]) + RECORD_HEADER + 40] ^= 1;
        let theirs = RelationFile {
            relation: FIRST_USER_OID + 1,
            ..FILE
        };
        let ours = RelationFile {
            relation: FIRST_USER_OID,
            ..FILE
        };
        let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
        records.pass_by_other_files([theirs].into_iter().collect());
        records.feed(SEGMENT, &wal).unwrap();
        assert_eq!(records.next_record(), Ok(None));
        assert!(records.passed() > lsns[0]);
        assert_eq!(
            first_read(&wal, lsns[0], &[ours]),
            Err(at(lsns[0], Problem::Checksum))
        );
    }

    #[test]
    fn checks_the_pages_a_record_passed_by_runs_across() {
        let mut spec = plain_block(None, vec![7; 20]);
        spec.resource_manager = RESOURCE_MANAGER_HEAP;
        spec.main = vec![1; 10_000];
        let LaidOut { mut wal, lsns } = lay_out(MAJOR, SEGMENT, &[assemble(&spec)]);
        let page = second_page();
        wal[page + PAGE_REMAINING.start] ^= 1;
        let theirs = RelationFile {
            relation: FIRST_USER_OID + 1,
            ..FILE
        };
        let page_lsn = SEGMENT + PAGE_SIZE;
        assert_eq!(
            first_read(&wal, lsns[0], &[theirs]),
            Err(at(page_lsn, Problem::Continuation))
        );
    }

    /// One record of 100 bytes of main data, alone in its segment.
    fn one_record() -> LaidOut {
        one_record_of(MAJOR)
    }

    /// As [`one_record`], laid out as PostgreSQL `major` does.
    fn one_record_of(major: u32) -> LaidOut {
        let mut rng = Rng(3);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(100);
        lay_out(major, SEGMENT, &[assemble(&spec)])
    }

    /// The first page's flags in `wal`.
    fn first_page_flags(wal: &[u8]) -> u16 {
        read_u16(&wal[PAGE_FLAGS_FIELD])
    }

    /// `wal` with `flag` also set on its first page.
    fn set_first_page_flag(wal: &mut [u8], flag: u16) {
        let flags = first_page_flags(wal) | flag;
        wal[PAGE_FLAGS_FIELD].copy_from_slice(&flags.to_le_bytes());
    }

    /// One record so long it runs on to a second page.
    fn record_across_pages(seed: u64) -> LaidOut {
        let mut rng = Rng(seed);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(10_000);
        lay_out(MAJOR, SEGMENT, &[assemble(&spec)])
    }

    /// Where the second page of a segment laid out by `lay_out` begins.
    fn second_page() -> usize {
        offset(SEGMENT + PAGE_SIZE)
    }

    #[test]
    fn refuses_a_gap() {
        let mut records = Records::new(SEGMENT + LONG_HEADER, SEGMENT, MAJOR);
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
        assert_eq!(
            first_error(MAJOR, &wal, lsns[0]),
            at(lsns[0], Problem::Checksum)
        );
    }

    #[test]
    fn refuses_a_page_in_the_wrong_place() {
        let LaidOut { mut wal, lsns } = one_record();
        wal[PAGE_ADDRESS_FIELD.start] ^= 1;
        assert_eq!(
            first_problem(MAJOR, &wal, lsns[0]),
            Some(Problem::PageAddress)
        );
    }

    #[test]
    fn refuses_unknown_page_flags() {
        // No `XLP_` flag uses the eighth bit.
        let unknown_flag = 0x80;
        for major in MAJORS {
            let LaidOut { mut wal, lsns } = one_record_of(major);
            set_first_page_flag(&mut wal, unknown_flag);
            let problem = first_problem(major, &wal, lsns[0]);
            assert_eq!(problem, Some(Problem::PageFlags), "{major}");
        }
    }

    #[test]
    fn refuses_the_overwrite_bit_of_18_on_19() {
        let LaidOut { mut wal, lsns } = one_record_of(19);
        set_first_page_flag(&mut wal, FIRST_IS_OVERWRITE_UNTIL_18);
        assert_eq!(first_problem(19, &wal, lsns[0]), Some(Problem::PageFlags));
    }

    #[test]
    fn reads_past_backup_removable_before_19() {
        for major in [17, 18] {
            let LaidOut { wal, lsns } = one_record_of(major);
            assert_ne!(first_page_flags(&wal) & BACKUP_REMOVABLE, 0, "{major}");
            let mut records = Records::new(lsns[0], SEGMENT, major);
            records.feed(SEGMENT, &wal).unwrap();
            assert!(records.next_record().unwrap().is_some(), "{major}");
        }
    }

    #[test]
    fn lays_out_19_without_backup_removable() {
        let LaidOut { wal, .. } = one_record_of(19);
        assert_eq!(first_page_flags(&wal) & BACKUP_REMOVABLE, 0);
    }

    #[test]
    fn waits_for_the_rest_of_a_long_page_header() {
        let LaidOut { wal, lsns } = one_record();
        let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
        let short_header = usize::try_from(SHORT_HEADER).unwrap();
        records.feed(SEGMENT, &wal[..short_header]).unwrap();
        assert_eq!(records.next_record(), Ok(None));
        records
            .feed(records.wanted(), &wal[short_header..])
            .unwrap();
        assert!(records.next_record().unwrap().is_some());
    }

    #[test]
    fn refuses_another_segment_size() {
        let LaidOut { wal, lsns } = one_record();
        let mut records = Records::new(lsns[0], SEGMENT * 2, MAJOR);
        records.feed(SEGMENT, &wal).unwrap();
        assert!(matches!(records.next_record(), Err(WalError::Sizes { .. })));
    }

    #[test]
    fn refuses_a_change_of_format() {
        let LaidOut { mut wal, lsns } = record_across_pages(4);
        wal[second_page() + PAGE_MAGIC.start] ^= 1;
        assert_eq!(
            first_problem(MAJOR, &wal, lsns[0]),
            Some(Problem::OtherFormat)
        );
    }

    #[test]
    fn refuses_a_page_that_does_not_carry_on() {
        let LaidOut { mut wal, lsns } = record_across_pages(5);
        let flags = second_page() + PAGE_FLAGS_FIELD.start;
        wal[flags] &= !FIRST_IS_CONTINUATION.to_le_bytes()[0];
        assert_eq!(
            first_problem(MAJOR, &wal, lsns[0]),
            Some(Problem::Continuation)
        );
        let LaidOut { mut wal, lsns } = record_across_pages(5);
        wal[second_page() + PAGE_REMAINING.start] ^= 1;
        assert_eq!(
            first_problem(MAJOR, &wal, lsns[0]),
            Some(Problem::Continuation)
        );
    }

    #[test]
    fn refuses_a_continuation_where_a_record_should_start() {
        let LaidOut { mut wal, .. } = one_record();
        wal[PAGE_FLAGS_FIELD.start] |= FIRST_IS_CONTINUATION.to_le_bytes()[0];
        assert_eq!(
            first_problem(MAJOR, &wal, SEGMENT),
            Some(Problem::Continuation)
        );
    }

    #[test]
    fn refuses_a_start_inside_a_page_header() {
        let LaidOut { wal, .. } = one_record();
        let inside_header = SEGMENT + PAGE_ADDRESS_FIELD.start as u64;
        assert_eq!(
            first_problem(MAJOR, &wal, inside_header),
            Some(Problem::Continuation)
        );
    }

    #[test]
    fn refuses_an_overwritten_record() {
        for major in [17, 18] {
            let LaidOut { mut wal, .. } = one_record_of(major);
            set_first_page_flag(&mut wal, FIRST_IS_OVERWRITE_UNTIL_18);
            let problem = first_problem(major, &wal, SEGMENT);
            assert_eq!(problem, Some(Problem::Overwritten), "{major}");
        }
    }

    #[test]
    fn refuses_an_overwritten_record_where_19_moved_the_flag() {
        let LaidOut { mut wal, .. } = one_record_of(19);
        set_first_page_flag(&mut wal, FIRST_IS_OVERWRITE_FROM_19);
        assert_eq!(first_problem(19, &wal, SEGMENT), Some(Problem::Overwritten));
    }

    #[test]
    fn refuses_an_impossible_length() {
        let LaidOut { mut wal, lsns } = one_record();
        let record = offset(lsns[0]);
        let length = record + RECORD_LENGTH.start..record + RECORD_LENGTH.end;
        let shorter_than_a_header = 3u32;
        wal[length].copy_from_slice(&shorter_than_a_header.to_le_bytes());
        assert_eq!(first_problem(MAJOR, &wal, lsns[0]), Some(Problem::Length));
    }
}
