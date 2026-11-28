//! The physical WAL reader. PostgreSQL's write-ahead log (WAL) holds every
//! change before it reaches a table's pages, and a physical slot streams it
//! as raw bytes. They flow: `reader.rs` cuts WAL pages into records,
//! `record.rs` reads a record's header and block references, `heap/`
//! applies heap records to the pages Straum keeps and gives row changes,
//! `xact.rs` and `transactions.rs` hold them until their transaction
//! commits, `messages.rs` and `tuple.rs` turn a commit into the decoder's
//! messages, and `pipeline.rs` runs the chain. This file holds what they share.

use std::fmt;

use straum_app::Lsn;

mod compress;
pub(crate) mod heap;
pub(crate) mod messages;
pub(crate) mod pipeline;
mod reader;
mod record;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod transactions;
pub(crate) mod tuple;
pub(crate) mod xact;

pub(crate) use reader::Records;
pub(crate) use record::{Block, Compression, Image, Record, RelationFile};

/// A transaction id, PostgreSQL's `TransactionId`. It is 32 bits and wraps
/// around, so which of two is later is [`transactions::follows`]'s to say.
pub(crate) type TransactionId = u32;
/// A page's number within its relation file, PostgreSQL's `BlockNumber`.
pub(crate) type BlockNumber = u32;
/// A row's slot on a heap page, counted from 1: PostgreSQL's
/// `OffsetNumber`. The page's line pointer for the slot says where the
/// row's bytes lie.
pub(crate) type SlotNumber = u16;
/// An object id, PostgreSQL's `Oid`: a table's, a database's, a
/// tablespace's or an enum label's.
pub(crate) type Oid = u32;

/// `XLOG_BLCKSZ`, the size of a WAL page. The long page header names it,
/// and Straum refuses others.
pub(crate) const PAGE_SIZE: u64 = 8192;
/// `BLCKSZ`, the size of a table page and of a full-page image.
pub(crate) const BLOCK_SIZE: usize = 8192;
/// The bits of `xl_info` its resource manager owns; the low four are
/// `XLR_INFO_MASK`, the WAL insertion's own.
pub(crate) const RESOURCE_MANAGER_INFO: u8 = 0xF0;

/// The resource managers whose records Straum reads, by `xl_rmid`. A
/// resource manager is the part of PostgreSQL that writes a kind of record
/// and replays it in recovery (`src/include/access/rmgrlist.h`).
/// `RM_XLOG_ID`: checkpoints, full-page images, segment switches.
pub(crate) const RESOURCE_MANAGER_XLOG: u8 = 0;
/// `RM_XACT_ID`: commits and aborts.
pub(crate) const RESOURCE_MANAGER_TRANSACTION: u8 = 1;
/// `RM_SMGR_ID`: the storage manager, which truncates relation files.
pub(crate) const RESOURCE_MANAGER_STORAGE: u8 = 2;
/// `RM_STANDBY_ID`: the transactions running, for standbys.
pub(crate) const RESOURCE_MANAGER_STANDBY: u8 = 8;
/// `RM_HEAP2_ID`: prunes, vacuums and bulk inserts on heap pages.
pub(crate) const RESOURCE_MANAGER_HEAP2: u8 = 9;
/// `RM_HEAP_ID`: inserts, updates and deletes of single rows.
pub(crate) const RESOURCE_MANAGER_HEAP: u8 = 10;

// PostgreSQL writes WAL in the server's byte order. Straum reads it
// little-endian only, which every platform PostgreSQL ships on for the
// majors Straum reads uses.

/// The first two bytes of `bytes`, little-endian.
fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

/// The first four bytes of `bytes`, little-endian.
fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The two bytes at `at`, little-endian, if `bytes` reach that far.
pub(crate) fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let bytes = bytes.get(at..)?.first_chunk()?;
    Some(u16::from_le_bytes(*bytes))
}

/// The four bytes at `at`, little-endian, if `bytes` reach that far.
pub(crate) fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let bytes = bytes.get(at..)?.first_chunk()?;
    Some(u32::from_le_bytes(*bytes))
}

/// The first eight bytes of `bytes`, little-endian.
fn read_u64(bytes: &[u8]) -> u64 {
    let mut array = [0; 8];
    array.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(array)
}

/// WAL Straum can't read. Any of these ends the stream, and Straum starts
/// over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalError {
    /// Bytes came for an LSN other than the one after the last.
    Gap { wanted: u64, got: u64 },
    /// A long page header names a segment or page size other than the
    /// server's.
    Sizes {
        segment_size: u64,
        page_size: u64,
        expected_segment_size: u64,
    },
    /// The page or record at `lsn` has `problem`.
    At { lsn: u64, problem: Problem },
}

/// What is wrong with the WAL at an LSN. Each says it in the words a
/// person reads after "WAL at <LSN> has".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Problem {
    PageAddress,
    PageFlags,
    /// The WAL format changed in the stream.
    OtherFormat,
    Continuation,
    /// After a crash, PostgreSQL wrote over a record it never finished.
    Overwritten,
    Length,
    Checksum,
    Blocks,
    UnreadableImage,
    ImageInZstd,
    ImageNotDecompressing,
    CheckpointWithoutRedo,
    ShortHeapRecord,
    HeapRecordWithoutPage,
    PageWithoutImage,
    MissingRow,
    RowInSlotZero,
    UpdateMisfit,
    ShortTransactionRecord,
    UntrackedTable,
    ShortRow,
    TooManyColumns,
    UnknownLength,
    CompressedWithoutSize,
    UnknownCompression,
    ValueNotDecompressing,
    WrongLength,
    UnlabelledEnum,
    UnreadableNumeric,
    UnreadableBool,
    NotUtf8,
}

impl fmt::Display for Problem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Problem::PageAddress => "a page header that names another page",
            Problem::PageFlags => "a page header with unknown flags",
            Problem::OtherFormat => "a page in another WAL format",
            Problem::Continuation => "a page that doesn't carry on the record before it",
            Problem::Overwritten => "a record written over one a crash cut off",
            Problem::Length => "a record length out of range",
            Problem::Checksum => "a record that fails its checksum",
            Problem::Blocks => "a record whose block headers don't add up",
            Problem::UnreadableImage => "a page image Straum can't read",
            Problem::ImageInZstd => {
                "a page image in zstd; Straum reads wal_compression off, pglz and lz4"
            }
            Problem::ImageNotDecompressing => "a page image that doesn't decompress",
            Problem::CheckpointWithoutRedo => "a checkpoint without its redo point",
            Problem::ShortHeapRecord => "a heap record shorter than its kind",
            Problem::HeapRecordWithoutPage => "a heap record without its page",
            Problem::PageWithoutImage => "a change to a page Straum has no image of",
            Problem::MissingRow => "a change to a row Straum doesn't have",
            Problem::RowInSlotZero => "a row in slot 0",
            Problem::UpdateMisfit => "an update that doesn't fit its old row",
            Problem::ShortTransactionRecord => "a transaction record shorter than its kind",
            Problem::UntrackedTable => "a change to a table Straum doesn't read",
            Problem::ShortRow => "a row shorter than its columns",
            Problem::TooManyColumns => "a row with more columns than its table",
            Problem::UnknownLength => "a column of unknown length",
            Problem::CompressedWithoutSize => "a compressed value without its size",
            Problem::UnknownCompression => "a value compressed in a way Straum can't read",
            Problem::ValueNotDecompressing => "a value that doesn't decompress",
            Problem::WrongLength => "a value of the wrong length for its type",
            Problem::UnlabelledEnum => "an enum value Straum has no label for",
            Problem::UnreadableNumeric => "a numeric Straum can't read",
            Problem::UnreadableBool => "a boolean Straum can't read",
            Problem::NotUtf8 => "text that isn't UTF-8",
        };
        formatter.write_str(text)
    }
}

/// The error of `problem` at `lsn`.
fn at(lsn: u64, problem: Problem) -> WalError {
    WalError::At { lsn, problem }
}

impl fmt::Display for WalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalError::Gap { wanted, got } => {
                write!(
                    formatter,
                    "WAL came at {}, not at {}",
                    Lsn(*got),
                    Lsn(*wanted)
                )
            }
            WalError::Sizes {
                segment_size,
                page_size,
                expected_segment_size,
            } => write!(
                formatter,
                "WAL has {segment_size}-byte segments and {page_size}-byte pages; \
                 Straum expected {expected_segment_size} and {PAGE_SIZE}"
            ),
            WalError::At { lsn, problem } => {
                write!(formatter, "WAL at {} has {problem}", Lsn(*lsn))
            }
        }
    }
}

impl std::error::Error for WalError {}

/// Where the last record before `insert` ends, which is where a stream
/// that has read all WAL so far stands. `pg_current_wal_insert_lsn()` at
/// the start of a page points past the page header, where no record
/// ends: the long header at the start of a `segment_bytes` segment, the
/// short one elsewhere. A record that ends on a page holds at least a
/// byte of it, aligned to 8, so it ends past either.
pub(crate) fn record_end(insert: Lsn, segment_bytes: u64) -> Lsn {
    let Lsn(at) = insert;
    let in_page = at % PAGE_SIZE;
    let header = if segment_bytes > 0 && at % segment_bytes == in_page {
        reader::LONG_HEADER
    } else {
        reader::SHORT_HEADER
    };
    if in_page == header {
        Lsn(at - in_page)
    } else {
        insert
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every problem, and the words it says.
    const EVERY_PROBLEM: [(Problem, &str); 31] = [
        (
            Problem::PageAddress,
            "a page header that names another page",
        ),
        (Problem::PageFlags, "a page header with unknown flags"),
        (Problem::OtherFormat, "a page in another WAL format"),
        (
            Problem::Continuation,
            "a page that doesn't carry on the record before it",
        ),
        (
            Problem::Overwritten,
            "a record written over one a crash cut off",
        ),
        (Problem::Length, "a record length out of range"),
        (Problem::Checksum, "a record that fails its checksum"),
        (Problem::Blocks, "a record whose block headers don't add up"),
        (Problem::UnreadableImage, "a page image Straum can't read"),
        (
            Problem::ImageInZstd,
            "a page image in zstd; Straum reads wal_compression off, pglz and lz4",
        ),
        (
            Problem::ImageNotDecompressing,
            "a page image that doesn't decompress",
        ),
        (
            Problem::CheckpointWithoutRedo,
            "a checkpoint without its redo point",
        ),
        (
            Problem::ShortHeapRecord,
            "a heap record shorter than its kind",
        ),
        (
            Problem::HeapRecordWithoutPage,
            "a heap record without its page",
        ),
        (
            Problem::PageWithoutImage,
            "a change to a page Straum has no image of",
        ),
        (Problem::MissingRow, "a change to a row Straum doesn't have"),
        (Problem::RowInSlotZero, "a row in slot 0"),
        (
            Problem::UpdateMisfit,
            "an update that doesn't fit its old row",
        ),
        (
            Problem::ShortTransactionRecord,
            "a transaction record shorter than its kind",
        ),
        (
            Problem::UntrackedTable,
            "a change to a table Straum doesn't read",
        ),
        (Problem::ShortRow, "a row shorter than its columns"),
        (
            Problem::TooManyColumns,
            "a row with more columns than its table",
        ),
        (Problem::UnknownLength, "a column of unknown length"),
        (
            Problem::CompressedWithoutSize,
            "a compressed value without its size",
        ),
        (
            Problem::UnknownCompression,
            "a value compressed in a way Straum can't read",
        ),
        (
            Problem::ValueNotDecompressing,
            "a value that doesn't decompress",
        ),
        (
            Problem::WrongLength,
            "a value of the wrong length for its type",
        ),
        (
            Problem::UnlabelledEnum,
            "an enum value Straum has no label for",
        ),
        (Problem::UnreadableNumeric, "a numeric Straum can't read"),
        (Problem::UnreadableBool, "a boolean Straum can't read"),
        (Problem::NotUtf8, "text that isn't UTF-8"),
    ];

    #[test]
    fn says_each_problem_in_words() {
        for (problem, text) in EVERY_PROBLEM {
            assert_eq!(problem.to_string(), text);
        }
    }

    #[test]
    fn a_record_ends_before_the_next_page_header() {
        let segment = 16 << 20;
        for (insert, end) in [
            (0x1_0000_0110, 0x1_0000_0110),
            (0x1_0000_2018, 0x1_0000_2000),
            (0x1_0100_0028, 0x1_0100_0000),
            // Past a short header a record can end at 40, and at 24 is a
            // long header's inside.
            (0x1_0000_2028, 0x1_0000_2028),
            (0x1_0100_0018, 0x1_0100_0018),
        ] {
            assert_eq!(record_end(Lsn(insert), segment), Lsn(end), "{insert:x}");
        }
    }

    #[test]
    fn says_what_went_wrong() {
        assert_eq!(
            at(0x1_0000_0028, Problem::Checksum).to_string(),
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
