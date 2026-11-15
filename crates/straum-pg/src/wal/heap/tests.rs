use super::super::BLOCK_SIZE;
use super::edit::*;
use super::page::*;
use super::*;
use crate::wal::test_support::decode;
use crate::wal::test_support::{BlockSpec, FILE, ImageSpec, Spec, assemble};
use crate::wal::{Compression, Problem, Record, RelationFile, WalError};

const OTHER: RelationFile = RelationFile {
    tablespace: 1663,
    database: 5,
    relation: 16_999,
};

fn tuple(columns: &[u8]) -> HeapTuple {
    HeapTuple {
        infomask2: 2,
        infomask: 0,
        hoff: 24,
        body: [&[0][..], columns].concat().into(),
    }
}

/// A heap page holding `rows` in its slots, as PostgreSQL lays it out.
fn page_bytes(rows: &[(u16, &HeapTuple)]) -> Vec<u8> {
    let mut page = vec![0; BLOCK_SIZE];
    let slots = rows.iter().map(|(offnum, _)| *offnum).max().unwrap_or(0);
    let lower = PAGE_HEADER + LINE_POINTER * usize::from(slots);
    page[PAGE_LOWER].copy_from_slice(&u16::try_from(lower).unwrap().to_le_bytes());
    let mut upper = BLOCK_SIZE;
    for (offnum, tuple) in rows {
        let length = TUPLE_HEADER + tuple.body.len();
        upper -= length;
        let header = upper + TUPLE_LOGGED_HEADER;
        page[header..header + 2].copy_from_slice(&tuple.infomask2.to_le_bytes());
        page[header + 2..header + 4].copy_from_slice(&tuple.infomask.to_le_bytes());
        page[header + 4] = tuple.hoff;
        page[upper + TUPLE_HEADER..upper + length].copy_from_slice(&tuple.body);
        let at = PAGE_HEADER + LINE_POINTER * usize::from(offnum - 1);
        page[at..at + LINE_POINTER].copy_from_slice(&line_pointer(upper, length));
    }
    page
}

/// A normal `ItemIdData` for the row at `offset`.
fn line_pointer(offset: usize, length: usize) -> [u8; LINE_POINTER] {
    let offset = u32::try_from(offset).unwrap();
    let length = u32::try_from(length).unwrap();
    (offset | LP_NORMAL << LP_FLAGS_SHIFT | length << LP_LENGTH_SHIFT).to_le_bytes()
}

fn image(bytes: Vec<u8>) -> ImageSpec {
    ImageSpec {
        hole_offset: 0,
        hole_length: 0,
        compression: None,
        apply: true,
        bytes,
    }
}

fn block(id: u8, number: u32, data: Vec<u8>, image: Option<ImageSpec>) -> BlockSpec {
    BlockSpec {
        id,
        fork: 0,
        will_init: false,
        file: FILE,
        number,
        image,
        data,
    }
}

fn record(resource_manager: u8, info: u8, main: Vec<u8>, blocks: Vec<BlockSpec>) -> Record {
    let spec = Spec {
        xid: 700,
        resource_manager,
        info,
        toplevel_xid: None,
        origin: false,
        blocks,
        main,
    };
    let bytes = assemble(&spec);
    decode(0x100_0028, 0x100_0028 + bytes.len() as u64, bytes).unwrap()
}

/// `xl_heap_header` and the tuple from its null bitmap on.
fn logged(tuple: &HeapTuple) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend(tuple.infomask2.to_le_bytes());
    data.extend(tuple.infomask.to_le_bytes());
    data.push(tuple.hoff);
    data.extend_from_slice(&tuple.body);
    data
}

fn insert_main(offnum: u16, flags: u8) -> Vec<u8> {
    [&offnum.to_le_bytes()[..], &[flags]].concat()
}

fn delete_main(offnum: u16, flags: u8) -> Vec<u8> {
    [&[0; 4][..], &offnum.to_le_bytes(), &[0, flags]].concat()
}

fn update_main(old: u16, flags: u8, new: u16) -> Vec<u8> {
    [
        &[0; 4][..],
        &old.to_le_bytes(),
        &[0, flags],
        &[0; 4],
        &new.to_le_bytes(),
    ]
    .concat()
}

fn heap(major: u32) -> Heap {
    Heap::new([FILE].into(), major)
}

fn at(heap: &Heap, block: u32) -> &Page {
    heap.page(PageId { file: FILE, block }).unwrap()
}

fn insert(file: RelationFile, new: HeapTuple) -> HeapChange {
    HeapChange::Insert { file, new }
}

fn checkpoint(redo: u64) -> Record {
    let mut main = redo.to_le_bytes().to_vec();
    main.extend([0; 80]);
    record(
        RESOURCE_MANAGER_XLOG,
        XLOG_CHECKPOINT_ONLINE,
        main,
        Vec::new(),
    )
}

/// A checkpoint lets go of the pages last changed by its redo point,
/// and keeps those changed after it, whose next change brings no
/// image.
#[test]
fn a_checkpoint_lets_go_of_pages_changed_before_its_redo() {
    let mut heap = heap(18);
    let row = tuple(b"ab");
    for number in 0..3 {
        let init = BlockSpec {
            will_init: true,
            ..block(0, number, logged(&row), None)
        };
        let main = [&1u16.to_le_bytes()[..], &[0]].concat();
        heap.apply(&record(
            RESOURCE_MANAGER_HEAP,
            HEAP_INSERT | INIT_PAGE,
            main,
            vec![init],
        ))
        .unwrap();
    }
    // Every test record has one LSN; a page's is where its record ends.
    let end = at(&heap, 0).lsn;
    assert!(end > 0x100_0028);
    assert!(heap.bytes() > 0);
    assert_eq!(heap.bytes(), heap.recount());
    heap.apply(&checkpoint(end - 8)).unwrap();
    assert_eq!(heap.pages().count(), 3);
    // A page whose LSN is the redo point goes: its next change is
    // logged whole.
    heap.apply(&checkpoint(end)).unwrap();
    assert_eq!((heap.pages().count(), heap.bytes()), (0, 0));
    let cut = record(
        RESOURCE_MANAGER_XLOG,
        XLOG_CHECKPOINT_SHUTDOWN,
        vec![0; 4],
        Vec::new(),
    );
    assert!(heap.apply(&cut).is_err());
}

#[test]
fn follows_rows_through_inserts_updates_and_deletes() {
    let mut heap = heap(18);
    let first = tuple(b"first row, long enough to share");
    let record_with_image = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(1, 0),
        vec![block(0, 0, vec![], Some(image(page_bytes(&[(1, &first)]))))],
    );
    assert_eq!(
        heap.apply(&record_with_image),
        Ok(vec![insert(FILE, first.clone())])
    );

    let second = tuple(b"second");
    let logged_insert = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(2, 0),
        vec![block(0, 0, logged(&second), None)],
    );
    assert_eq!(
        heap.apply(&logged_insert),
        Ok(vec![insert(FILE, second.clone())])
    );

    // "first row," + "FIRST" + " long enough to share": a prefix of 10
    // and a suffix of 21 come from the old row.
    let new = tuple(b"first row,FIRST long enough to share");
    let mut data = Vec::new();
    data.extend(10u16.to_le_bytes());
    data.extend(21u16.to_le_bytes());
    data.extend(new.infomask2.to_le_bytes());
    data.extend(new.infomask.to_le_bytes());
    data.push(new.hoff);
    data.push(0);
    data.extend(b"FIRST");
    let update = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_HOT_UPDATE,
        update_main(1, UPDATE_PREFIX_FROM_OLD | UPDATE_SUFFIX_FROM_OLD, 3),
        vec![block(0, 0, data, None)],
    );
    assert_eq!(
        heap.apply(&update),
        Ok(vec![HeapChange::Update {
            file: FILE,
            old: first.clone(),
            new: new.clone(),
        }])
    );

    let delete = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_DELETE,
        delete_main(2, 0),
        vec![block(0, 0, vec![], None)],
    );
    assert_eq!(
        heap.apply(&delete),
        Ok(vec![HeapChange::Delete {
            file: FILE,
            old: second.clone(),
        }])
    );
    let rows: Vec<(u16, &HeapTuple)> = at(&heap, 0).rows().collect();
    assert_eq!(rows, vec![(1, &first), (2, &second), (3, &new)]);
}

#[test]
fn moves_a_row_to_another_page() {
    let mut heap = heap(18);
    let old = tuple(b"old");
    let new = tuple(b"new and longer");
    let update = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_UPDATE | INIT_PAGE,
        update_main(1, 0, 1),
        vec![
            BlockSpec {
                will_init: true,
                ..block(0, 7, logged(&new), None)
            },
            block(1, 3, vec![], Some(image(page_bytes(&[(1, &old)])))),
        ],
    );
    assert_eq!(
        heap.apply(&update),
        Ok(vec![HeapChange::Update {
            file: FILE,
            old,
            new: new.clone(),
        }])
    );
    assert_eq!(at(&heap, 7).get(1), Some(&new));
}

#[test]
fn reads_a_bulk_insert() {
    let mut heap = heap(18);
    let rows = [tuple(b"a"), tuple(b"bb"), tuple(b"ccc")];
    let mut data = Vec::new();
    for row in &rows {
        if data.len() % 2 == 1 {
            data.push(0xEE);
        }
        data.extend(u16::try_from(row.body.len()).unwrap().to_le_bytes());
        data.extend(row.infomask2.to_le_bytes());
        data.extend(row.infomask.to_le_bytes());
        data.push(row.hoff);
        data.extend_from_slice(&row.body);
    }
    let main = [&[0, 0][..], &3u16.to_le_bytes()].concat();
    let fresh = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_MULTI_INSERT | INIT_PAGE,
        main.clone(),
        vec![BlockSpec {
            will_init: true,
            ..block(0, 0, data.clone(), None)
        }],
    );
    let inserts: Vec<HeapChange> = rows.iter().map(|row| insert(FILE, row.clone())).collect();
    assert_eq!(heap.apply(&fresh), Ok(inserts.clone()));
    // Into slots 4 to 6 of the same page, listed.
    let listed = [
        main,
        4u16.to_le_bytes().to_vec(),
        5u16.to_le_bytes().to_vec(),
        6u16.to_le_bytes().to_vec(),
    ]
    .concat();
    let more = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_MULTI_INSERT,
        listed,
        vec![block(0, 0, data, None)],
    );
    assert_eq!(heap.apply(&more), Ok(inserts));
    assert_eq!(at(&heap, 0).rows().count(), 6);
}

fn init(id: u8, number: u32, data: Vec<u8>) -> BlockSpec {
    BlockSpec {
        will_init: true,
        ..block(id, number, data, None)
    }
}

#[test]
fn counts_a_speculative_insert_once_confirmed() {
    let mut heap = heap(18);
    let row = tuple(b"maybe");
    let speculative = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT | INIT_PAGE,
        insert_main(1, INSERT_IS_SPECULATIVE),
        vec![init(0, 0, logged(&row))],
    );
    assert_eq!(heap.apply(&speculative), Ok(vec![]));
    let confirm = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_CONFIRM,
        1u16.to_le_bytes().to_vec(),
        vec![block(0, 0, vec![], None)],
    );
    assert_eq!(heap.apply(&confirm), Ok(vec![insert(FILE, row.clone())]));
    let taken_back = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_DELETE,
        delete_main(1, DELETE_IS_SUPER),
        vec![block(0, 0, vec![], None)],
    );
    assert_eq!(heap.apply(&taken_back), Ok(vec![]));
}

/// Five rows on page 0, slots 1 to 5.
fn five_rows(heap: &mut Heap) {
    let rows: Vec<HeapTuple> = (0..5u8).map(|i| tuple(&[i])).collect();
    let page: Vec<(u16, &HeapTuple)> = (1..).zip(&rows).collect();
    let fpi = record(
        RESOURCE_MANAGER_XLOG,
        0xB0,
        vec![],
        vec![block(0, 0, vec![], Some(image(page_bytes(&page))))],
    );
    assert_eq!(heap.apply(&fpi), Ok(vec![]));
}

fn offnums(list: &[u16]) -> Vec<u8> {
    list.iter().flat_map(|n| n.to_le_bytes()).collect()
}

fn slots(heap: &Heap) -> Vec<u16> {
    at(heap, 0).rows().map(|(offnum, _)| offnum).collect()
}

/// Two freeze plans, then redirected 1 to 2, 3 dead, 4 unused, then
/// the frozen slots.
fn prune_freeze_data() -> Vec<u8> {
    let mut data = vec![2, 0, 0, 0];
    data.extend([0x55; 2 * FREEZE_PLAN]);
    for list in [&[1u16, 1, 2][..], &[1, 3], &[1, 4]] {
        data.extend(offnums(list));
    }
    data.extend(offnums(&[5, 5]));
    data
}

const PRUNE_HAS_CONFLICT_HORIZON: u8 = 1 << 3;
const PRUNE_FLAGS: u8 = PRUNE_HAS_CONFLICT_HORIZON
    | PRUNE_HAS_FREEZE_PLANS
    | PRUNE_HAS_REDIRECTIONS
    | PRUNE_HAS_DEAD_ITEMS
    | PRUNE_HAS_NOW_UNUSED_ITEMS;

#[test]
fn prunes_as_17_and_19_log_it() {
    for (major, main) in [
        (17, vec![1, PRUNE_FLAGS, 9, 9, 9, 9]),
        (19, vec![PRUNE_FLAGS, 0x01, 9, 9, 9, 9]),
    ] {
        let mut heap = heap(major);
        five_rows(&mut heap);
        for info in [HEAP2_PRUNE, HEAP2_VACUUM, HEAP2_PRUNE_CLEANUP] {
            let prune = record(
                RESOURCE_MANAGER_HEAP2,
                info,
                main.clone(),
                vec![block(0, 0, prune_freeze_data(), None)],
            );
            assert_eq!(heap.apply(&prune), Ok(vec![]));
            assert_eq!(slots(&heap), vec![2, 5], "{major}");
        }
    }
    // Without lists, nothing goes.
    let mut heap = heap(17);
    five_rows(&mut heap);
    let nothing = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_PRUNE,
        vec![1, 0],
        vec![block(0, 0, vec![], None)],
    );
    assert_eq!(heap.apply(&nothing), Ok(vec![]));
    assert_eq!(slots(&heap), vec![1, 2, 3, 4, 5]);
}

#[test]
fn drops_pages_a_truncate_cuts_off() {
    let mut heap = heap(18);
    for number in 0..4 {
        let fpi = record(
            RESOURCE_MANAGER_XLOG,
            0xB0,
            vec![],
            vec![block(0, number, vec![], Some(image(page_bytes(&[]))))],
        );
        heap.apply(&fpi).unwrap();
    }
    let mut main = 2u32.to_le_bytes().to_vec();
    for part in [
        FILE.tablespace,
        FILE.database,
        FILE.relation,
        SMGR_TRUNCATE_HEAP,
    ] {
        main.extend(part.to_le_bytes());
    }
    heap.apply(&record(
        RESOURCE_MANAGER_STORAGE,
        SMGR_TRUNCATE,
        main,
        vec![],
    ))
    .unwrap();
    let mut kept: Vec<u32> = heap.pages().map(|(id, _)| id.block).collect();
    kept.sort_unstable();
    assert_eq!(kept, vec![0, 1]);
}

#[test]
fn leaves_other_relations_and_forks_alone() {
    let mut heap = heap(18);
    let row = tuple(b"x");
    let other = BlockSpec {
        file: OTHER,
        ..block(0, 0, logged(&row), None)
    };
    let insert = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(1, 0),
        vec![other],
    );
    assert_eq!(heap.apply(&insert), Ok(vec![]));
    let free_space = BlockSpec {
        fork: 1,
        ..block(0, 0, vec![], Some(image(page_bytes(&[(1, &row)]))))
    };
    heap.apply(&record(
        RESOURCE_MANAGER_XLOG,
        0xB0,
        vec![],
        vec![free_space],
    ))
    .unwrap();
    assert_eq!(heap.pages().count(), 0);
}

#[test]
fn restores_images_with_holes_and_compression() {
    let row = tuple(b"compressed");
    let page = page_bytes(&[(1, &row)]);
    // The hole is the empty middle of the page.
    let (hole_offset, hole_length) = (28u16, 8000u16);
    let kept = [&page[..28], &page[8028..]].concat();
    let pglz: Vec<u8> = kept
        .chunks(8)
        .flat_map(|chunk| [&[0u8][..], chunk].concat())
        .collect();
    let lz4 = [
        &[0xF0u8][..],
        &[255; 0],
        &[u8::try_from(kept.len() - 15).unwrap()],
        &kept,
    ]
    .concat();
    for (compression, bytes) in [
        (Some(Compression::Pglz), pglz),
        (Some(Compression::Lz4), lz4),
    ] {
        let mut heap = heap(18);
        let image = ImageSpec {
            hole_offset,
            hole_length,
            compression,
            apply: true,
            bytes,
        };
        let fpi = record(
            RESOURCE_MANAGER_XLOG,
            0xB0,
            vec![],
            vec![block(0, 0, vec![], Some(image))],
        );
        assert_eq!(heap.apply(&fpi), Ok(vec![]), "{compression:?}");
        assert_eq!(at(&heap, 0).get(1), Some(&row), "{compression:?}");
    }
}

fn error(heap: &mut Heap, record: &Record) -> Problem {
    match heap.apply(record) {
        Err(WalError::At { problem, .. }) => problem,
        other => panic!("{other:?}"),
    }
}

#[test]
fn refuses_changes_to_what_it_lacks() {
    let mut heap = heap(18);
    let row = tuple(b"x");
    let no_page = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(1, 0),
        vec![block(0, 0, logged(&row), None)],
    );
    assert_eq!(error(&mut heap, &no_page), Problem::PageWithoutImage);
    five_rows(&mut heap);
    let no_row = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_DELETE,
        delete_main(9, 0),
        vec![block(0, 0, vec![], None)],
    );
    assert_eq!(error(&mut heap, &no_row), Problem::MissingRow);
    let slot_zero = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(0, 0),
        vec![block(0, 0, logged(&row), None)],
    );
    assert_eq!(error(&mut heap, &slot_zero), Problem::RowInSlotZero);
}

#[test]
fn refuses_heap_records_cut_short() {
    let mut heap = heap(18);
    five_rows(&mut heap);
    let row = tuple(b"x");
    let short = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_UPDATE,
        vec![0; 13],
        vec![block(0, 0, vec![], None)],
    );
    assert_eq!(error(&mut heap, &short), Problem::ShortHeapRecord);
    let headless = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(6, 0),
        vec![block(0, 0, vec![0; 4], None)],
    );
    assert_eq!(error(&mut heap, &headless), Problem::ShortHeapRecord);
    let no_block = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(6, 0),
        vec![],
    );
    assert_eq!(error(&mut heap, &no_block), Problem::HeapRecordWithoutPage);
    let mut too_long = 200u16.to_le_bytes().to_vec();
    too_long.extend(logged(&row));
    let misfit = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_UPDATE,
        update_main(1, UPDATE_PREFIX_FROM_OLD, 6),
        vec![block(0, 0, too_long, None)],
    );
    assert_eq!(error(&mut heap, &misfit), Problem::UpdateMisfit);
    let trailing = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_MULTI_INSERT,
        [&[0, 0][..], &1u16.to_le_bytes(), &7u16.to_le_bytes()].concat(),
        vec![block(0, 0, vec![1, 0, 2, 0, 0, 0, 24, 0, 9], None)],
    );
    assert_eq!(error(&mut heap, &trailing), Problem::ShortHeapRecord);
    let broken_prune = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_PRUNE,
        vec![1, PRUNE_HAS_DEAD_ITEMS],
        vec![block(0, 0, vec![3, 0, 1, 0], None)],
    );
    assert_eq!(error(&mut heap, &broken_prune), Problem::ShortHeapRecord);
}

#[test]
fn refuses_page_images_it_can_not_read() {
    let mut heap = heap(18);
    five_rows(&mut heap);
    let page = page_bytes(&[(1, &tuple(b"x"))]);
    let zstd = ImageSpec {
        compression: Some(Compression::Zstd),
        ..image(page[..100].to_vec())
    };
    let zstd = record(
        RESOURCE_MANAGER_XLOG,
        0xB0,
        vec![],
        vec![block(0, 0, vec![], Some(zstd))],
    );
    assert_eq!(error(&mut heap, &zstd), Problem::ImageInZstd);
    let garbage = ImageSpec {
        compression: Some(Compression::Pglz),
        ..image(vec![0xFF, 1, 2])
    };
    let garbage = record(
        RESOURCE_MANAGER_XLOG,
        0xB0,
        vec![],
        vec![block(0, 0, vec![], Some(garbage))],
    );
    assert_eq!(error(&mut heap, &garbage), Problem::ImageNotDecompressing);
    let mut bad_pointer = page.clone();
    bad_pointer[PAGE_HEADER..PAGE_HEADER + LINE_POINTER].copy_from_slice(&line_pointer(8190, 100));
    let bad = record(
        RESOURCE_MANAGER_XLOG,
        0xB0,
        vec![],
        vec![block(0, 0, vec![], Some(image(bad_pointer)))],
    );
    assert_eq!(error(&mut heap, &bad), Problem::UnreadableImage);
    let mut tiny = page;
    tiny[PAGE_HEADER..PAGE_HEADER + LINE_POINTER].copy_from_slice(&line_pointer(100, 10));
    let tiny = record(
        RESOURCE_MANAGER_XLOG,
        0xB0,
        vec![],
        vec![block(0, 0, vec![], Some(image(tiny)))],
    );
    assert_eq!(error(&mut heap, &tiny), Problem::UnreadableImage);
}

#[test]
fn says_what_went_wrong() {
    let error = problem(0x1_0000_0028, Problem::RowInSlotZero);
    assert_eq!(error.to_string(), "WAL at 1/28 has a row in slot 0");
}
