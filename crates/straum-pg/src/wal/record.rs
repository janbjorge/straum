//! One WAL record, read from its bytes: the second step of the physical
//! stream, after `reader.rs` has put the record together from its pages
//! and before `heap/` and `xact.rs` read what it says. A record is a
//! header (`XLogRecord`), then a header per page it changes (its block
//! references, `XLogRecordBlockHeader`), then their payloads and the main
//! data. Layouts: `src/include/access/xlogrecord.h`, read as
//! `DecodeXLogRecord` in `src/backend/access/transam/xlogreader.c` does.
//! A `Record` keeps the bytes and ranges into them, so reading one copies
//! nothing.

use std::ops::Range;

use straum_domain::FastSet;

use super::heap::MAIN_FORK;
use super::{
    BLOCK_SIZE, BlockNumber, Oid, Problem, RESOURCE_MANAGER_INFO, RESOURCE_MANAGER_STANDBY,
    RESOURCE_MANAGER_STORAGE, RESOURCE_MANAGER_TRANSACTION, RESOURCE_MANAGER_XLOG, TransactionId,
    WalError, at, read_u16, read_u32,
};

/// An image header holds lengths within a page as `uint16`.
#[expect(clippy::cast_possible_truncation, reason = "BLCKSZ, 8192, fits a u16")]
pub(super) const BLOCK_SIZE_U16: u16 = BLOCK_SIZE as u16;

/// `SizeOfXLogRecord`, `XLogRecord`: `xl_tot_len`, `xl_xid`, `xl_prev`,
/// `xl_info`, `xl_rmid`, two bytes of padding, `xl_crc`.
pub(super) const RECORD_HEADER: usize = 24;
/// `xl_tot_len`: the whole record's length.
pub(super) const RECORD_LENGTH: Range<usize> = 0..4;
/// `xl_xid`: the transaction that wrote the record.
pub(super) const RECORD_XID: Range<usize> = 4..8;
/// `xl_info`: the record's kind and flags.
pub(super) const RECORD_INFO: usize = 16;
/// `xl_rmid`: the resource manager that wrote it.
pub(super) const RECORD_RESOURCE_MANAGER: usize = 17;
/// `xl_crc`: the record's checksum.
pub(super) const RECORD_CHECKSUM: Range<usize> = 20..24;
/// `MAXALIGN`: records start on 8-byte boundaries.
pub(super) const RECORD_ALIGNMENT: u64 = 8;
/// `XLR_BLOCK_ID_ORIGIN` is followed by a `RepOriginId`, a `uint16`.
pub(super) const ORIGIN_SIZE: usize = 2;

/// `XLOG_SWITCH`: the record ends its WAL segment.
pub(super) const XLOG_SWITCH: u8 = 0x40;

/// `XLR_BLOCK_ID_DATA_SHORT`: main data, its length in one byte.
pub(super) const DATA_SHORT: u8 = 255;
/// `XLR_BLOCK_ID_DATA_LONG`: main data, its length in four bytes.
pub(super) const DATA_LONG: u8 = 254;
/// `XLR_BLOCK_ID_ORIGIN`: the replication origin that wrote the record.
pub(super) const ORIGIN: u8 = 253;
/// `XLR_BLOCK_ID_TOPLEVEL_XID`: the top transaction of a subtransaction.
pub(super) const TOPLEVEL_XID: u8 = 252;
/// `XLR_MAX_BLOCK_ID`: ids up to it are block references.
pub(super) const MAX_BLOCK_ID: u8 = 32;

/// `BKPBLOCK_FORK_MASK`: which fork of the relation the page is in.
pub(super) const FORK_MASK: u8 = 0x0F;
/// `BKPBLOCK_HAS_IMAGE`: a full-page image follows.
pub(super) const HAS_IMAGE: u8 = 0x10;
/// `BKPBLOCK_HAS_DATA`: the record has data for the page.
pub(super) const HAS_DATA: u8 = 0x20;
/// `BKPBLOCK_WILL_INIT`: redo starts the page afresh.
pub(super) const WILL_INIT: u8 = 0x40;
/// `BKPBLOCK_SAME_REL`: the relation file is the previous block's.
pub(super) const SAME_RELATION: u8 = 0x80;

/// `BKPIMAGE_HAS_HOLE`: the image leaves out the page's empty middle.
pub(super) const IMAGE_HAS_HOLE: u8 = 0x01;
/// `BKPIMAGE_APPLY`: redo restores the page from the image.
pub(super) const IMAGE_APPLY: u8 = 0x02;
/// `BKPIMAGE_COMPRESS_PGLZ`.
pub(super) const IMAGE_PGLZ: u8 = 0x04;
/// `BKPIMAGE_COMPRESS_LZ4`.
pub(super) const IMAGE_LZ4: u8 = 0x08;
/// `BKPIMAGE_COMPRESS_ZSTD`.
pub(super) const IMAGE_ZSTD: u8 = 0x10;

/// One WAL record. Owns its bytes, since a record split across pages has
/// to be put back together anyway; `Records` lends out the one it reuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    /// Where the record starts in the WAL.
    pub(crate) lsn: u64,
    /// The end of the record, before the alignment to the next.
    pub(crate) end: u64,
    pub(crate) xid: TransactionId,
    pub(crate) resource_manager: u8,
    /// `xl_info`: the record kind within its resource manager, with its
    /// flag bits.
    pub(crate) info: u8,
    /// The top transaction, when `xid` is a subtransaction it names first.
    pub(crate) toplevel_xid: Option<TransactionId>,
    pub(crate) blocks: Vec<Block>,
    /// Where the main data lies in `bytes`.
    main: Range<usize>,
    /// The whole record, its header first.
    pub(super) bytes: Vec<u8>,
}

/// A block reference: a page the record changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Block {
    /// The block's id within the record, 0 to `XLR_MAX_BLOCK_ID`.
    pub(crate) id: u8,
    /// `ForkNumber`: 0 is the table's rows, the others its maps.
    pub(crate) fork: u8,
    /// Redo starts the page afresh, so no earlier state of it is needed.
    pub(crate) will_init: bool,
    pub(crate) file: RelationFile,
    pub(crate) number: BlockNumber,
    pub(crate) image: Option<Image>,
    /// Where the record's data for the page lies in its bytes.
    data: Range<usize>,
}

/// Where a relation's pages live: `RelFileLocator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RelationFile {
    pub(crate) tablespace: Oid,
    pub(crate) database: Oid,
    /// The file node, which is the table's OID until a rewrite gives it a
    /// new file.
    pub(crate) relation: Oid,
}

/// A full-page image, as logged: maybe compressed, without its hole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Image {
    /// Where on the page the left-out hole starts.
    pub(crate) hole_offset: u16,
    pub(crate) hole_length: u16,
    pub(crate) compression: Option<Compression>,
    /// Redo restores it; otherwise it is there only for consistency checks.
    pub(crate) apply: bool,
    /// Where the image's bytes lie in the record's bytes.
    bytes: Range<usize>,
}

/// How `wal_compression` packed a full-page image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    Pglz,
    Lz4,
    Zstd,
}

impl Record {
    /// A record with nothing in it yet, to read records into.
    pub(super) fn empty() -> Record {
        Record {
            lsn: 0,
            end: 0,
            xid: 0,
            resource_manager: 0,
            info: 0,
            toplevel_xid: None,
            blocks: Vec::new(),
            main: 0..0,
            bytes: Vec::new(),
        }
    }

    /// The record's main data, which its resource manager reads.
    pub(crate) fn main_data(&self) -> &[u8] {
        &self.bytes[self.main.clone()]
    }

    /// The record's data for `block`.
    pub(crate) fn block_data(&self, block: &Block) -> &[u8] {
        &self.bytes[block.data.clone()]
    }

    /// The bytes of `image`, as logged.
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
        self.resource_manager == RESOURCE_MANAGER_XLOG
            && self.info & RESOURCE_MANAGER_INFO == XLOG_SWITCH
    }
}

/// Reads `record`'s bytes, which start at `lsn` and end at `end`, into
/// the rest of it.
pub(super) fn decode_into(lsn: u64, end: u64, record: &mut Record) -> Result<(), WalError> {
    let short = at(lsn, Problem::Blocks);
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
                let mut previous_file = None;
                if let Some(previous) = blocks.last() {
                    if previous.id >= id {
                        return Err(short);
                    }
                    previous_file = Some(previous.file);
                }
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
    record.xid = read_u32(&bytes[RECORD_XID]);
    record.info = bytes[RECORD_INFO];
    record.resource_manager = bytes[RECORD_RESOURCE_MANAGER];
    record.lsn = lsn;
    record.end = end;
    record.toplevel_xid = toplevel_xid;
    record.main = main_start..main_start + main_length;
    Ok(())
}

/// Resource managers whose records the stream reads whatever pages they
/// name: checkpoints and switches, transaction endings, the standby
/// snapshot's running transactions, and relation files cut short.
const ALWAYS_READ: [u8; 4] = [
    RESOURCE_MANAGER_XLOG,
    RESOURCE_MANAGER_TRANSACTION,
    RESOURCE_MANAGER_STORAGE,
    RESOURCE_MANAGER_STANDBY,
];

/// What a reader of some tables' pages makes of a record from its first
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Interest {
    /// The record may matter: read it whole and check it.
    Read,
    /// It names only pages of other relation files, and its resource
    /// manager says nothing else Straum reads.
    PassBy,
    /// Its block headers run past `head`: read it whole to know.
    Unsure,
}

/// Whether a reader of `files`' rows needs the record of `total_length`
/// bytes whose first bytes are `head`, read from its block headers
/// before any byte is copied or checksummed.
pub(super) fn interest(
    head: &[u8],
    total_length: usize,
    files: &FastSet<RelationFile>,
) -> Interest {
    let Some(resource_manager) = head.get(RECORD_RESOURCE_MANAGER) else {
        return Interest::Unsure;
    };
    if ALWAYS_READ.contains(resource_manager) {
        return Interest::Read;
    }
    let mut cursor = Cursor {
        bytes: head,
        at: RECORD_HEADER,
    };
    let mut previous_file = None;
    // As `decode_into` walks: headers until the payloads they announce.
    let mut payload = 0usize;
    while total_length.saturating_sub(cursor.at) > payload {
        let Some(id) = cursor.u8() else {
            return Interest::Unsure;
        };
        match id {
            DATA_SHORT | DATA_LONG => return Interest::PassBy,
            ORIGIN if cursor.take(ORIGIN_SIZE).is_some() => {}
            TOPLEVEL_XID if cursor.u32().is_some() => {}
            id if id <= MAX_BLOCK_ID => {
                let Some(block) = read_block_header(&mut cursor, id, previous_file) else {
                    return Interest::Unsure;
                };
                if block.fork == MAIN_FORK && files.contains(&block.file) {
                    return Interest::Read;
                }
                payload += block.data.len();
                if let Some(image) = &block.image {
                    payload += image.bytes.len();
                }
                previous_file = Some(block.file);
            }
            _ => return Interest::Unsure,
        }
    }
    Interest::PassBy
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
    let mut image = None;
    if flags & HAS_IMAGE != 0 {
        image = Some(read_image_header(cursor)?);
    }
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
    let flags = cursor.u8()?;
    let compression = match flags & (IMAGE_PGLZ | IMAGE_LZ4 | IMAGE_ZSTD) {
        0 => None,
        IMAGE_PGLZ => Some(Compression::Pglz),
        IMAGE_LZ4 => Some(Compression::Lz4),
        IMAGE_ZSTD => Some(Compression::Zstd),
        _ => return None,
    };
    let has_hole = flags & IMAGE_HAS_HOLE != 0;
    let hole_length = match (compression, has_hole) {
        (Some(_), true) => cursor.u16()?,
        (Some(_), false) => 0,
        // An uncompressed image is the page less its hole.
        (None, _) => BLOCK_SIZE_U16.wrapping_sub(length),
    };
    let image = Image {
        hole_offset,
        hole_length,
        compression,
        apply: flags & IMAGE_APPLY != 0,
        bytes: 0..usize::from(length),
    };
    if !image_fits(&image, has_hole) {
        return None;
    }
    Some(image)
}

/// The checks `DecodeXLogRecord` makes of an image header: a hole is
/// somewhere and something, no hole is nowhere and nothing, and only an
/// uncompressed image without a hole is the whole page.
fn image_fits(image: &Image, has_hole: bool) -> bool {
    let length = image.bytes.len();
    if length > BLOCK_SIZE {
        return false;
    }
    let whole_page = length == BLOCK_SIZE;
    let compressed = image.compression.is_some();
    if compressed && whole_page {
        return false;
    }
    // A hole past the page's end would cut the page short, or past it.
    let hole_end = usize::from(image.hole_offset) + usize::from(image.hole_length);
    if hole_end > BLOCK_SIZE {
        return false;
    }
    if has_hole {
        image.hole_offset != 0 && image.hole_length != 0 && !whole_page
    } else {
        image.hole_offset == 0 && image.hole_length == 0 && (compressed || whole_page)
    }
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

/// Reads a record's headers front to back.
// It borrows the record's bytes (`'a`) and reads them in place: a cursor
// that owned them would copy every record.
struct Cursor<'a> {
    bytes: &'a [u8],
    /// Where the next read starts.
    at: usize,
}

impl Cursor<'_> {
    /// The next `length` bytes, if there are that many.
    fn take(&mut self, length: usize) -> Option<&[u8]> {
        let bytes = self.bytes.get(self.at..self.at.checked_add(length)?)?;
        self.at += length;
        Some(bytes)
    }

    fn u8(&mut self) -> Option<u8> {
        let bytes = self.take(size_of::<u8>())?;
        Some(bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        let bytes = self.take(size_of::<u16>())?;
        Some(read_u16(bytes))
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.take(size_of::<u32>())?;
        Some(read_u32(bytes))
    }
}

/// `lsn` rounded up to where a record may start.
pub(super) fn align(lsn: u64) -> u64 {
    lsn.div_ceil(RECORD_ALIGNMENT) * RECORD_ALIGNMENT
}

/// The checksum a record carries: CRC-32C over the record after its
/// header, then over the header up to the checksum, as `XLogRecord` has it.
pub(super) fn checksum(record: &[u8]) -> u32 {
    let before_checksum = &record[..RECORD_CHECKSUM.start];
    crc32c::crc32c_append(crc32c::crc32c(&record[RECORD_HEADER..]), before_checksum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::reader::LONG_HEADER;
    use crate::wal::test_support::*;

    /// The first block header's id, right after the record header.
    const FIRST_BLOCK_ID: usize = RECORD_HEADER;
    /// `fork_flags`, after the id in `XLogRecordBlockHeader`.
    const FIRST_BLOCK_FLAGS: usize = FIRST_BLOCK_ID + 1;
    /// `SizeOfXLogRecordBlockHeader`: `id`, `fork_flags`, `data_length`.
    const BLOCK_HEADER_SIZE: usize = 4;
    /// `bimg_info` in `XLogRecordBlockImageHeader`, after `length` and
    /// `hole_offset`.
    const IMAGE_FLAGS_OFFSET: usize = 4;
    /// `bimg_info` of the first block's image, which follows its header.
    const FIRST_IMAGE_FLAGS: usize = FIRST_BLOCK_ID + BLOCK_HEADER_SIZE + IMAGE_FLAGS_OFFSET;

    /// The error a lone test record at the start of a segment stops at.
    fn blocks_problem() -> WalError {
        at(SEGMENT + LONG_HEADER, Problem::Blocks)
    }

    /// A compressed image of 50 bytes, without a hole.
    fn compressed_image() -> ImageSpec {
        ImageSpec {
            hole_offset: 0,
            hole_length: 0,
            compression: Some(Compression::Lz4),
            apply: true,
            bytes: vec![1; 50],
        }
    }

    #[test]
    fn refuses_data_flags_that_do_not_match_lengths() {
        // Data flagged but none counted, and data counted but not flagged.
        let mut record = assemble(&plain_block(None, vec![7; 4]));
        record[FIRST_BLOCK_FLAGS] &= !HAS_DATA;
        assert_eq!(decode_error(record), blocks_problem());
        let mut record = assemble(&plain_block(None, Vec::new()));
        record[FIRST_BLOCK_FLAGS] |= HAS_DATA;
        assert_eq!(decode_error(record), blocks_problem());
    }

    #[test]
    fn refuses_a_first_block_that_names_the_relation_before_it() {
        let mut record = assemble(&plain_block(None, vec![7; 4]));
        record[FIRST_BLOCK_FLAGS] |= SAME_RELATION;
        assert_eq!(decode_error(record), blocks_problem());
    }

    #[test]
    fn refuses_unknown_and_unordered_block_ids() {
        // Past `XLR_MAX_BLOCK_ID`, short of the special ids.
        let unknown_id = 100;
        let mut record = assemble(&plain_block(None, vec![7; 4]));
        record[FIRST_BLOCK_ID] = unknown_id;
        assert_eq!(decode_error(record), blocks_problem());
        let mut spec = plain_block(None, vec![7; 4]);
        let mut second = spec.blocks[0].clone();
        second.id = 0;
        spec.blocks.push(second);
        assert_eq!(decode_error(assemble(&spec)), blocks_problem());
    }

    #[test]
    fn refuses_image_headers_that_do_not_fit() {
        // Two kinds of compression at once.
        let image = compressed_image();
        let mut record = assemble(&plain_block(Some(image.clone()), Vec::new()));
        record[FIRST_IMAGE_FLAGS] |= IMAGE_PGLZ;
        assert_eq!(decode_error(record), blocks_problem());
        // A hole flagged on an uncompressed whole page.
        let whole = ImageSpec {
            compression: None,
            bytes: vec![1; BLOCK_SIZE],
            ..image.clone()
        };
        let mut record = assemble(&plain_block(Some(whole), Vec::new()));
        record[FIRST_IMAGE_FLAGS] |= IMAGE_HAS_HOLE;
        assert_eq!(decode_error(record), blocks_problem());
        // A short uncompressed image without a hole.
        let short = ImageSpec {
            compression: None,
            bytes: vec![1; 100],
            ..image
        };
        let record = assemble(&plain_block(Some(short), Vec::new()));
        assert_eq!(decode_error(record), blocks_problem());
        // A compressed image whose hole runs past the page, and one that
        // ends at the page's end.
        for (hole_length, fits) in [(9_000, false), (8_093, false), (8_092, true)] {
            let holed = ImageSpec {
                hole_offset: 100,
                hole_length,
                ..compressed_image()
            };
            let record = assemble(&plain_block(Some(holed), Vec::new()));
            let decoded = decode(SEGMENT + LONG_HEADER, record.len() as u64, record);
            assert_eq!(decoded.is_ok(), fits, "{hole_length}");
        }
    }

    #[test]
    fn refuses_images_of_more_than_a_page() {
        // Compressed to a whole page: PostgreSQL would have stored it plain.
        let whole = ImageSpec {
            bytes: vec![1; BLOCK_SIZE],
            ..compressed_image()
        };
        let record = assemble(&plain_block(Some(whole), Vec::new()));
        assert_eq!(decode_error(record), blocks_problem());
        let past_a_page = ImageSpec {
            compression: None,
            bytes: vec![1; BLOCK_SIZE + 1],
            ..compressed_image()
        };
        let record = assemble(&plain_block(Some(past_a_page), Vec::new()));
        assert_eq!(decode_error(record), blocks_problem());
    }

    #[test]
    fn refuses_bytes_the_headers_do_not_account_for() {
        let mut record = assemble(&plain_block(None, vec![7; 4]));
        record.push(0);
        let total = u32::try_from(record.len()).unwrap();
        record[RECORD_LENGTH].copy_from_slice(&total.to_le_bytes());
        assert_eq!(decode_error(record), blocks_problem());
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
}
