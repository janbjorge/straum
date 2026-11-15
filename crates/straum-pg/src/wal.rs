//! Physical WAL as a physical slot streams it: 8 KiB pages, each opening
//! with a header, holding records that may run across pages. PostgreSQL
//! writes it in the server's byte order; Straum reads little-endian only,
//! which every platform PostgreSQL ships on for these majors uses.
//! Layouts: `src/include/access/xlog_internal.h` and `xlogrecord.h`,
//! read as `DecodeXLogRecord` in `src/backend/access/transam/xlogreader.c`
//! does.

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

/// `XLOG_BLCKSZ`. The long page header names it, and Straum refuses others.
pub(crate) const PAGE_SIZE: u64 = 8192;
/// `BLCKSZ`, the size of a table page and of a full-page image.
pub(crate) const BLOCK_SIZE: usize = 8192;
/// The bits of `xl_info` its resource manager owns; the low four are
/// `XLR_INFO_MASK`, the WAL insertion's own.
pub(crate) const RESOURCE_MANAGER_INFO: u8 = 0xF0;

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
    ValueTooBig,
    UnknownCompression,
    ValueNotDecompressing,
    WrongLength,
    UnlabelledEnum,
    UnreadableNumeric,
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
            Problem::ValueTooBig => "a value too big",
            Problem::UnknownCompression => "a value compressed in a way Straum can't read",
            Problem::ValueNotDecompressing => "a value that doesn't decompress",
            Problem::WrongLength => "a value of the wrong length for its type",
            Problem::UnlabelledEnum => "an enum value Straum has no label for",
            Problem::UnreadableNumeric => "a numeric Straum can't read",
            Problem::NotUtf8 => "text that isn't UTF-8",
        };
        formatter.write_str(text)
    }
}

fn at(lsn: u64, problem: Problem) -> WalError {
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
        (Problem::ValueTooBig, "a value too big"),
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
        (Problem::NotUtf8, "text that isn't UTF-8"),
    ];

    #[test]
    fn says_each_problem_in_words() {
        for (problem, text) in EVERY_PROBLEM {
            assert_eq!(problem.to_string(), text);
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
