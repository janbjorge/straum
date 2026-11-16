//! What the WAL tests share: a seeded random generator, records described
//! as plain structs (`Spec`) and assembled into the bytes PostgreSQL's
//! `XLogRecordAssemble` (`src/backend/access/transam/xloginsert.c`) would
//! write, WAL pages laid out around them as `CopyXLogRecordToWAL` lays
//! them, and small builders of rows and attributes. A test builds a
//! `Spec`, turns it into bytes, and checks what the reader, the heap or the
//! transactions make of them.

use std::ops::Range;

use straum_domain::FastSet;

use super::reader::{
    BACKUP_REMOVABLE, FIRST_IS_CONTINUATION, LONG_HEADER, LONG_PAGE_HEADER, PAGE_ADDRESS_FIELD,
    PAGE_BLOCK_SIZE, PAGE_FLAGS_FIELD, PAGE_MAGIC, PAGE_REMAINING, PAGE_SEGMENT_SIZE, SHORT_HEADER,
    page_start,
};
use super::record::{
    DATA_LONG, DATA_SHORT, HAS_DATA, HAS_IMAGE, IMAGE_APPLY, IMAGE_HAS_HOLE, IMAGE_LZ4, IMAGE_PGLZ,
    IMAGE_ZSTD, MAX_BLOCK_ID, ORIGIN, RECORD_CHECKSUM, RECORD_HEADER, RECORD_INFO, RECORD_LENGTH,
    RECORD_RESOURCE_MANAGER, RECORD_XID, SAME_RELATION, TOPLEVEL_XID, WILL_INIT, XLOG_SWITCH,
    align, checksum, decode_into,
};
use super::tuple::{Attribute, Kind};
use super::{
    BLOCK_SIZE, Block, BlockNumber, Compression, Oid, PAGE_SIZE, Problem, RESOURCE_MANAGER_HEAP,
    RESOURCE_MANAGER_INFO, RESOURCE_MANAGER_XLOG, Record, Records, RelationFile, TransactionId,
    WalError,
};

/// The segment size the tests lay WAL out in: small, so tests cross
/// segments often.
pub(crate) const SEGMENT: u64 = 64 * 1024;
/// An `xlp_magic`: PostgreSQL 18's.
pub(crate) const MAGIC: u16 = 0xD118;
/// `xlp_tli`: the timeline the page was written on.
const PAGE_TIMELINE: Range<usize> = 4..8;
/// `xlp_sysid` in the long header: the database system's identifier.
const PAGE_SYSTEM_ID: Range<usize> = 24..32;
/// `pg_default`'s OID, the tablespace a table is in unless told.
pub(crate) const DEFAULT_TABLESPACE: Oid = 1663;
/// `FirstNormalObjectId`: the first OID a user's object gets.
pub(crate) const FIRST_USER_OID: Oid = 16_384;
/// The database the test tables are in.
pub(crate) const DATABASE: Oid = 5;
/// `SizeOfPageHeaderData`: where a heap page's line pointers start, so
/// the earliest a page image's hole can start.
const HEAP_PAGE_HEADER: u16 = 24;
/// A test row's `t_hoff`: `SizeofHeapTupleHeader`, 23, rounded up to
/// `MAXALIGN`, so the body opens with one byte of padding and no null
/// bitmap.
pub(crate) const ROW_HEADER: u8 = 24;
/// The padding byte that opens such a row's body.
pub(crate) const ROW_PADDING: usize = 1;
/// The LSN of a record a test decodes on its own.
pub(crate) const TEST_LSN: u64 = 0x100_0028;
/// A `RepOriginId` for records that name a replication origin.
const ORIGIN_ID: u16 = 7;

/// A splitmix generator, so a failing seed replays.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    /// The next number of the sequence.
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }

    /// A number below `limit`.
    pub(crate) fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }

    /// A number below `limit`, for a length or an index.
    pub(crate) fn below_usize(&mut self, limit: usize) -> usize {
        usize::try_from(self.below(limit as u64)).unwrap()
    }

    /// A number below `limit`, for a one-byte field.
    pub(crate) fn below_u8(&mut self, limit: u8) -> u8 {
        u8::try_from(self.below(u64::from(limit))).unwrap()
    }

    /// A number below `limit`, for a two-byte field.
    pub(crate) fn below_u16(&mut self, limit: u16) -> u16 {
        u16::try_from(self.below(u64::from(limit))).unwrap()
    }

    /// A number below `limit`, for a four-byte field.
    pub(crate) fn below_u32(&mut self, limit: u32) -> u32 {
        u32::try_from(self.below(u64::from(limit))).unwrap()
    }

    /// `length` random bytes.
    pub(crate) fn bytes(&mut self, length: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(length);
        for _ in 0..length {
            bytes.push(u8::try_from(self.below(256)).unwrap());
        }
        bytes
    }

    /// At least `least` random bytes, and fewer than `least + more`.
    pub(crate) fn some_bytes(&mut self, least: u64, more: u64) -> Vec<u8> {
        let length = least + self.below(more);
        self.bytes(usize::try_from(length).unwrap())
    }
}

/// The relation file of the table most tests change.
pub(crate) const FILE: RelationFile = RelationFile {
    tablespace: DEFAULT_TABLESPACE,
    database: DATABASE,
    relation: FIRST_USER_OID,
};

/// An attribute of `length` bytes, aligned to `align`, read as `kind`.
pub(crate) fn attribute(length: i16, align: usize, kind: Kind) -> Attribute {
    Attribute {
        length,
        align,
        kind,
    }
}

/// `text` as a varlena with a one-byte header, as a short value lies in
/// a row: the length with the header, shifted up a bit, low bit set.
pub(crate) fn short_varlena(text: &str) -> Vec<u8> {
    let mut bytes = vec![u8::try_from((text.len() + 1) << 1 | 1).unwrap()];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// Where `lsn` lies in WAL laid out from `SEGMENT`.
pub(crate) fn offset(lsn: u64) -> usize {
    usize::try_from(lsn - SEGMENT).unwrap()
}

/// A record as a test describes it, and what the reader must give back.
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    pub(crate) xid: TransactionId,
    pub(crate) resource_manager: u8,
    pub(crate) info: u8,
    pub(crate) toplevel_xid: Option<TransactionId>,
    /// The record names a replication origin.
    pub(crate) origin: bool,
    pub(crate) blocks: Vec<BlockSpec>,
    pub(crate) main: Vec<u8>,
}

/// A block reference as a test describes it.
#[derive(Debug, Clone)]
pub(crate) struct BlockSpec {
    pub(crate) id: u8,
    pub(crate) fork: u8,
    pub(crate) will_init: bool,
    pub(crate) file: RelationFile,
    pub(crate) number: BlockNumber,
    pub(crate) image: Option<ImageSpec>,
    pub(crate) data: Vec<u8>,
}

/// A full-page image as a test describes it: its bytes as logged.
#[derive(Debug, Clone)]
pub(crate) struct ImageSpec {
    pub(crate) hole_offset: u16,
    pub(crate) hole_length: u16,
    pub(crate) compression: Option<Compression>,
    pub(crate) apply: bool,
    pub(crate) bytes: Vec<u8>,
}

/// The bytes `XLogRecordAssemble` would make of a spec.
pub(crate) fn assemble(spec: &Spec) -> Vec<u8> {
    let mut headers = Vec::new();
    let mut payload: Vec<u8> = Vec::new();
    if let Some(xid) = spec.toplevel_xid {
        headers.push(TOPLEVEL_XID);
        headers.extend(xid.to_le_bytes());
    }
    if spec.origin {
        headers.push(ORIGIN);
        headers.extend(ORIGIN_ID.to_le_bytes());
    }
    let mut previous_file = None;
    for block in &spec.blocks {
        let relation = if previous_file == Some(block.file) {
            Relation::Same
        } else {
            Relation::Named
        };
        assemble_block(block, relation, &mut headers, &mut payload);
        previous_file = Some(block.file);
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
    // `xl_prev` and the padding stay zero; the checksum comes last.
    let mut record = vec![0; RECORD_HEADER];
    record[RECORD_LENGTH].copy_from_slice(&total.to_le_bytes());
    record[RECORD_XID].copy_from_slice(&spec.xid.to_le_bytes());
    record[RECORD_INFO] = spec.info;
    record[RECORD_RESOURCE_MANAGER] = spec.resource_manager;
    record.extend(headers);
    record.extend(payload);
    reseal(record)
}

/// Whether a block header names its relation file or says it is the one
/// the block before it names (`BKPBLOCK_SAME_REL`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Relation {
    Named,
    Same,
}

/// A block's header into `headers`, its image and data into `payload`.
fn assemble_block(
    block: &BlockSpec,
    relation: Relation,
    headers: &mut Vec<u8>,
    payload: &mut Vec<u8>,
) {
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
    if relation == Relation::Same {
        flags |= SAME_RELATION;
    }
    headers.push(block.id);
    headers.push(flags);
    headers.extend(u16::try_from(block.data.len()).unwrap().to_le_bytes());
    if let Some(image) = &block.image {
        headers.extend(u16::try_from(image.bytes.len()).unwrap().to_le_bytes());
        headers.extend(image.hole_offset.to_le_bytes());
        headers.push(image_flags(image));
        if image.compression.is_some() && image.hole_length != 0 {
            headers.extend(image.hole_length.to_le_bytes());
        }
        payload.extend(&image.bytes);
    }
    if relation == Relation::Named {
        headers.extend(block.file.tablespace.to_le_bytes());
        headers.extend(block.file.database.to_le_bytes());
        headers.extend(block.file.relation.to_le_bytes());
    }
    headers.extend(block.number.to_le_bytes());
    payload.extend(&block.data);
}

/// `bimg_info` for an image.
fn image_flags(image: &ImageSpec) -> u8 {
    let mut flags = 0;
    if image.hole_length != 0 {
        flags |= IMAGE_HAS_HOLE;
    }
    if image.apply {
        flags |= IMAGE_APPLY;
    }
    match image.compression {
        None => {}
        Some(Compression::Pglz) => flags |= IMAGE_PGLZ,
        Some(Compression::Lz4) => flags |= IMAGE_LZ4,
        Some(Compression::Zstd) => flags |= IMAGE_ZSTD,
    }
    flags
}

/// WAL as `lay_out` writes it, and the LSN of each record in it.
pub(crate) struct LaidOut {
    pub(crate) wal: Vec<u8>,
    pub(crate) lsns: Vec<u64>,
}

/// WAL from `start`, a segment start, laid out as `CopyXLogRecordToWAL`
/// does: a header on every page, records 8-aligned and split across
/// pages, a switch record padding out its segment.
pub(crate) fn lay_out(start: u64, records: &[Vec<u8>]) -> LaidOut {
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
        let is_switch = record[RECORD_RESOURCE_MANAGER] == RESOURCE_MANAGER_XLOG
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
    LaidOut { wal, lsns }
}

/// The page header at `position`, with `remaining` bytes of a record
/// carrying on into the page: where the page's first record starts.
fn write_page_header(wal: &mut Vec<u8>, position: u64, remaining: usize) -> u64 {
    let long = position.is_multiple_of(SEGMENT);
    let mut flags = BACKUP_REMOVABLE;
    let mut length = SHORT_HEADER;
    if long {
        flags |= LONG_PAGE_HEADER;
        length = LONG_HEADER;
    }
    if remaining > 0 {
        flags |= FIRST_IS_CONTINUATION;
    }
    let timeline = 1u32;
    let system_id = 42u64;
    let mut header = vec![0; usize::try_from(length).unwrap()];
    header[PAGE_MAGIC].copy_from_slice(&MAGIC.to_le_bytes());
    header[PAGE_FLAGS_FIELD].copy_from_slice(&flags.to_le_bytes());
    header[PAGE_TIMELINE].copy_from_slice(&timeline.to_le_bytes());
    header[PAGE_ADDRESS_FIELD].copy_from_slice(&position.to_le_bytes());
    let remaining = u32::try_from(remaining).unwrap();
    header[PAGE_REMAINING].copy_from_slice(&remaining.to_le_bytes());
    if long {
        header[PAGE_SYSTEM_ID].copy_from_slice(&system_id.to_le_bytes());
        let segment_size = u32::try_from(SEGMENT).unwrap();
        header[PAGE_SEGMENT_SIZE].copy_from_slice(&segment_size.to_le_bytes());
        let page_size = u32::try_from(PAGE_SIZE).unwrap();
        header[PAGE_BLOCK_SIZE].copy_from_slice(&page_size.to_le_bytes());
    }
    wal.extend(header);
    position + length
}

/// One of the three test tables' files, at random.
fn random_file(rng: &mut Rng) -> RelationFile {
    RelationFile {
        tablespace: DEFAULT_TABLESPACE,
        database: DATABASE,
        relation: FIRST_USER_OID + rng.below_u32(3),
    }
}

/// A full-page image of any kind the reader takes, or none.
fn random_image(rng: &mut Rng) -> Option<ImageSpec> {
    match rng.below(4) {
        0 => {
            let hole_offset = HEAP_PAGE_HEADER + rng.below_u16(1000);
            let hole_length = 1 + rng.below_u16(6000);
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
            let compressions = [Compression::Pglz, Compression::Lz4, Compression::Zstd];
            let compression = compressions[rng.below_usize(3)];
            let has_hole = rng.below(2) == 0;
            let mut image = ImageSpec {
                hole_offset: 0,
                hole_length: 0,
                compression: Some(compression),
                apply: true,
                bytes: Vec::new(),
            };
            if has_hole {
                image.hole_offset = 100;
                image.hole_length = 200;
            }
            image.bytes = rng.some_bytes(1, 3000);
            Some(image)
        }
        _ => None,
    }
}

/// A record of any shape the reader takes: blocks with and without
/// images and data, main data short, long or none.
pub(crate) fn random_spec(rng: &mut Rng) -> Spec {
    let mut blocks = Vec::new();
    let mut id = 0u8;
    for _ in 0..rng.below(4) {
        id += rng.below_u8(3);
        if id > MAX_BLOCK_ID {
            break;
        }
        let image = random_image(rng);
        let data = rng.some_bytes(0, 300);
        blocks.push(BlockSpec {
            id,
            fork: rng.below_u8(3),
            will_init: rng.below(5) == 0,
            file: random_file(rng),
            number: rng.below_u32(1000),
            image,
            data,
        });
        id += 1;
    }
    let main_length = match rng.below(4) {
        0 => 0,
        1 => 300 + rng.below_usize(20_000),
        _ => rng.below_usize(200),
    };
    let xid = rng.below_u32(1000);
    let resource_manager = 1 + rng.below_u8(20);
    let info = rng.below_u8(16) << 4;
    let mut toplevel_xid = None;
    if rng.below(5) == 0 {
        toplevel_xid = Some(rng.below_u32(1000));
    }
    Spec {
        xid,
        resource_manager,
        info,
        toplevel_xid,
        origin: rng.below(10) == 0,
        blocks,
        main: rng.bytes(main_length),
    }
}

/// A switch record, which pads out the rest of its segment.
pub(crate) fn switch() -> Spec {
    Spec {
        xid: 0,
        resource_manager: RESOURCE_MANAGER_XLOG,
        info: XLOG_SWITCH,
        toplevel_xid: None,
        origin: false,
        blocks: Vec::new(),
        main: Vec::new(),
    }
}

/// Asserts that `record`, read at `lsn`, is what `spec` describes.
pub(crate) fn check(record: &Record, spec: &Spec, lsn: u64) {
    assert_eq!(record.lsn, lsn);
    assert_eq!(record.xid, spec.xid);
    assert_eq!(record.resource_manager, spec.resource_manager);
    assert_eq!(record.info, spec.info);
    assert_eq!(record.toplevel_xid, spec.toplevel_xid);
    assert_eq!(record.main_data(), &spec.main[..]);
    assert_eq!(record.blocks.len(), spec.blocks.len());
    for (index, block) in record.blocks.iter().enumerate() {
        check_block(record, block, &spec.blocks[index]);
    }
}

/// Asserts that `block` of `record` is what `want` describes.
fn check_block(record: &Record, block: &Block, want: &BlockSpec) {
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

/// Reads `wal`, which starts at `wal_start`, from the record at `start`,
/// fed in pieces of random size: every record that comes out.
pub(crate) fn read(
    rng: &mut Rng,
    wal: &[u8],
    wal_start: u64,
    start: u64,
) -> Result<Vec<Record>, WalError> {
    read_passing_by(rng, wal, wal_start, start, None)
}

/// As [`read`], passing by the records that name only pages of files
/// other than `files`, when it has some.
pub(crate) fn read_passing_by(
    rng: &mut Rng,
    wal: &[u8],
    wal_start: u64,
    start: u64,
    files: Option<FastSet<RelationFile>>,
) -> Result<Vec<Record>, WalError> {
    let mut records = Records::new(start, SEGMENT);
    if let Some(files) = files {
        records.pass_by_other_files(files);
    }
    let mut at = usize::try_from(page_start(start) - wal_start).unwrap();
    let mut read = Vec::new();
    while at < wal.len() {
        let take = (1 + rng.below_usize(20_000)).min(wal.len() - at);
        records.feed(records.wanted(), &wal[at..at + take])?;
        at += take;
        while let Some(record) = records.next_record()? {
            read.push(record.clone());
        }
    }
    Ok(read)
}

/// The error reading `wal`, a segment from `SEGMENT`, from `start` stops at.
pub(crate) fn first_error(wal: &[u8], start: u64) -> WalError {
    let mut records = Records::new(start, SEGMENT);
    records.feed(SEGMENT, wal).unwrap();
    records.next_record().unwrap_err()
}

/// The problem reading `wal` from `start` stops at, if it is a problem
/// at an LSN.
pub(crate) fn first_problem(wal: &[u8], start: u64) -> Option<Problem> {
    match first_error(wal, start) {
        WalError::At { problem, .. } => Some(problem),
        WalError::Gap { .. } | WalError::Sizes { .. } => None,
    }
}

/// Re-checksums `record` after a test edits it.
pub(crate) fn reseal(mut record: Vec<u8>) -> Vec<u8> {
    let crc = checksum(&record);
    record[RECORD_CHECKSUM].copy_from_slice(&crc.to_le_bytes());
    record
}

/// The error reading `record`, resealed, alone in a segment.
pub(crate) fn decode_error(record: Vec<u8>) -> WalError {
    let laid_out = lay_out(SEGMENT, &[reseal(record)]);
    first_error(&laid_out.wal, laid_out.lsns[0])
}

/// A heap record with one block reference, to `FILE`'s page 1.
pub(crate) fn plain_block(image: Option<ImageSpec>, data: Vec<u8>) -> Spec {
    Spec {
        xid: 9,
        resource_manager: RESOURCE_MANAGER_HEAP,
        info: 0,
        toplevel_xid: None,
        origin: false,
        blocks: vec![BlockSpec {
            id: 0,
            fork: 0,
            will_init: false,
            file: FILE,
            number: 1,
            image,
            data,
        }],
        main: vec![1, 2, 3],
    }
}

/// The record `spec` describes, decoded on its own at `TEST_LSN`.
pub(crate) fn decoded(spec: &Spec) -> Record {
    let bytes = assemble(spec);
    decode(TEST_LSN, TEST_LSN + bytes.len() as u64, bytes).unwrap()
}

/// `bytes`, a whole record, read as the record at `lsn` that ends at
/// `end`.
pub(crate) fn decode(lsn: u64, end: u64, bytes: Vec<u8>) -> Result<Record, WalError> {
    let mut record = Record::empty();
    record.bytes = bytes;
    decode_into(lsn, end, &mut record)?;
    Ok(record)
}
