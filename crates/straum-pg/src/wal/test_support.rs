//! Test support for the WAL modules.

use super::reader::*;
use super::record::*;
use super::*;

pub(crate) const SEGMENT: u64 = 64 * 1024;
pub(crate) const MAGIC: u16 = 0xD118;

pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }

    pub(crate) fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }

    /// A number below `limit`, as the type a field holds.
    pub(crate) fn below_as<T: TryFrom<u64, Error: std::fmt::Debug>>(&mut self, limit: u64) -> T {
        T::try_from(self.below(limit)).unwrap()
    }

    pub(crate) fn bytes(&mut self, length: usize) -> Vec<u8> {
        (0..length).map(|_| self.below_as(256)).collect()
    }

    pub(crate) fn some_bytes(&mut self, least: u64, more: u64) -> Vec<u8> {
        let length = least + self.below(more);
        self.bytes(usize::try_from(length).unwrap())
    }
}

/// Where `lsn` lies in WAL laid out from `SEGMENT`.
pub(crate) fn offset(lsn: u64) -> usize {
    usize::try_from(lsn - SEGMENT).unwrap()
}

/// A record as a test describes it, and what the reader must give back.
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    pub(crate) xid: u32,
    pub(crate) resource_manager: u8,
    pub(crate) info: u8,
    pub(crate) toplevel_xid: Option<u32>,
    pub(crate) origin: bool,
    pub(crate) blocks: Vec<BlockSpec>,
    pub(crate) main: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct BlockSpec {
    pub(crate) id: u8,
    pub(crate) fork: u8,
    pub(crate) will_init: bool,
    pub(crate) file: RelationFile,
    pub(crate) number: u32,
    pub(crate) image: Option<ImageSpec>,
    pub(crate) data: Vec<u8>,
}

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
pub(crate) fn assemble_block(
    block: &BlockSpec,
    same: bool,
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

pub(crate) fn image_info(image: &ImageSpec) -> u8 {
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
pub(crate) fn lay_out(start: u64, records: &[Vec<u8>]) -> (Vec<u8>, Vec<u64>) {
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
pub(crate) fn write_page_header(wal: &mut Vec<u8>, position: u64, remaining: usize) -> u64 {
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

pub(crate) fn file(rng: &mut Rng) -> RelationFile {
    RelationFile {
        tablespace: 1663,
        database: 5,
        relation: 16384 + rng.below_as::<u32>(3),
    }
}

pub(crate) fn random_image(rng: &mut Rng) -> Option<ImageSpec> {
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
            let compression =
                [Compression::Pglz, Compression::Lz4, Compression::Zstd][rng.below_as::<usize>(3)];
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

pub(crate) fn random_spec(rng: &mut Rng) -> Spec {
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

pub(crate) fn switch() -> Spec {
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

pub(crate) fn check(record: &Record, spec: &Spec, lsn: u64) {
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
pub(crate) fn read(
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

pub(crate) fn first_error(wal: &[u8], start: u64) -> WalError {
    let mut records = Records::new(start, SEGMENT);
    records.feed(SEGMENT, wal).unwrap();
    records.next_record().unwrap_err()
}

/// Re-checksums `record` after a test edits it.
pub(crate) fn reseal(mut record: Vec<u8>) -> Vec<u8> {
    let crc = checksum(&record);
    record[RECORD_CHECKSUM].copy_from_slice(&crc.to_le_bytes());
    record
}

pub(crate) fn decode_error(record: Vec<u8>) -> WalError {
    let (wal, lsns) = lay_out(SEGMENT, &[reseal(record)]);
    first_error(&wal, lsns[0])
}

pub(crate) fn plain_block(image: Option<ImageSpec>, data: Vec<u8>) -> Spec {
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

pub(crate) fn decode(lsn: u64, end: u64, bytes: Vec<u8>) -> Result<Record, WalError> {
    let mut record = Record::empty();
    record.bytes = bytes;
    decode_into(lsn, end, &mut record)?;
    Ok(record)
}
