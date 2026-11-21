//! Records and pages built byte for byte for the heap tests: page images
//! as `bufpage.h` lays them out, and each record's main data and block
//! data as the `heapam_xlog.h` structs have them.

use std::ops::Range;

use super::*;
use crate::wal::record::XLOG_FPI;
use crate::wal::test_support::ROW_PADDING;

/// `CheckPoint.redo`, the field a checkpoint record opens with.
pub(super) const CHECKPOINT_REDO: Range<usize> = 0..8;
/// The rest of `CheckPoint`, which Straum doesn't read.
const CHECKPOINT_REST: usize = 80;
/// The padding before a test row's columns.
pub(super) const PADDING: [u8; ROW_PADDING] = [0; ROW_PADDING];
/// A test row's `t_infomask2`: two columns, no flags.
const TWO_COLUMNS: u16 = 2;
/// `PruneReason` `PRUNE_VACUUM_SCAN`, the reason byte up to 18.
const REASON_VACUUM_SCAN: u8 = 1;
/// The second flags byte from 19: the visibility map was set.
const PRUNE_SETS_VISIBILITY: u8 = 0x01;
/// `XLHP_HAS_CONFLICT_HORIZON`: a transaction id follows the flags.
pub(super) const PRUNE_HAS_CONFLICT_HORIZON: u8 = 1 << 3;
/// A conflict horizon, a `TransactionId`, no test reads.
const HORIZON: [u8; 4] = [9; 4];
/// `xl_heap_prune.reason`, up to 18.
const PRUNE_REASON_AT: usize = 0;
/// The second byte of `xl_heap_prune.flags`, from 19.
const PRUNE_VISIBILITY_FLAGS_AT: usize = 1;
/// A byte that pads a bulk insert's row to its 2-byte boundary.
const ALIGNMENT_FILLER: u8 = 0xEE;

/// A row of `columns` after the padding.
pub(super) fn tuple(columns: &[u8]) -> HeapTuple {
    HeapTuple {
        infomask2: TWO_COLUMNS,
        infomask: 0,
        hoff: ROW_HEADER,
        body: [&PADDING[..], columns].concat().into(),
    }
}

/// A row and the slot it sits in on a test page.
pub(super) struct Placed {
    pub(super) slot: SlotNumber,
    pub(super) tuple: HeapTuple,
}

/// A heap page holding `rows` in their slots, as PostgreSQL lays it out.
pub(super) fn page_bytes(rows: &[Placed]) -> Vec<u8> {
    let mut page = vec![0; BLOCK_SIZE];
    let mut slots = 0;
    for row in rows {
        slots = slots.max(row.slot);
    }
    let lower = PAGE_HEADER + LINE_POINTER * usize::from(slots);
    page[PAGE_LOWER].copy_from_slice(&u16::try_from(lower).unwrap().to_le_bytes());
    let mut upper = BLOCK_SIZE;
    for Placed { slot, tuple } in rows {
        let length = TUPLE_HEADER + tuple.body.len();
        upper -= length;
        let header = &mut page[upper + TUPLE_LOGGED_HEADER..upper + TUPLE_HEADER];
        write_heap_header(header, tuple);
        page[upper + TUPLE_HEADER..upper + length].copy_from_slice(&tuple.body);
        let at = PAGE_HEADER + LINE_POINTER * usize::from(slot - 1);
        page[at..at + LINE_POINTER].copy_from_slice(&line_pointer(upper, length));
    }
    page
}

/// `tuple`'s `t_infomask2`, `t_infomask` and `t_hoff` into `header`, laid
/// out as `xl_heap_header`.
fn write_heap_header(header: &mut [u8], tuple: &HeapTuple) {
    header[HEADER_INFOMASK2].copy_from_slice(&tuple.infomask2.to_le_bytes());
    header[HEADER_INFOMASK].copy_from_slice(&tuple.infomask.to_le_bytes());
    header[HEADER_HOFF] = tuple.hoff;
}

/// A normal `ItemIdData` for the row at `offset`.
pub(super) fn line_pointer(offset: usize, length: usize) -> [u8; LINE_POINTER] {
    let offset = u32::try_from(offset).unwrap();
    let length = u32::try_from(length).unwrap();
    (offset | LP_NORMAL << LP_FLAGS_SHIFT | length << LP_LENGTH_SHIFT).to_le_bytes()
}

/// An uncompressed image of a whole page, without a hole.
pub(super) fn image(bytes: Vec<u8>) -> ImageSpec {
    ImageSpec {
        hole_offset: 0,
        hole_length: 0,
        compression: None,
        apply: true,
        bytes,
    }
}

pub(super) fn block(id: u8, number: BlockNumber, data: Vec<u8>, image: Option<ImageSpec>) -> BlockSpec {
    BlockSpec {
        id,
        fork: MAIN_FORK,
        will_init: false,
        file: FILE,
        number,
        image,
        data,
    }
}

/// A block whose page the record starts afresh.
pub(super) fn init(id: u8, number: BlockNumber, data: Vec<u8>) -> BlockSpec {
    BlockSpec {
        will_init: true,
        ..block(id, number, data, None)
    }
}

/// A block of page `number` holding only `image`.
pub(super) fn imaged(number: BlockNumber, image: ImageSpec) -> BlockSpec {
    block(MAIN_BLOCK, number, Vec::new(), Some(image))
}

pub(super) fn record(resource_manager: u8, info: u8, main: Vec<u8>, blocks: Vec<BlockSpec>) -> Record {
    decoded(&Spec {
        xid: 700,
        resource_manager,
        info,
        toplevel_xid: None,
        origin: false,
        blocks,
        main,
    })
}

/// A full-page image record of `blocks`.
pub(super) fn full_page_images(blocks: Vec<BlockSpec>) -> Record {
    record(
        RESOURCE_MANAGER_XLOG,
        XLOG_FPI,
        Vec::new(),
        blocks,
    )
}

/// `xl_heap_header` and the row from its null bitmap on.
pub(super) fn logged(tuple: &HeapTuple) -> Vec<u8> {
    let mut data = vec![0; HEAP_HEADER];
    write_heap_header(&mut data, tuple);
    data.extend_from_slice(&tuple.body);
    data
}

/// `xl_heap_insert`.
pub(super) fn insert_main(slot: SlotNumber, flags: u8) -> Vec<u8> {
    let mut main = vec![0; INSERT_SIZE];
    main[INSERT_SLOT].copy_from_slice(&slot.to_le_bytes());
    main[INSERT_FLAGS] = flags;
    main
}

/// `xl_heap_confirm`.
pub(super) fn confirm_main(slot: SlotNumber) -> Vec<u8> {
    let mut main = vec![0; CONFIRM_SIZE];
    main[CONFIRM_SLOT].copy_from_slice(&slot.to_le_bytes());
    main
}

/// `xl_heap_delete`.
pub(super) fn delete_main(slot: SlotNumber, flags: u8) -> Vec<u8> {
    let mut main = vec![0; DELETE_SIZE];
    main[DELETE_SLOT].copy_from_slice(&slot.to_le_bytes());
    main[DELETE_FLAGS] = flags;
    main
}

/// `xl_heap_update`.
pub(super) fn update_main(old: SlotNumber, flags: u8, new: SlotNumber) -> Vec<u8> {
    let mut main = vec![0; UPDATE_SIZE];
    main[UPDATE_OLD_SLOT].copy_from_slice(&old.to_le_bytes());
    main[UPDATE_FLAGS] = flags;
    main[UPDATE_NEW_SLOT].copy_from_slice(&new.to_le_bytes());
    main
}

/// `xl_heap_multi_insert` of `count` rows, with `listed` slots after it
/// unless the record starts its page afresh.
pub(super) fn multi_insert_main(count: u16, listed: &[SlotNumber]) -> Vec<u8> {
    let mut main = vec![0; MULTI_INSERT_SIZE];
    main[MULTI_INSERT_COUNT].copy_from_slice(&count.to_le_bytes());
    main.extend(slots_le(listed));
    main
}

/// `rows`, each an `xl_multi_insert_tuple` at a 2-byte boundary, then its
/// bytes from the null bitmap on.
pub(super) fn multi_insert_data(rows: &[HeapTuple]) -> Vec<u8> {
    let mut data = Vec::new();
    for row in rows {
        let aligned = data.len().next_multiple_of(MULTI_INSERT_TUPLE_ALIGNMENT);
        data.resize(aligned, ALIGNMENT_FILLER);
        let mut header = vec![0; MULTI_INSERT_TUPLE];
        let length = u16::try_from(row.body.len()).unwrap();
        header[MULTI_INSERT_TUPLE_LENGTH].copy_from_slice(&length.to_le_bytes());
        write_heap_header(&mut header[MULTI_INSERT_TUPLE_LENGTH.end..], row);
        data.extend(header);
        data.extend_from_slice(&row.body);
    }
    data
}

/// `xl_heap_prune` as PostgreSQL `major` lays it out, with a conflict
/// horizon after it.
pub(super) fn prune_main(major: u32, flags: u8) -> Vec<u8> {
    let mut main = vec![0; PRUNE_SIZE];
    if major >= PRUNE_FLAGS_FIRST_MAJOR {
        main[PRUNE_FLAGS_FIRST] = flags;
        main[PRUNE_VISIBILITY_FLAGS_AT] = PRUNE_SETS_VISIBILITY;
    } else {
        main[PRUNE_REASON_AT] = REASON_VACUUM_SCAN;
        main[PRUNE_FLAGS_AFTER_REASON] = flags;
    }
    if flags & PRUNE_HAS_CONFLICT_HORIZON != 0 {
        main.extend(HORIZON);
    }
    main
}

/// `xl_smgr_truncate` of `file` from page `first_gone`, with `flags`.
pub(super) fn truncate_main(file: RelationFile, first_gone: BlockNumber, flags: u32) -> Vec<u8> {
    let mut main = vec![0; TRUNCATE_SIZE];
    main[TRUNCATE_BLOCK].copy_from_slice(&first_gone.to_le_bytes());
    main[TRUNCATE_TABLESPACE].copy_from_slice(&file.tablespace.to_le_bytes());
    main[TRUNCATE_DATABASE].copy_from_slice(&file.database.to_le_bytes());
    main[TRUNCATE_RELATION].copy_from_slice(&file.relation.to_le_bytes());
    main[TRUNCATE_FLAGS].copy_from_slice(&flags.to_le_bytes());
    main
}

/// Slots as `OffsetNumber`s, little-endian.
pub(super) fn slots_le(list: &[SlotNumber]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for slot in list {
        bytes.extend(slot.to_le_bytes());
    }
    bytes
}

/// A prune list: its `ntargets`, then its slots.
pub(super) fn prune_list(count: u16, slots: &[SlotNumber]) -> Vec<u8> {
    let mut bytes = count.to_le_bytes().to_vec();
    bytes.extend(slots_le(slots));
    bytes
}

pub(super) fn heap(major: u32) -> Heap {
    Heap::new([FILE].into(), major)
}

pub(super) fn page_at(heap: &Heap, block: BlockNumber) -> &Page {
    heap.page(PageId { file: FILE, block }).unwrap()
}

pub(super) fn insert(file: RelationFile, new: HeapTuple) -> HeapChange {
    HeapChange::Insert { file, new }
}

pub(super) fn checkpoint(redo: u64) -> Record {
    let mut main = vec![0; CHECKPOINT_REDO.end];
    main[CHECKPOINT_REDO].copy_from_slice(&redo.to_le_bytes());
    main.extend([0; CHECKPOINT_REST]);
    record(
        RESOURCE_MANAGER_XLOG,
        XLOG_CHECKPOINT_ONLINE,
        main,
        Vec::new(),
    )
}
