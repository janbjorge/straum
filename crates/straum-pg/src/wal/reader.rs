//! Reads records out of WAL pages.

use std::ops::Range;

use super::record::{
    RECORD_CHECKSUM, RECORD_HEADER, RECORD_LENGTH, Record, align, checksum, decode_into,
};
use super::{
    CHECKSUM, CONTINUATION, LENGTH, OTHER_FORMAT, OVERWRITTEN, PAGE_ADDRESS, PAGE_FLAGS, PAGE_SIZE,
    WalError, at, le_u16, le_u32, le_u64,
};

/// `SizeOfXLogShortPHD`, `XLogPageHeaderData`: `xlp_magic`, `xlp_info`,
/// `xlp_tli`, `xlp_pageaddr`, `xlp_rem_len`, padded to 8 bytes.
pub(super) const SHORT_HEADER: u64 = 24;
/// `SizeOfXLogLongPHD`, `XLogLongPageHeaderData`: the short header, then
/// `xlp_sysid`, `xlp_seg_size` and `xlp_xlog_blcksz`. The first page of
/// a segment has it.
pub(super) const LONG_HEADER: u64 = 40;
pub(super) const PAGE_MAGIC: Range<usize> = 0..2;
pub(super) const PAGE_FLAGS_FIELD: Range<usize> = 2..4;
pub(super) const PAGE_ADDRESS_FIELD: Range<usize> = 8..16;
pub(super) const PAGE_REMAINING: Range<usize> = 16..20;
pub(super) const PAGE_SEGMENT_SIZE: Range<usize> = 32..36;
pub(super) const PAGE_BLOCK_SIZE: Range<usize> = 36..40;

/// `XLogRecordMaxSize`.
pub(super) const MAX_RECORD: usize = 1020 * 1024 * 1024;

pub(super) const FIRST_IS_CONTINUATION: u16 = 0x0001;
pub(super) const LONG_PAGE_HEADER: u16 = 0x0002;
/// Set unless a base backup runs; nothing for a reader.
pub(super) const BACKUP_REMOVABLE: u16 = 0x0004;
pub(super) const FIRST_IS_OVERWRITE: u16 = 0x0008;
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
    record: Record,
}

impl Records {
    /// Records from `start`, which must be where a record begins, such as a
    /// slot's restart LSN. The bytes fed in start at the page holding it.
    pub(crate) fn new(start: u64, segment_size: u64) -> Records {
        Records {
            segment_size,
            start: page_start(start),
            buffer: Vec::new(),
            next: start,
            magic: None,
            unchecked: (!start.is_multiple_of(PAGE_SIZE)).then_some(page_start(start)),
            record: Record::empty(),
        }
    }

    /// The LSN the next fed bytes must start at.
    pub(crate) fn wanted(&self) -> u64 {
        self.start + self.buffer.len() as u64
    }

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
        let mut bytes = std::mem::take(&mut self.record.bytes);
        let read = self.read(&mut bytes);
        self.record.bytes = bytes;
        let Some((position, end)) = read? else {
            return Ok(None);
        };
        decode_into(position, end, &mut self.record)?;
        self.next = if self.record.is_switch() {
            end.div_ceil(self.segment_size) * self.segment_size
        } else {
            align(end)
        };
        Ok(Some(&self.record))
    }

    /// The next whole record's bytes into `bytes`, checked: where it
    /// starts and ends.
    fn read(&mut self, bytes: &mut Vec<u8>) -> Result<Option<(u64, u64)>, WalError> {
        if let Some(page) = self.unchecked {
            let Some(header) = self.page_header(page)? else {
                return Ok(None);
            };
            if self.next < page + header.length {
                return Err(at(page, CONTINUATION));
            }
            self.unchecked = None;
        }
        let mut position = self.next;
        if position.is_multiple_of(PAGE_SIZE) {
            let Some(header) = self.page_header(position)? else {
                return Ok(None);
            };
            if header.flags & FIRST_IS_OVERWRITE != 0 {
                return Err(at(position, OVERWRITTEN));
            }
            if header.flags & FIRST_IS_CONTINUATION != 0 {
                return Err(at(position, CONTINUATION));
            }
            position += header.length;
        }
        let total = match self.slice(position, RECORD_HEADER as u64) {
            Some(head) if position % PAGE_SIZE + RECORD_HEADER as u64 <= PAGE_SIZE => {
                le_u32(&head[RECORD_LENGTH])
            }
            _ => {
                if self.gather(position, RECORD_HEADER, bytes)?.is_none() {
                    return Ok(None);
                }
                le_u32(&bytes[RECORD_LENGTH])
            }
        };
        let Ok(total) = usize::try_from(total) else {
            return Err(at(position, LENGTH));
        };
        if !(RECORD_HEADER..=MAX_RECORD).contains(&total) {
            return Err(at(position, LENGTH));
        }
        let Some(end) = self.gather(position, total, bytes)? else {
            return Ok(None);
        };
        let stored = le_u32(&bytes[RECORD_CHECKSUM]);
        if stored != checksum(bytes) {
            return Err(at(position, CHECKSUM));
        }
        Ok(Some((position, end)))
    }

    fn slice(&self, at: u64, length: u64) -> Option<&[u8]> {
        let from = usize::try_from(at.checked_sub(self.start)?).ok()?;
        let to = from.checked_add(usize::try_from(length).ok()?)?;
        self.buffer.get(from..to)
    }

    fn page_header(&mut self, page: u64) -> Result<Option<PageHeader>, WalError> {
        let Some(short) = self.slice(page, SHORT_HEADER) else {
            return Ok(None);
        };
        let magic = le_u16(&short[PAGE_MAGIC]);
        let flags = le_u16(&short[PAGE_FLAGS_FIELD]);
        let address = le_u64(&short[PAGE_ADDRESS_FIELD]);
        let remaining = le_u32(&short[PAGE_REMAINING]);
        if address != page {
            return Err(at(page, PAGE_ADDRESS));
        }
        if flags & !ALL_PAGE_FLAGS != 0 {
            return Err(at(page, PAGE_FLAGS));
        }
        match self.magic {
            None => self.magic = Some(magic),
            Some(known) if known != magic => {
                return Err(at(page, OTHER_FORMAT));
            }
            Some(_) => {}
        }
        let length = if flags & LONG_PAGE_HEADER != 0 {
            let Some(long) = self.slice(page, LONG_HEADER) else {
                return Ok(None);
            };
            let segment_size = u64::from(le_u32(&long[PAGE_SEGMENT_SIZE]));
            let page_size = u64::from(le_u32(&long[PAGE_BLOCK_SIZE]));
            if segment_size != self.segment_size || page_size != PAGE_SIZE {
                return Err(WalError::Sizes {
                    segment_size,
                    page_size,
                    expected_segment_size: self.segment_size,
                });
            }
            LONG_HEADER
        } else {
            SHORT_HEADER
        };
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
                // Past the record header, the page knows the whole length.
                let expected = if out.len() >= RECORD_HEADER {
                    Some(left)
                } else {
                    None
                };
                if header.flags & FIRST_IS_CONTINUATION == 0
                    || expected.is_some_and(|left| u64::from(header.remaining) != left)
                {
                    return Err(at(position, CONTINUATION));
                }
                position += header.length;
            }
            let room = PAGE_SIZE - position % PAGE_SIZE;
            let take = room.min((length - out.len()) as u64);
            let Some(bytes) = self.slice(position, take) else {
                return Ok(None);
            };
            out.extend_from_slice(bytes);
            position += take;
        }
        Ok(Some(position))
    }
}

struct PageHeader {
    flags: u16,
    remaining: u32,
    length: u64,
}

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
            let mut specs: Vec<Spec> = (0..=rng.below(40)).map(|_| random_spec(&mut rng)).collect();
            if rng.below(3) == 0 {
                let at = rng.below_as(specs.len() as u64);
                specs.insert(at, switch());
            }
            let assembled: Vec<Vec<u8>> = specs.iter().map(assemble).collect();
            let (wal, lsns) = lay_out(start, &assembled);
            let first = rng.below_as(specs.len() as u64);
            let records = read(&mut rng, &wal, start, lsns[first]).unwrap();
            assert_eq!(records.len(), specs.len() - first, "seed {seed}");
            for ((record, spec), lsn) in records.iter().zip(&specs[first..]).zip(&lsns[first..]) {
                check(record, spec, *lsn);
            }
        }
    }

    #[test]
    fn splits_a_record_header_across_pages() {
        let mut rng = Rng(1);
        // A main data length that leaves 8 bytes on the first page, so the
        // second record's header runs across the page boundary.
        let first_room = PAGE_SIZE - LONG_HEADER - 8;
        let mut first = random_spec(&mut rng);
        first.blocks.clear();
        first.toplevel_xid = None;
        first.origin = false;
        first.main = rng.bytes(usize::try_from(first_room).unwrap() - RECORD_HEADER - 5);
        let second = random_spec(&mut rng);
        let assembled = vec![assemble(&first), assemble(&second)];
        let (wal, lsns) = lay_out(SEGMENT, &assembled);
        assert_eq!(lsns[1] % PAGE_SIZE, PAGE_SIZE - 8);
        let records = read(&mut rng, &wal, SEGMENT, lsns[0]).unwrap();
        check(&records[1], &second, lsns[1]);
    }

    #[test]
    fn waits_for_the_rest_of_a_record() {
        let mut rng = Rng(2);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(20_000);
        let (wal, lsns) = lay_out(SEGMENT, &[assemble(&spec)]);
        let mut records = Records::new(lsns[0], SEGMENT);
        records.feed(SEGMENT, &wal[..wal.len() - 1]).unwrap();
        assert_eq!(records.next_record(), Ok(None));
        records
            .feed(records.wanted(), &wal[wal.len() - 1..])
            .unwrap();
        check(records.next_record().unwrap().unwrap(), &spec, lsns[0]);
        assert_eq!(records.next_record(), Ok(None));
    }

    fn one_record() -> (Vec<u8>, Vec<u64>) {
        let mut rng = Rng(3);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(100);
        lay_out(SEGMENT, &[assemble(&spec)])
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
        let (mut wal, lsns) = one_record();
        wal[offset(lsns[0]) + 30] ^= 1;
        assert_eq!(first_error(&wal, lsns[0]), at(lsns[0], CHECKSUM));
    }

    #[test]
    fn refuses_a_page_in_the_wrong_place() {
        let (mut wal, lsns) = one_record();
        wal[PAGE_ADDRESS_FIELD.start] ^= 1;
        assert!(matches!(
            first_error(&wal, lsns[0]),
            WalError::At {
                problem: PAGE_ADDRESS,
                ..
            }
        ));
    }

    #[test]
    fn refuses_unknown_page_flags() {
        let (mut wal, lsns) = one_record();
        wal[PAGE_FLAGS_FIELD.start] |= 0x80;
        assert!(matches!(
            first_error(&wal, lsns[0]),
            WalError::At {
                problem: PAGE_FLAGS,
                ..
            }
        ));
    }

    #[test]
    fn refuses_another_segment_size() {
        let (wal, lsns) = one_record();
        let mut records = Records::new(lsns[0], SEGMENT * 2);
        records.feed(SEGMENT, &wal).unwrap();
        assert!(matches!(records.next_record(), Err(WalError::Sizes { .. })));
    }

    #[test]
    fn refuses_a_change_of_format() {
        let mut rng = Rng(4);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(10_000);
        let (mut wal, lsns) = lay_out(SEGMENT, &[assemble(&spec)]);
        let second_page = offset(SEGMENT + PAGE_SIZE);
        wal[second_page] ^= 1;
        assert!(matches!(
            first_error(&wal, lsns[0]),
            WalError::At {
                problem: OTHER_FORMAT,
                ..
            }
        ));
    }

    #[test]
    fn refuses_a_page_that_does_not_carry_on() {
        let mut rng = Rng(5);
        let mut spec = random_spec(&mut rng);
        spec.main = rng.bytes(10_000);
        let (mut wal, lsns) = lay_out(SEGMENT, &[assemble(&spec)]);
        let second_page = offset(SEGMENT + PAGE_SIZE);
        wal[second_page + PAGE_FLAGS_FIELD.start] &= !FIRST_IS_CONTINUATION.to_le_bytes()[0];
        assert!(matches!(
            first_error(&wal, lsns[0]),
            WalError::At {
                problem: CONTINUATION,
                ..
            }
        ));
        let (mut wal, lsns) = lay_out(SEGMENT, &[assemble(&spec)]);
        wal[second_page + PAGE_REMAINING.start] ^= 1;
        assert!(matches!(
            first_error(&wal, lsns[0]),
            WalError::At {
                problem: CONTINUATION,
                ..
            }
        ));
    }

    #[test]
    fn refuses_a_continuation_where_a_record_should_start() {
        let (mut wal, _) = one_record();
        wal[PAGE_FLAGS_FIELD.start] |= FIRST_IS_CONTINUATION.to_le_bytes()[0];
        assert!(matches!(
            first_error(&wal, SEGMENT),
            WalError::At {
                problem: CONTINUATION,
                ..
            }
        ));
    }

    #[test]
    fn refuses_a_start_inside_a_page_header() {
        let (wal, _) = one_record();
        assert!(matches!(
            first_error(&wal, SEGMENT + 8),
            WalError::At {
                problem: CONTINUATION,
                ..
            }
        ));
    }

    #[test]
    fn refuses_an_overwritten_record() {
        let (mut wal, _) = one_record();
        wal[PAGE_FLAGS_FIELD.start] |= FIRST_IS_OVERWRITE.to_le_bytes()[0];
        assert!(matches!(
            first_error(&wal, SEGMENT),
            WalError::At {
                problem: OVERWRITTEN,
                ..
            }
        ));
    }

    #[test]
    fn refuses_an_impossible_length() {
        let (mut wal, lsns) = one_record();
        let at = offset(lsns[0]);
        wal[at..at + 4].copy_from_slice(&3u32.to_le_bytes());
        assert!(matches!(
            first_error(&wal, lsns[0]),
            WalError::At {
                problem: LENGTH,
                ..
            }
        ));
    }
}
