//! Physical WAL as a physical slot streams it: 8 KiB pages, each opening
//! with a header, holding records that may run across pages. PostgreSQL
//! writes it in the server's byte order; Straum reads little-endian only,
//! which every platform PostgreSQL ships on for these majors uses.
//! Layouts: `src/include/access/xlog_internal.h` and `xlogrecord.h`,
//! read as `DecodeXLogRecord` in `src/backend/access/transam/xlogreader.c`
//! does.

use std::fmt;
use std::ops::Range;

use straum_app::Lsn;

mod compress;
pub(crate) mod heap;
pub(crate) mod messages;
pub(crate) mod pipeline;
pub(crate) mod transactions;
pub(crate) mod tuple;
pub(crate) mod xact;

/// `XLOG_BLCKSZ`. The long page header names it, and Straum refuses others.
pub(crate) const PAGE_SIZE: u64 = 8192;
/// `BLCKSZ`, the size of a table page and of a full-page image.
pub(crate) const BLOCK_SIZE: usize = 8192;
/// An image header holds lengths within a page as `uint16`.
#[expect(clippy::cast_possible_truncation, reason = "BLCKSZ, 8192, fits a u16")]
const BLOCK_SIZE_U16: u16 = BLOCK_SIZE as u16;

/// `SizeOfXLogShortPHD`, `XLogPageHeaderData`: `xlp_magic`, `xlp_info`,
/// `xlp_tli`, `xlp_pageaddr`, `xlp_rem_len`, padded to 8 bytes.
const SHORT_HEADER: u64 = 24;
/// `SizeOfXLogLongPHD`, `XLogLongPageHeaderData`: the short header, then
/// `xlp_sysid`, `xlp_seg_size` and `xlp_xlog_blcksz`. The first page of
/// a segment has it.
const LONG_HEADER: u64 = 40;
const PAGE_MAGIC: Range<usize> = 0..2;
const PAGE_FLAGS_FIELD: Range<usize> = 2..4;
const PAGE_ADDRESS_FIELD: Range<usize> = 8..16;
const PAGE_REMAINING: Range<usize> = 16..20;
const PAGE_SEGMENT_SIZE: Range<usize> = 32..36;
const PAGE_BLOCK_SIZE: Range<usize> = 36..40;

/// `SizeOfXLogRecord`, `XLogRecord`: `xl_tot_len`, `xl_xid`, `xl_prev`,
/// `xl_info`, `xl_rmid`, two bytes of padding, `xl_crc`.
const RECORD_HEADER: usize = 24;
const RECORD_LENGTH: Range<usize> = 0..4;
const RECORD_XID: Range<usize> = 4..8;
const RECORD_INFO: usize = 16;
const RECORD_RESOURCE_MANAGER: usize = 17;
const RECORD_CHECKSUM: Range<usize> = 20..24;
/// `XLogRecordMaxSize`.
const MAX_RECORD: usize = 1020 * 1024 * 1024;
/// `MAXALIGN`: records start on 8-byte boundaries.
const RECORD_ALIGNMENT: u64 = 8;
/// The bits of `xl_info` its resource manager owns; the low four are
/// `XLR_INFO_MASK`, the WAL insertion's own.
const RESOURCE_MANAGER_INFO: u8 = 0xF0;
/// `XLR_BLOCK_ID_ORIGIN` is followed by a `RepOriginId`, a `uint16`.
const ORIGIN_SIZE: usize = 2;

const FIRST_IS_CONTINUATION: u16 = 0x0001;
const LONG_PAGE_HEADER: u16 = 0x0002;
/// Set unless a base backup runs; nothing for a reader.
const BACKUP_REMOVABLE: u16 = 0x0004;
const FIRST_IS_OVERWRITE: u16 = 0x0008;
const ALL_PAGE_FLAGS: u16 =
    FIRST_IS_CONTINUATION | LONG_PAGE_HEADER | BACKUP_REMOVABLE | FIRST_IS_OVERWRITE;

const XLOG_RESOURCE_MANAGER: u8 = 0;
const XLOG_SWITCH: u8 = 0x40;

const DATA_SHORT: u8 = 255;
const DATA_LONG: u8 = 254;
const ORIGIN: u8 = 253;
const TOPLEVEL_XID: u8 = 252;
const MAX_BLOCK_ID: u8 = 32;

const FORK_MASK: u8 = 0x0F;
const HAS_IMAGE: u8 = 0x10;
const HAS_DATA: u8 = 0x20;
const WILL_INIT: u8 = 0x40;
const SAME_RELATION: u8 = 0x80;

const IMAGE_HAS_HOLE: u8 = 0x01;
const IMAGE_APPLY: u8 = 0x02;
const IMAGE_PGLZ: u8 = 0x04;
const IMAGE_LZ4: u8 = 0x08;
const IMAGE_ZSTD: u8 = 0x10;

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

/// One WAL record. Owns its bytes, since a record split across pages has
/// to be put back together anyway; `Records` lends out the one it reuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) lsn: u64,
    /// The end of the record, before the alignment to the next.
    pub(crate) end: u64,
    pub(crate) xid: u32,
    pub(crate) resource_manager: u8,
    /// The record kind within its resource manager, with its flag bits.
    pub(crate) info: u8,
    /// The top transaction, when `xid` is a subtransaction it names first.
    pub(crate) toplevel_xid: Option<u32>,
    pub(crate) blocks: Vec<Block>,
    main: Range<usize>,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Block {
    pub(crate) id: u8,
    pub(crate) fork: u8,
    /// Redo starts the page afresh, so no earlier state of it is needed.
    pub(crate) will_init: bool,
    pub(crate) file: RelationFile,
    pub(crate) number: u32,
    pub(crate) image: Option<Image>,
    data: Range<usize>,
}

/// Where a relation's pages live: `RelFileLocator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RelationFile {
    pub(crate) tablespace: u32,
    pub(crate) database: u32,
    pub(crate) relation: u32,
}

/// A full-page image, as logged: maybe compressed, without its hole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Image {
    pub(crate) hole_offset: u16,
    pub(crate) hole_length: u16,
    pub(crate) compression: Option<Compression>,
    /// Redo restores it; otherwise it is there only for consistency checks.
    pub(crate) apply: bool,
    bytes: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    Pglz,
    Lz4,
    Zstd,
}

impl Record {
    pub(crate) fn main_data(&self) -> &[u8] {
        &self.bytes[self.main.clone()]
    }

    pub(crate) fn block_data(&self, block: &Block) -> &[u8] {
        &self.bytes[block.data.clone()]
    }

    pub(crate) fn image_bytes(&self, image: &Image) -> &[u8] {
        &self.bytes[image.bytes.clone()]
    }

    /// `xl_tot_len`: the header, the block headers and every payload.
    #[cfg(test)]
    pub(crate) fn total_length(&self) -> usize {
        self.bytes.len()
    }

    /// Where the next record may start: the end, aligned. Logical
    /// decoding reports it as a transaction's end (`EndRecPtr`).
    pub(crate) fn next(&self) -> u64 {
        align(self.end)
    }

    /// A switch record ends its segment: the rest of it is padding.
    pub(crate) fn is_switch(&self) -> bool {
        self.resource_manager == XLOG_RESOURCE_MANAGER
            && self.info & RESOURCE_MANAGER_INFO == XLOG_SWITCH
    }
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
            record: Record {
                lsn: 0,
                end: 0,
                xid: 0,
                resource_manager: 0,
                info: 0,
                toplevel_xid: None,
                blocks: Vec::new(),
                main: 0..0,
                bytes: Vec::new(),
            },
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

#[cfg(test)]
fn decode(lsn: u64, end: u64, bytes: Vec<u8>) -> Result<Record, WalError> {
    let mut record = Records::new(lsn, PAGE_SIZE).record;
    record.bytes = bytes;
    decode_into(lsn, end, &mut record)?;
    Ok(record)
}

/// Reads `record`'s bytes into the rest of it.
fn decode_into(lsn: u64, end: u64, record: &mut Record) -> Result<(), WalError> {
    let short = at(lsn, BLOCKS);
    let bytes = &record.bytes;
    let blocks = &mut record.blocks;
    blocks.clear();
    let mut cursor = Cursor {
        bytes,
        at: RECORD_HEADER,
    };
    let mut toplevel_xid = None;
    let mut main_length = 0usize;
    let mut payload = 0usize;
    while bytes.len() - cursor.at > payload {
        let id = cursor.u8().ok_or(short)?;
        match id {
            DATA_SHORT => {
                main_length = cursor.u8().ok_or(short)?.into();
                break;
            }
            DATA_LONG => {
                main_length = cursor.u32().ok_or(short)? as usize;
                break;
            }
            ORIGIN => {
                cursor.take(ORIGIN_SIZE).ok_or(short)?;
            }
            TOPLEVEL_XID => toplevel_xid = Some(cursor.u32().ok_or(short)?),
            id if id <= MAX_BLOCK_ID => {
                let previous = blocks.last();
                if previous.is_some_and(|block| block.id >= id) {
                    return Err(short);
                }
                let previous_file = previous.map(|block| block.file);
                let block = read_block_header(&mut cursor, id, previous_file).ok_or(short)?;
                payload += block.data.len();
                if let Some(image) = &block.image {
                    payload += image.bytes.len();
                }
                blocks.push(block);
            }
            _ => return Err(short),
        }
    }
    if bytes.len() - cursor.at != payload + main_length {
        return Err(short);
    }
    let main_start = place_payloads(blocks, cursor.at);
    record.xid = le_u32(&bytes[RECORD_XID]);
    record.info = bytes[RECORD_INFO];
    record.resource_manager = bytes[RECORD_RESOURCE_MANAGER];
    record.lsn = lsn;
    record.end = end;
    record.toplevel_xid = toplevel_xid;
    record.main = main_start..main_start + main_length;
    Ok(())
}

/// One `XLogRecordBlockHeader`, with its image header and relation file
/// when it has them. Its payloads are placed once all headers are read.
fn read_block_header(
    cursor: &mut Cursor,
    id: u8,
    previous_file: Option<RelationFile>,
) -> Option<Block> {
    let flags = cursor.u8()?;
    let data_length = usize::from(cursor.u16()?);
    if (flags & HAS_DATA != 0) != (data_length != 0) {
        return None;
    }
    let image = if flags & HAS_IMAGE != 0 {
        Some(read_image_header(cursor)?)
    } else {
        None
    };
    let file = if flags & SAME_RELATION != 0 {
        previous_file?
    } else {
        RelationFile {
            tablespace: cursor.u32()?,
            database: cursor.u32()?,
            relation: cursor.u32()?,
        }
    };
    Some(Block {
        id,
        fork: flags & FORK_MASK,
        will_init: flags & WILL_INIT != 0,
        file,
        number: cursor.u32()?,
        image,
        data: 0..data_length,
    })
}

/// `XLogRecordBlockImageHeader`, and the hole's length when it is
/// compressed (`XLogRecordBlockCompressHeader`), checked as
/// `DecodeXLogRecord` checks them.
fn read_image_header(cursor: &mut Cursor) -> Option<Image> {
    let length = cursor.u16()?;
    let hole_offset = cursor.u16()?;
    let info = cursor.u8()?;
    let compression = match info & (IMAGE_PGLZ | IMAGE_LZ4 | IMAGE_ZSTD) {
        0 => None,
        IMAGE_PGLZ => Some(Compression::Pglz),
        IMAGE_LZ4 => Some(Compression::Lz4),
        IMAGE_ZSTD => Some(Compression::Zstd),
        _ => return None,
    };
    let has_hole = info & IMAGE_HAS_HOLE != 0;
    let hole_length = match (compression, has_hole) {
        (Some(_), true) => cursor.u16()?,
        (Some(_), false) => 0,
        (None, _) => BLOCK_SIZE_U16.wrapping_sub(length),
    };
    let valid = usize::from(length) <= BLOCK_SIZE
        && if has_hole {
            hole_offset != 0 && hole_length != 0 && length != BLOCK_SIZE_U16
        } else {
            hole_offset == 0
                && hole_length == 0
                && (compression.is_some() || length == BLOCK_SIZE_U16)
        }
        && (compression.is_none() || length != BLOCK_SIZE_U16);
    if !valid {
        return None;
    }
    Some(Image {
        hole_offset,
        hole_length,
        compression,
        apply: info & IMAGE_APPLY != 0,
        bytes: 0..usize::from(length),
    })
}

/// Moves each block's payload ranges to where they lie from `start`:
/// where the main data starts.
fn place_payloads(blocks: &mut [Block], start: usize) -> usize {
    // Payloads follow the headers in block order: image, then data.
    let mut offset = start;
    for block in blocks.iter_mut() {
        if let Some(image) = &mut block.image {
            image.bytes = offset..offset + image.bytes.len();
            offset = image.bytes.end;
        }
        block.data = offset..offset + block.data.len();
        offset = block.data.end;
    }
    offset
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, length: usize) -> Option<&[u8]> {
        let bytes = self.bytes.get(self.at..self.at.checked_add(length)?)?;
        self.at += length;
        Some(bytes)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(le_u16)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(le_u32)
    }
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The `u32` at `at`, if `bytes` reach that far.
pub(crate) fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let bytes = bytes.get(at..)?.first_chunk()?;
    Some(u32::from_le_bytes(*bytes))
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut array = [0; 8];
    array.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(array)
}

fn page_start(lsn: u64) -> u64 {
    lsn - lsn % PAGE_SIZE
}

fn align(lsn: u64) -> u64 {
    lsn.div_ceil(RECORD_ALIGNMENT) * RECORD_ALIGNMENT
}

/// The checksum a record carries: CRC-32C over the record after its
/// header, then over the header up to the checksum, as `XLogRecord` has it.
fn checksum(record: &[u8]) -> u32 {
    let before_checksum = &record[..RECORD_CHECKSUM.start];
    crc32c::crc32c_append(crc32c::crc32c(&record[RECORD_HEADER..]), before_checksum)
}

/// WAL Straum can't read. Any of these ends the stream, and Straum starts
/// over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalError {
    /// Bytes came for an LSN other than the one after the last.
    Gap { wanted: u64, got: u64 },
    Sizes {
        segment_size: u64,
        page_size: u64,
        expected_segment_size: u64,
    },
    /// The page or record at `lsn` has `problem`.
    At { lsn: u64, problem: &'static str },
}

const PAGE_ADDRESS: &str = "a page header that names another page";
const PAGE_FLAGS: &str = "a page header with unknown flags";
/// The WAL format changed in the stream.
const OTHER_FORMAT: &str = "a page in another WAL format";
const CONTINUATION: &str = "a page that doesn't carry on the record before it";
/// After a crash, PostgreSQL wrote over a record it never finished.
const OVERWRITTEN: &str = "a record written over one a crash cut off";
const LENGTH: &str = "a record length out of range";
const CHECKSUM: &str = "a record that fails its checksum";
const BLOCKS: &str = "a record whose block headers don't add up";

fn at(lsn: u64, problem: &'static str) -> WalError {
    WalError::At { lsn, problem }
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalError::Gap { wanted, got } => {
                write!(f, "WAL came at {}, not at {}", Lsn(*got), Lsn(*wanted))
            }
            WalError::Sizes {
                segment_size,
                page_size,
                expected_segment_size,
            } => write!(
                f,
                "WAL has {segment_size}-byte segments and {page_size}-byte pages; \
                 Straum expected {expected_segment_size} and {PAGE_SIZE}"
            ),
            WalError::At { lsn, problem } => write!(f, "WAL at {} has {problem}", Lsn(*lsn)),
        }
    }
}

impl std::error::Error for WalError {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const SEGMENT: u64 = 64 * 1024;
    const MAGIC: u16 = 0xD118;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut mixed = self.0;
            mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            mixed ^ (mixed >> 31)
        }

        fn below(&mut self, limit: u64) -> u64 {
            self.next() % limit
        }

        /// A number below `limit`, as the type a field holds.
        fn below_as<T: TryFrom<u64, Error: std::fmt::Debug>>(&mut self, limit: u64) -> T {
            T::try_from(self.below(limit)).unwrap()
        }

        fn bytes(&mut self, length: usize) -> Vec<u8> {
            (0..length).map(|_| self.below_as(256)).collect()
        }

        fn some_bytes(&mut self, least: u64, more: u64) -> Vec<u8> {
            let length = least + self.below(more);
            self.bytes(usize::try_from(length).unwrap())
        }
    }

    /// Where `lsn` lies in WAL laid out from `SEGMENT`.
    fn offset(lsn: u64) -> usize {
        usize::try_from(lsn - SEGMENT).unwrap()
    }

    /// A record as a test describes it, and what the reader must give back.
    #[derive(Debug, Clone)]
    pub(super) struct Spec {
        pub(super) xid: u32,
        pub(super) resource_manager: u8,
        pub(super) info: u8,
        pub(super) toplevel_xid: Option<u32>,
        pub(super) origin: bool,
        pub(super) blocks: Vec<BlockSpec>,
        pub(super) main: Vec<u8>,
    }

    #[derive(Debug, Clone)]
    pub(super) struct BlockSpec {
        pub(super) id: u8,
        pub(super) fork: u8,
        pub(super) will_init: bool,
        pub(super) file: RelationFile,
        pub(super) number: u32,
        pub(super) image: Option<ImageSpec>,
        pub(super) data: Vec<u8>,
    }

    #[derive(Debug, Clone)]
    pub(super) struct ImageSpec {
        pub(super) hole_offset: u16,
        pub(super) hole_length: u16,
        pub(super) compression: Option<Compression>,
        pub(super) apply: bool,
        pub(super) bytes: Vec<u8>,
    }

    /// The bytes `XLogRecordAssemble` would make of a spec.
    pub(super) fn assemble(spec: &Spec) -> Vec<u8> {
        let mut headers = Vec::new();
        let mut payload: Vec<u8> = Vec::new();
        if let Some(xid) = spec.toplevel_xid {
            headers.push(TOPLEVEL_XID);
            headers.extend(xid.to_le_bytes());
        }
        if spec.origin {
            headers.push(ORIGIN);
            headers.extend(7u16.to_le_bytes());
        }
        let mut last = None;
        for block in &spec.blocks {
            let same = last == Some(block.file);
            assemble_block(block, same, &mut headers, &mut payload);
            last = Some(block.file);
        }
        if !spec.main.is_empty() {
            if let Ok(length) = u8::try_from(spec.main.len()) {
                headers.push(DATA_SHORT);
                headers.push(length);
            } else {
                headers.push(DATA_LONG);
                headers.extend(u32::try_from(spec.main.len()).unwrap().to_le_bytes());
            }
            payload.extend(&spec.main);
        }
        let total = u32::try_from(RECORD_HEADER + headers.len() + payload.len()).unwrap();
        let mut record = Vec::new();
        record.extend(total.to_le_bytes());
        record.extend(spec.xid.to_le_bytes());
        record.extend(0u64.to_le_bytes());
        record.push(spec.info);
        record.push(spec.resource_manager);
        record.extend([0, 0]);
        record.extend([0; 4]);
        record.extend(headers);
        record.extend(payload);
        reseal(record)
    }

    /// A block's header into `headers`, its image and data into `payload`.
    /// `same` says it names the relation the block before it does.
    fn assemble_block(block: &BlockSpec, same: bool, headers: &mut Vec<u8>, payload: &mut Vec<u8>) {
        let mut flags = block.fork;
        if block.image.is_some() {
            flags |= HAS_IMAGE;
        }
        if !block.data.is_empty() {
            flags |= HAS_DATA;
        }
        if block.will_init {
            flags |= WILL_INIT;
        }
        if same {
            flags |= SAME_RELATION;
        }
        headers.push(block.id);
        headers.push(flags);
        headers.extend(u16::try_from(block.data.len()).unwrap().to_le_bytes());
        if let Some(image) = &block.image {
            headers.extend(u16::try_from(image.bytes.len()).unwrap().to_le_bytes());
            headers.extend(image.hole_offset.to_le_bytes());
            headers.push(image_info(image));
            if image.compression.is_some() && image.hole_length != 0 {
                headers.extend(image.hole_length.to_le_bytes());
            }
            payload.extend(&image.bytes);
        }
        if !same {
            headers.extend(block.file.tablespace.to_le_bytes());
            headers.extend(block.file.database.to_le_bytes());
            headers.extend(block.file.relation.to_le_bytes());
        }
        headers.extend(block.number.to_le_bytes());
        payload.extend(&block.data);
    }

    fn image_info(image: &ImageSpec) -> u8 {
        let mut info = 0;
        if image.hole_length != 0 {
            info |= IMAGE_HAS_HOLE;
        }
        if image.apply {
            info |= IMAGE_APPLY;
        }
        info | match image.compression {
            None => 0,
            Some(Compression::Pglz) => IMAGE_PGLZ,
            Some(Compression::Lz4) => IMAGE_LZ4,
            Some(Compression::Zstd) => IMAGE_ZSTD,
        }
    }

    /// WAL from `start`, a segment start, laid out as `CopyXLogRecordToWAL`
    /// does: a header on every page, records 8-aligned and split across
    /// pages, a switch record padding out its segment. Gives each record's
    /// LSN.
    fn lay_out(start: u64, records: &[Vec<u8>]) -> (Vec<u8>, Vec<u64>) {
        let mut wal = Vec::new();
        let mut lsns = Vec::new();
        let mut position = start;
        for record in records {
            if position.is_multiple_of(PAGE_SIZE) {
                position = write_page_header(&mut wal, position, 0);
            }
            lsns.push(position);
            let mut written = 0;
            while written < record.len() {
                if position.is_multiple_of(PAGE_SIZE) {
                    position = write_page_header(&mut wal, position, record.len() - written);
                }
                let room = usize::try_from(PAGE_SIZE - position % PAGE_SIZE).unwrap();
                let take = room.min(record.len() - written);
                wal.extend(&record[written..written + take]);
                written += take;
                position += take as u64;
            }
            let is_switch = record[RECORD_RESOURCE_MANAGER] == XLOG_RESOURCE_MANAGER
                && record[RECORD_INFO] & RESOURCE_MANAGER_INFO == XLOG_SWITCH;
            let next = if is_switch {
                position.div_ceil(SEGMENT) * SEGMENT
            } else {
                align(position)
            };
            while position < next {
                if position.is_multiple_of(PAGE_SIZE) {
                    position = write_page_header(&mut wal, position, 0);
                    continue;
                }
                wal.push(0);
                position += 1;
            }
        }
        (wal, lsns)
    }

    /// The page header at `position`, with `remaining` bytes of a record
    /// carrying on into the page: where the page's first record starts.
    fn write_page_header(wal: &mut Vec<u8>, position: u64, remaining: usize) -> u64 {
        let long = position.is_multiple_of(SEGMENT);
        let mut flags = BACKUP_REMOVABLE | if long { LONG_PAGE_HEADER } else { 0 };
        if remaining > 0 {
            flags |= FIRST_IS_CONTINUATION;
        }
        wal.extend(MAGIC.to_le_bytes());
        wal.extend(flags.to_le_bytes());
        wal.extend(1u32.to_le_bytes());
        wal.extend(position.to_le_bytes());
        wal.extend(u32::try_from(remaining).unwrap().to_le_bytes());
        wal.extend([0; 4]);
        if long {
            wal.extend(42u64.to_le_bytes());
            wal.extend(u32::try_from(SEGMENT).unwrap().to_le_bytes());
            wal.extend(u32::try_from(PAGE_SIZE).unwrap().to_le_bytes());
        }
        position + if long { LONG_HEADER } else { SHORT_HEADER }
    }

    fn file(rng: &mut Rng) -> RelationFile {
        RelationFile {
            tablespace: 1663,
            database: 5,
            relation: 16384 + rng.below_as::<u32>(3),
        }
    }

    fn random_image(rng: &mut Rng) -> Option<ImageSpec> {
        match rng.below(4) {
            0 => {
                let hole_offset = 24 + rng.below_as::<u16>(1000);
                let hole_length = 1 + rng.below_as::<u16>(6000);
                Some(ImageSpec {
                    hole_offset,
                    hole_length,
                    compression: None,
                    apply: rng.below(2) == 0,
                    bytes: rng.bytes(BLOCK_SIZE - usize::from(hole_length)),
                })
            }
            1 => Some(ImageSpec {
                hole_offset: 0,
                hole_length: 0,
                compression: None,
                apply: true,
                bytes: rng.bytes(BLOCK_SIZE),
            }),
            2 => {
                let compression = [Compression::Pglz, Compression::Lz4, Compression::Zstd]
                    [rng.below_as::<usize>(3)];
                let hole = rng.below(2) == 0;
                Some(ImageSpec {
                    hole_offset: if hole { 100 } else { 0 },
                    hole_length: if hole { 200 } else { 0 },
                    compression: Some(compression),
                    apply: true,
                    bytes: rng.some_bytes(1, 3000),
                })
            }
            _ => None,
        }
    }

    fn random_spec(rng: &mut Rng) -> Spec {
        let mut blocks = Vec::new();
        let mut id = 0u8;
        for _ in 0..rng.below(4) {
            id += rng.below_as::<u8>(3);
            if id > MAX_BLOCK_ID {
                break;
            }
            let image = random_image(rng);
            let data = rng.some_bytes(0, 300);
            blocks.push(BlockSpec {
                id,
                fork: rng.below_as(3),
                will_init: rng.below(5) == 0,
                file: file(rng),
                number: rng.below_as(1000),
                image,
                data,
            });
            id += 1;
        }
        let main_length = match rng.below(4) {
            0 => 0,
            1 => 300 + rng.below_as::<usize>(20_000),
            _ => rng.below_as(200),
        };
        Spec {
            xid: rng.below_as(1000),
            resource_manager: 1 + rng.below_as::<u8>(20),
            info: rng.below_as::<u8>(16) << 4,
            toplevel_xid: (rng.below(5) == 0).then(|| rng.below_as(1000)),
            origin: rng.below(10) == 0,
            blocks,
            main: rng.bytes(main_length),
        }
    }

    fn switch() -> Spec {
        Spec {
            xid: 0,
            resource_manager: XLOG_RESOURCE_MANAGER,
            info: XLOG_SWITCH,
            toplevel_xid: None,
            origin: false,
            blocks: Vec::new(),
            main: Vec::new(),
        }
    }

    fn check(record: &Record, spec: &Spec, lsn: u64) {
        assert_eq!(record.lsn, lsn);
        assert_eq!(record.xid, spec.xid);
        assert_eq!(record.resource_manager, spec.resource_manager);
        assert_eq!(record.info, spec.info);
        assert_eq!(record.toplevel_xid, spec.toplevel_xid);
        assert_eq!(record.main_data(), &spec.main[..]);
        assert_eq!(record.blocks.len(), spec.blocks.len());
        for (block, want) in record.blocks.iter().zip(&spec.blocks) {
            assert_eq!(block.id, want.id);
            assert_eq!(block.fork, want.fork);
            assert_eq!(block.will_init, want.will_init);
            assert_eq!(block.file, want.file);
            assert_eq!(block.number, want.number);
            assert_eq!(record.block_data(block), &want.data[..]);
            match (&block.image, &want.image) {
                (None, None) => {}
                (Some(image), Some(want)) => {
                    assert_eq!(image.hole_offset, want.hole_offset);
                    assert_eq!(image.hole_length, want.hole_length);
                    assert_eq!(image.compression, want.compression);
                    assert_eq!(image.apply, want.apply);
                    assert_eq!(record.image_bytes(image), &want.bytes[..]);
                }
                other => panic!("image mismatch: {other:?}"),
            }
        }
    }

    /// Reads `wal` from `start`, fed in chunks of random size, and gives
    /// every record that comes out.
    fn read(
        rng: &mut Rng,
        wal: &[u8],
        wal_start: u64,
        start: u64,
    ) -> Result<Vec<Record>, WalError> {
        let mut records = Records::new(start, SEGMENT);
        let skip = usize::try_from(page_start(start) - wal_start).unwrap();
        let mut out = Vec::new();
        let mut at = skip;
        while at < wal.len() {
            let take = (1 + rng.below_as::<usize>(20_000)).min(wal.len() - at);
            records.feed(records.wanted(), &wal[at..at + take])?;
            at += take;
            while let Some(record) = records.next_record()? {
                out.push(record.clone());
            }
        }
        Ok(out)
    }

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

    fn first_error(wal: &[u8], start: u64) -> WalError {
        let mut records = Records::new(start, SEGMENT);
        records.feed(SEGMENT, wal).unwrap();
        records.next_record().unwrap_err()
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

    /// Re-checksums `record` after a test edits it.
    fn reseal(mut record: Vec<u8>) -> Vec<u8> {
        let crc = checksum(&record);
        record[RECORD_CHECKSUM].copy_from_slice(&crc.to_le_bytes());
        record
    }

    fn decode_error(record: Vec<u8>) -> WalError {
        let (wal, lsns) = lay_out(SEGMENT, &[reseal(record)]);
        first_error(&wal, lsns[0])
    }

    fn plain_block(image: Option<ImageSpec>, data: Vec<u8>) -> Spec {
        Spec {
            xid: 9,
            resource_manager: 10,
            info: 0,
            toplevel_xid: None,
            origin: false,
            blocks: vec![BlockSpec {
                id: 0,
                fork: 0,
                will_init: false,
                file: RelationFile {
                    tablespace: 1663,
                    database: 5,
                    relation: 16384,
                },
                number: 1,
                image,
                data,
            }],
            main: vec![1, 2, 3],
        }
    }

    #[test]
    fn refuses_block_headers_that_do_not_add_up() {
        let at_flags = RECORD_HEADER + 1;
        let lsn = SEGMENT + LONG_HEADER;
        let expected = at(lsn, BLOCKS);

        // Data flagged but none counted, and data counted but not flagged.
        let mut record = assemble(&plain_block(None, vec![7; 4]));
        record[at_flags] &= !HAS_DATA;
        assert_eq!(decode_error(record), expected);
        let mut record = assemble(&plain_block(None, Vec::new()));
        record[at_flags] |= HAS_DATA;
        assert_eq!(decode_error(record), expected);

        // A block naming the same relation as a block before it, first.
        let mut record = assemble(&plain_block(None, vec![7; 4]));
        record[at_flags] |= SAME_RELATION;
        assert_eq!(decode_error(record), expected);

        // An unknown block id, and blocks out of order.
        let mut record = assemble(&plain_block(None, vec![7; 4]));
        record[RECORD_HEADER] = 100;
        assert_eq!(decode_error(record), expected);
        let mut spec = plain_block(None, vec![7; 4]);
        let mut second = spec.blocks[0].clone();
        second.id = 0;
        spec.blocks.push(second);
        assert_eq!(decode_error(assemble(&spec)), expected);

        // Two kinds of compression at once.
        let image = ImageSpec {
            hole_offset: 0,
            hole_length: 0,
            compression: Some(Compression::Lz4),
            apply: true,
            bytes: vec![1; 50],
        };
        let mut record = assemble(&plain_block(Some(image.clone()), Vec::new()));
        record[RECORD_HEADER + 8] |= IMAGE_PGLZ;
        assert_eq!(decode_error(record), expected);

        // A hole flagged on an uncompressed whole page.
        let whole = ImageSpec {
            compression: None,
            bytes: vec![1; BLOCK_SIZE],
            ..image.clone()
        };
        let mut record = assemble(&plain_block(Some(whole), Vec::new()));
        record[RECORD_HEADER + 8] |= IMAGE_HAS_HOLE;
        assert_eq!(decode_error(record), expected);

        // A short uncompressed image without a hole.
        let short = ImageSpec {
            compression: None,
            bytes: vec![1; 100],
            ..image
        };
        assert_eq!(
            decode_error(assemble(&plain_block(Some(short), Vec::new()))),
            expected
        );

        // More bytes than the headers account for.
        let mut record = assemble(&plain_block(None, vec![7; 4]));
        record.push(0);
        let total = u32::try_from(record.len()).unwrap();
        record[RECORD_LENGTH].copy_from_slice(&total.to_le_bytes());
        assert_eq!(decode_error(record), expected);
    }

    #[test]
    fn checksums_as_postgresql_does() {
        // Bytes 0 to 63 as a record, summed bit by bit with the reflected
        // Castagnoli polynomial 0x82F63B78: body first, then header.
        let record: Vec<u8> = (0..64).collect();
        assert_eq!(checksum(&record), 0xF711_C61A);
        let mut flipped = record.clone();
        flipped[RECORD_HEADER] ^= 1;
        assert_ne!(checksum(&flipped), 0xF711_C61A);
    }

    #[test]
    fn says_what_went_wrong() {
        assert_eq!(
            at(0x1_0000_0028, CHECKSUM).to_string(),
            "WAL at 1/28 has a record that fails its checksum"
        );
        for error in [
            WalError::Gap { wanted: 1, got: 2 },
            WalError::Sizes {
                segment_size: 1,
                page_size: 2,
                expected_segment_size: 3,
            },
        ] {
            assert!(error.to_string().starts_with("WAL "));
        }
    }
}
