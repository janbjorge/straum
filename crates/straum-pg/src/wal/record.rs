//! One WAL record and its block references.

use std::ops::Range;

use super::{
    BLOCK_SIZE, BlockNumber, Oid, Problem, RESOURCE_MANAGER_INFO, RESOURCE_MANAGER_XLOG,
    TransactionId, WalError, at, read_u16, read_u32,
};

/// An image header holds lengths within a page as `uint16`.
#[expect(clippy::cast_possible_truncation, reason = "BLCKSZ, 8192, fits a u16")]
pub(super) const BLOCK_SIZE_U16: u16 = BLOCK_SIZE as u16;

/// `SizeOfXLogRecord`, `XLogRecord`: `xl_tot_len`, `xl_xid`, `xl_prev`,
/// `xl_info`, `xl_rmid`, two bytes of padding, `xl_crc`.
pub(super) const RECORD_HEADER: usize = 24;
pub(super) const RECORD_LENGTH: Range<usize> = 0..4;
pub(super) const RECORD_XID: Range<usize> = 4..8;
pub(super) const RECORD_INFO: usize = 16;
pub(super) const RECORD_RESOURCE_MANAGER: usize = 17;
pub(super) const RECORD_CHECKSUM: Range<usize> = 20..24;
/// `MAXALIGN`: records start on 8-byte boundaries.
pub(super) const RECORD_ALIGNMENT: u64 = 8;
/// `XLR_BLOCK_ID_ORIGIN` is followed by a `RepOriginId`, a `uint16`.
pub(super) const ORIGIN_SIZE: usize = 2;

pub(super) const XLOG_SWITCH: u8 = 0x40;

pub(super) const DATA_SHORT: u8 = 255;
pub(super) const DATA_LONG: u8 = 254;
pub(super) const ORIGIN: u8 = 253;
pub(super) const TOPLEVEL_XID: u8 = 252;
pub(super) const MAX_BLOCK_ID: u8 = 32;

pub(super) const FORK_MASK: u8 = 0x0F;
pub(super) const HAS_IMAGE: u8 = 0x10;
pub(super) const HAS_DATA: u8 = 0x20;
pub(super) const WILL_INIT: u8 = 0x40;
pub(super) const SAME_RELATION: u8 = 0x80;

pub(super) const IMAGE_HAS_HOLE: u8 = 0x01;
pub(super) const IMAGE_APPLY: u8 = 0x02;
pub(super) const IMAGE_PGLZ: u8 = 0x04;
pub(super) const IMAGE_LZ4: u8 = 0x08;
pub(super) const IMAGE_ZSTD: u8 = 0x10;

/// One WAL record. Owns its bytes, since a record split across pages has
/// to be put back together anyway; `Records` lends out the one it reuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) lsn: u64,
    /// The end of the record, before the alignment to the next.
    pub(crate) end: u64,
    pub(crate) xid: TransactionId,
    pub(crate) resource_manager: u8,
    /// The record kind within its resource manager, with its flag bits.
    pub(crate) info: u8,
    /// The top transaction, when `xid` is a subtransaction it names first.
    pub(crate) toplevel_xid: Option<TransactionId>,
    pub(crate) blocks: Vec<Block>,
    main: Range<usize>,
    pub(super) bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Block {
    pub(crate) id: u8,
    pub(crate) fork: u8,
    /// Redo starts the page afresh, so no earlier state of it is needed.
    pub(crate) will_init: bool,
    pub(crate) file: RelationFile,
    pub(crate) number: BlockNumber,
    pub(crate) image: Option<Image>,
    data: Range<usize>,
}

/// Where a relation's pages live: `RelFileLocator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RelationFile {
    pub(crate) tablespace: Oid,
    pub(crate) database: Oid,
    pub(crate) relation: Oid,
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
        self.resource_manager == RESOURCE_MANAGER_XLOG
            && self.info & RESOURCE_MANAGER_INFO == XLOG_SWITCH
    }
}

/// Reads `record`'s bytes into the rest of it.
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
    record.xid = read_u32(&bytes[RECORD_XID]);
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
        self.take(2).map(read_u16)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(read_u32)
    }
}

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

    #[test]
    fn refuses_block_headers_that_do_not_add_up() {
        let at_flags = RECORD_HEADER + 1;
        let lsn = SEGMENT + LONG_HEADER;
        let expected = at(lsn, Problem::Blocks);

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
}
