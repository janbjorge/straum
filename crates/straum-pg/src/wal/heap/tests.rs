//! Tests of the heap's page state: records built byte for byte as
//! PostgreSQL writes them, applied to a [`Heap`], and the rows and changes
//! that come out. Pages are laid out as `bufpage.h` has them and main data
//! as the `heapam_xlog.h` structs, at the offsets `page.rs` and `edit.rs`
//! name.

mod build;

use std::ops::Range;

use super::super::BLOCK_SIZE;
use super::edit::*;
use super::page::*;
use super::*;
use crate::wal::test_support::{BlockSpec, FILE, ImageSpec, ROW_HEADER, Spec, TEST_LSN, decoded};
use crate::wal::{BlockNumber, Compression, Problem, Record, RelationFile, SlotNumber, WalError};
use build::*;

const OTHER: RelationFile = RelationFile {
    tablespace: 1663,
    database: 5,
    relation: 16_999,
};

/// The LSN every test record starts at.
const LSN: u64 = TEST_LSN;
/// `FSM_FORKNUM`: the free space map, a fork Straum leaves alone.
const FREE_SPACE_FORK: u8 = 1;
/// Every list a prune record may carry, and the conflict horizon.
const PRUNE_FLAGS: u8 = PRUNE_HAS_CONFLICT_HORIZON
    | PRUNE_HAS_FREEZE_PLANS
    | PRUNE_HAS_REDIRECTIONS
    | PRUNE_HAS_DEAD_ITEMS
    | PRUNE_HAS_NOW_UNUSED_ITEMS;
/// `xlhp_freeze_plans.nplans`.
const FREEZE_PLAN_COUNT: Range<usize> = 0..2;

/// A checkpoint lets go of the pages last changed by its redo point,
/// and keeps those changed after it, whose next change brings no
/// image.
#[test]
fn a_checkpoint_lets_go_of_pages_changed_before_its_redo() {
    let mut heap = heap(18);
    let row = tuple(b"ab");
    for number in 0..3 {
        let fresh = init(MAIN_BLOCK, number, logged(&row));
        heap.apply(&record(
            RESOURCE_MANAGER_HEAP,
            HEAP_INSERT | INIT_PAGE,
            insert_main(1, 0),
            vec![fresh],
        ))
        .unwrap();
    }
    // Every test record has one LSN; a page's is where its record ends.
    let end = page_at(&heap, 0).lsn;
    assert!(end > LSN);
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
        vec![0; CHECKPOINT_REDO.end / 2],
        Vec::new(),
    );
    assert!(heap.apply(&cut).is_err());
}

/// An insert onto a page whose image comes with it.
fn insert_with_image(heap: &mut Heap, first: &HeapTuple) {
    let page = page_bytes(&[Placed {
        slot: 1,
        tuple: first.clone(),
    }]);
    let record_with_image = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(1, 0),
        vec![imaged(0, image(page))],
    );
    assert_eq!(
        heap.apply(&record_with_image),
        Ok(vec![insert(FILE, first.clone())])
    );
}

/// An insert that logs its row.
fn insert_logged(heap: &mut Heap, second: &HeapTuple) {
    let logged_insert = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(2, 0),
        vec![block(MAIN_BLOCK, 0, logged(second), None)],
    );
    assert_eq!(
        heap.apply(&logged_insert),
        Ok(vec![insert(FILE, second.clone())])
    );
}

/// "first row," + "FIRST" + " long enough to share": a prefix of 10 and
/// a suffix of 21 come from the old row.
fn update_sharing_bytes(heap: &mut Heap, first: &HeapTuple, new: &HeapTuple) {
    let mut data = Vec::new();
    data.extend(10u16.to_le_bytes());
    data.extend(21u16.to_le_bytes());
    let mut changed = (*new).clone();
    changed.body = [&PADDING[..], b"FIRST"].concat().into();
    data.extend(logged(&changed));
    let update = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_HOT_UPDATE,
        update_main(1, UPDATE_PREFIX_FROM_OLD | UPDATE_SUFFIX_FROM_OLD, 3),
        vec![block(MAIN_BLOCK, 0, data, None)],
    );
    assert_eq!(
        heap.apply(&update),
        Ok(vec![HeapChange::Update {
            file: FILE,
            old: first.clone(),
            new: new.clone(),
        }])
    );
}

/// A delete names only the slot; the old row comes from the page.
fn delete_second(heap: &mut Heap, second: &HeapTuple) {
    let delete = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_DELETE,
        delete_main(2, 0),
        vec![block(MAIN_BLOCK, 0, Vec::new(), None)],
    );
    assert_eq!(
        heap.apply(&delete),
        Ok(vec![HeapChange::Delete {
            file: FILE,
            old: second.clone(),
        }])
    );
}

#[test]
fn follows_rows_through_inserts_updates_and_deletes() {
    let mut heap = heap(18);
    let first = tuple(b"first row, long enough to share");
    let second = tuple(b"second");
    let new = tuple(b"first row,FIRST long enough to share");
    insert_with_image(&mut heap, &first);
    insert_logged(&mut heap, &second);
    update_sharing_bytes(&mut heap, &first, &new);
    delete_second(&mut heap, &second);
    let rows: Vec<(SlotNumber, &HeapTuple)> = page_at(&heap, 0).rows().collect();
    assert_eq!(rows, vec![(1, &first), (2, &second), (3, &new)]);
}

#[test]
fn moves_a_row_to_another_page() {
    let mut heap = heap(18);
    let old = tuple(b"old");
    let new = tuple(b"new and longer");
    let old_page = page_bytes(&[Placed {
        slot: 1,
        tuple: old.clone(),
    }]);
    let update = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_UPDATE | INIT_PAGE,
        update_main(1, 0, 1),
        vec![
            init(MAIN_BLOCK, 7, logged(&new)),
            block(OLD_ROW_BLOCK, 3, Vec::new(), Some(image(old_page))),
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
    assert_eq!(page_at(&heap, 7).get(1), Some(&new));
}

#[test]
fn reads_a_bulk_insert() {
    let mut heap = heap(18);
    let rows = [tuple(b"a"), tuple(b"bb"), tuple(b"ccc")];
    let data = multi_insert_data(&rows);
    let fresh = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_MULTI_INSERT | INIT_PAGE,
        multi_insert_main(3, &[]),
        vec![init(MAIN_BLOCK, 0, data.clone())],
    );
    let mut inserts = Vec::new();
    for row in &rows {
        inserts.push(insert(FILE, row.clone()));
    }
    assert_eq!(heap.apply(&fresh), Ok(inserts.clone()));
    // Into slots 4 to 6 of the same page, listed.
    let more = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_MULTI_INSERT,
        multi_insert_main(3, &[4, 5, 6]),
        vec![block(MAIN_BLOCK, 0, data, None)],
    );
    assert_eq!(heap.apply(&more), Ok(inserts));
    assert_eq!(page_at(&heap, 0).rows().count(), 6);
}

#[test]
fn counts_a_speculative_insert_once_confirmed() {
    let mut heap = heap(18);
    let row = tuple(b"maybe");
    let speculative = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT | INIT_PAGE,
        insert_main(1, INSERT_IS_SPECULATIVE),
        vec![init(MAIN_BLOCK, 0, logged(&row))],
    );
    assert_eq!(heap.apply(&speculative), Ok(vec![]));
    let confirm = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_CONFIRM,
        confirm_main(1),
        vec![block(MAIN_BLOCK, 0, Vec::new(), None)],
    );
    assert_eq!(heap.apply(&confirm), Ok(vec![insert(FILE, row.clone())]));
    let taken_back = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_DELETE,
        delete_main(1, DELETE_IS_SUPER),
        vec![block(MAIN_BLOCK, 0, Vec::new(), None)],
    );
    assert_eq!(heap.apply(&taken_back), Ok(vec![]));
}

/// Five rows on page 0, slots 1 to 5.
fn five_rows(heap: &mut Heap) {
    let mut rows = Vec::new();
    for column in 0..5u8 {
        rows.push(tuple(&[column]));
    }
    let mut placed = Vec::new();
    let mut slot: SlotNumber = 0;
    for tuple in rows {
        slot += 1;
        placed.push(Placed { slot, tuple });
    }
    let images = full_page_images(vec![imaged(0, image(page_bytes(&placed)))]);
    assert_eq!(heap.apply(&images), Ok(vec![]));
}

fn slots(heap: &Heap) -> Vec<SlotNumber> {
    let mut slots = Vec::new();
    for (slot, _) in page_at(heap, 0).rows() {
        slots.push(slot);
    }
    slots
}

/// Two freeze plans, then redirected 1 to 2, 3 dead, 4 unused, then
/// the frozen slots.
fn prune_freeze_data() -> Vec<u8> {
    let mut data = vec![0; FREEZE_PLANS_HEADER];
    data[FREEZE_PLAN_COUNT].copy_from_slice(&2u16.to_le_bytes());
    data.extend([0x55; 2 * FREEZE_PLAN]);
    data.extend(prune_list(1, &[1, 2]));
    data.extend(prune_list(1, &[3]));
    data.extend(prune_list(1, &[4]));
    data.extend(slots_le(&[5, 5]));
    data
}

#[test]
fn prunes_as_17_and_19_log_it() {
    for major in [17, 19] {
        let mut heap = heap(major);
        five_rows(&mut heap);
        for info in [HEAP2_PRUNE, HEAP2_VACUUM, HEAP2_PRUNE_CLEANUP] {
            let prune = record(
                RESOURCE_MANAGER_HEAP2,
                info,
                prune_main(major, PRUNE_FLAGS),
                vec![block(MAIN_BLOCK, 0, prune_freeze_data(), None)],
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
        prune_main(17, 0),
        vec![block(MAIN_BLOCK, 0, Vec::new(), None)],
    );
    assert_eq!(heap.apply(&nothing), Ok(vec![]));
    assert_eq!(slots(&heap), vec![1, 2, 3, 4, 5]);
}

/// A truncate drops the pages it cuts off a tracked file; a truncate of
/// another file leaves every kept page.
#[test]
fn drops_pages_a_truncate_cuts_off() {
    let mut heap = heap(18);
    for number in 0..4 {
        let images = full_page_images(vec![imaged(number, image(page_bytes(&[])))]);
        heap.apply(&images).unwrap();
    }
    truncate(&mut heap, OTHER, 0);
    assert_eq!(kept_blocks(&heap), vec![0, 1, 2, 3]);
    truncate(&mut heap, FILE, 2);
    assert_eq!(kept_blocks(&heap), vec![0, 1]);
}

/// Applies a `VACUUM` truncate of `file` from page `first_gone`.
fn truncate(heap: &mut Heap, file: RelationFile, first_gone: BlockNumber) {
    let main = truncate_main(file, first_gone, SMGR_TRUNCATE_HEAP);
    let truncate = record(RESOURCE_MANAGER_STORAGE, SMGR_TRUNCATE, main, vec![]);
    assert_eq!(heap.apply(&truncate), Ok(vec![]));
}

/// The blocks of the kept pages, in order.
fn kept_blocks(heap: &Heap) -> Vec<BlockNumber> {
    let mut kept: Vec<BlockNumber> = Vec::new();
    for (id, _) in heap.pages() {
        kept.push(id.block);
    }
    kept.sort_unstable();
    kept
}

#[test]
fn leaves_other_relations_and_forks_alone() {
    let mut heap = heap(18);
    let row = tuple(b"x");
    let other = BlockSpec {
        file: OTHER,
        ..block(MAIN_BLOCK, 0, logged(&row), None)
    };
    let insert = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(1, 0),
        vec![other],
    );
    assert_eq!(heap.apply(&insert), Ok(vec![]));
    let page = page_bytes(&[Placed {
        slot: 1,
        tuple: row.clone(),
    }]);
    let free_space = BlockSpec {
        fork: FREE_SPACE_FORK,
        ..imaged(0, image(page))
    };
    heap.apply(&full_page_images(vec![free_space])).unwrap();
    assert_eq!(heap.pages().count(), 0);
}

/// The hole of a one-row page: from the end of its one line pointer to
/// short of the row, which sits at the end of the page.
const HOLE_OFFSET: u16 = 28;
const HOLE_LENGTH: u16 = 8000;
/// pglz: a control byte of 0 says the next eight items are literals.
const PGLZ_ALL_LITERALS: u8 = 0;
const PGLZ_ITEMS_PER_CONTROL: usize = 8;
/// LZ4: a token of 15 literals or more and no match; the length past
/// 15 follows in one byte.
const LZ4_LONG_LITERALS_TOKEN: u8 = 0xF0;
const LZ4_LONG_LITERALS: usize = 15;

/// `kept` as pglz writes it with no match found: all literals.
fn pglz_literals(kept: &[u8]) -> Vec<u8> {
    let mut packed = Vec::new();
    for chunk in kept.chunks(PGLZ_ITEMS_PER_CONTROL) {
        packed.push(PGLZ_ALL_LITERALS);
        packed.extend_from_slice(chunk);
    }
    packed
}

/// `kept` as one LZ4 sequence of literals.
fn lz4_literals(kept: &[u8]) -> Vec<u8> {
    let mut packed = vec![LZ4_LONG_LITERALS_TOKEN];
    packed.push(u8::try_from(kept.len() - LZ4_LONG_LITERALS).unwrap());
    packed.extend_from_slice(kept);
    packed
}

#[test]
fn restores_images_with_holes_and_compression() {
    let row = tuple(b"compressed");
    let page = page_bytes(&[Placed {
        slot: 1,
        tuple: row.clone(),
    }]);
    let hole_end = usize::from(HOLE_OFFSET + HOLE_LENGTH);
    let kept = [&page[..usize::from(HOLE_OFFSET)], &page[hole_end..]].concat();
    for (compression, bytes) in [
        (Compression::Pglz, pglz_literals(&kept)),
        (Compression::Lz4, lz4_literals(&kept)),
    ] {
        let mut heap = heap(18);
        let image = ImageSpec {
            hole_offset: HOLE_OFFSET,
            hole_length: HOLE_LENGTH,
            compression: Some(compression),
            apply: true,
            bytes,
        };
        let images = full_page_images(vec![imaged(0, image)]);
        assert_eq!(heap.apply(&images), Ok(vec![]), "{compression:?}");
        assert_eq!(page_at(&heap, 0).get(1), Some(&row), "{compression:?}");
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
        vec![block(MAIN_BLOCK, 0, logged(&row), None)],
    );
    assert_eq!(error(&mut heap, &no_page), Problem::PageWithoutImage);
    five_rows(&mut heap);
    let no_row = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_DELETE,
        delete_main(9, 0),
        vec![block(MAIN_BLOCK, 0, Vec::new(), None)],
    );
    assert_eq!(error(&mut heap, &no_row), Problem::MissingRow);
    let slot_zero = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(0, 0),
        vec![block(MAIN_BLOCK, 0, logged(&row), None)],
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
        vec![0; UPDATE_SIZE - 1],
        vec![block(MAIN_BLOCK, 0, Vec::new(), None)],
    );
    assert_eq!(error(&mut heap, &short), Problem::ShortHeapRecord);
    let headless = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(6, 0),
        vec![block(MAIN_BLOCK, 0, vec![0; HEAP_HEADER - 1], None)],
    );
    assert_eq!(error(&mut heap, &headless), Problem::ShortHeapRecord);
    let no_block = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(6, 0),
        vec![],
    );
    assert_eq!(error(&mut heap, &no_block), Problem::HeapRecordWithoutPage);
    // A prefix longer than the old row's columns.
    let mut too_long = 200u16.to_le_bytes().to_vec();
    too_long.extend(logged(&row));
    let misfit = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_UPDATE,
        update_main(1, UPDATE_PREFIX_FROM_OLD, 6),
        vec![block(MAIN_BLOCK, 0, too_long, None)],
    );
    assert_eq!(error(&mut heap, &misfit), Problem::UpdateMisfit);
}

#[test]
fn refuses_heap2_records_cut_short() {
    let mut heap = heap(18);
    five_rows(&mut heap);
    // One row, then a byte past it.
    let mut data = multi_insert_data(&[tuple(b"")]);
    data.push(9);
    let trailing = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_MULTI_INSERT,
        multi_insert_main(1, &[7]),
        vec![block(MAIN_BLOCK, 0, data, None)],
    );
    assert_eq!(error(&mut heap, &trailing), Problem::ShortHeapRecord);
    // Three dead slots counted, one there.
    let broken_prune = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_PRUNE,
        prune_main(17, PRUNE_HAS_DEAD_ITEMS),
        vec![block(MAIN_BLOCK, 0, prune_list(3, &[1]), None)],
    );
    assert_eq!(error(&mut heap, &broken_prune), Problem::ShortHeapRecord);
}

#[test]
fn refuses_page_images_it_can_not_read() {
    let mut heap = heap(18);
    five_rows(&mut heap);
    let row = tuple(b"x");
    let page = page_bytes(&[Placed {
        slot: 1,
        tuple: row.clone(),
    }]);
    let zstd = ImageSpec {
        compression: Some(Compression::Zstd),
        ..image(page[..100].to_vec())
    };
    let zstd = full_page_images(vec![imaged(0, zstd)]);
    assert_eq!(error(&mut heap, &zstd), Problem::ImageInZstd);
    let garbage = ImageSpec {
        compression: Some(Compression::Pglz),
        ..image(vec![0xFF, 1, 2])
    };
    let garbage = full_page_images(vec![imaged(0, garbage)]);
    assert_eq!(error(&mut heap, &garbage), Problem::ImageNotDecompressing);
    let first_pointer = PAGE_HEADER..PAGE_HEADER + LINE_POINTER;
    // A row that runs past the end of the page.
    let mut bad_pointer = page.clone();
    bad_pointer[first_pointer.clone()].copy_from_slice(&line_pointer(BLOCK_SIZE - 2, 100));
    let bad = full_page_images(vec![imaged(0, image(bad_pointer))]);
    assert_eq!(error(&mut heap, &bad), Problem::UnreadableImage);
    // A row shorter than its own header.
    let mut tiny = page;
    tiny[first_pointer].copy_from_slice(&line_pointer(100, TUPLE_HEADER - 1));
    let tiny = full_page_images(vec![imaged(0, image(tiny))]);
    assert_eq!(error(&mut heap, &tiny), Problem::UnreadableImage);
}

#[test]
fn says_what_went_wrong() {
    let error = at(0x1_0000_0028, Problem::RowInSlotZero);
    assert_eq!(error.to_string(), "WAL at 1/28 has a row in slot 0");
}

/// A page image that is all zeros, as a page PostgreSQL has just added,
/// holds no rows.
#[test]
fn reads_a_new_page_as_empty() {
    let mut heap = heap(18);
    let images = full_page_images(vec![imaged(0, image(vec![0; BLOCK_SIZE]))]);
    assert_eq!(heap.apply(&images), Ok(vec![]));
    assert_eq!(page_at(&heap, 0).rows().count(), 0);
}

/// An insert into a slot that holds a row puts the new row there, and
/// the bytes counted follow.
#[test]
fn puts_a_row_over_the_one_in_its_slot() {
    let mut heap = heap(18);
    five_rows(&mut heap);
    let row = tuple(b"in slot one");
    let over = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_INSERT,
        insert_main(1, 0),
        vec![block(MAIN_BLOCK, 0, logged(&row), None)],
    );
    assert_eq!(heap.apply(&over), Ok(vec![insert(FILE, row.clone())]));
    assert_eq!(page_at(&heap, 0).get(1), Some(&row));
    assert_eq!(heap.bytes(), heap.recount());
}

/// From 19 a visibility-map set is logged as a prune that removes nothing,
/// and without data checksums it brings no page image: it needs no page.
/// One that removes a row still does.
#[test]
fn a_prune_that_removes_nothing_needs_no_page() {
    let mut heap = heap(19);
    let nothing = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_PRUNE,
        prune_main(19, 0),
        vec![block(MAIN_BLOCK, 7, Vec::new(), None)],
    );
    assert_eq!(heap.apply(&nothing), Ok(vec![]));
    let some = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_PRUNE,
        prune_main(19, PRUNE_HAS_DEAD_ITEMS),
        vec![block(MAIN_BLOCK, 7, prune_list(1, &[1]), None)],
    );
    assert!(heap.apply(&some).is_err());
}

/// A prune that names slot 0, which no row has, and a truncate of
/// another fork leave every row.
#[test]
fn leaves_rows_a_prune_or_truncate_does_not_reach() {
    let mut heap = heap(17);
    five_rows(&mut heap);
    let slot_zero = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_PRUNE,
        prune_main(17, PRUNE_HAS_DEAD_ITEMS),
        vec![block(MAIN_BLOCK, 0, prune_list(1, &[0]), None)],
    );
    assert_eq!(heap.apply(&slot_zero), Ok(vec![]));
    let not_the_heap = 0;
    let truncate = record(
        RESOURCE_MANAGER_STORAGE,
        SMGR_TRUNCATE,
        truncate_main(FILE, 0, not_the_heap),
        Vec::new(),
    );
    assert_eq!(heap.apply(&truncate), Ok(vec![]));
    assert_eq!(slots(&heap), vec![1, 2, 3, 4, 5]);
}

/// A record of a kind Straum doesn't redo still sets the LSN of the
/// pages it names, and passes over pages Straum doesn't hold.
#[test]
fn passes_over_records_that_change_no_row() {
    // `XLOG_HEAP_LOCK`: a row lock, which changes no column.
    let heap_lock = 0x60;
    let mut heap = heap(18);
    five_rows(&mut heap);
    let lock = record(
        RESOURCE_MANAGER_HEAP,
        heap_lock,
        Vec::new(),
        vec![
            block(MAIN_BLOCK, 0, Vec::new(), None),
            block(1, 7, Vec::new(), None),
        ],
    );
    assert_eq!(heap.apply(&lock), Ok(vec![]));
    assert!(page_at(&heap, 0).lsn > LSN);
    assert!(
        heap.page(PageId {
            file: FILE,
            block: 7
        })
        .is_none()
    );
}

/// A confirm of another table's row, and a bulk insert its image already
/// holds, change nothing Straum must redo.
#[test]
fn redoes_only_what_it_must() {
    let mut heap = heap(18);
    let other = BlockSpec {
        file: OTHER,
        ..block(MAIN_BLOCK, 0, Vec::new(), None)
    };
    let confirm = record(
        RESOURCE_MANAGER_HEAP,
        HEAP_CONFIRM,
        confirm_main(1),
        vec![other],
    );
    assert_eq!(heap.apply(&confirm), Ok(vec![]));
    let rows = [tuple(b"a"), tuple(b"bb")];
    let mut placed = Vec::new();
    let mut inserts = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let slot = SlotNumber::try_from(index + 1).unwrap();
        placed.push(Placed {
            slot,
            tuple: row.clone(),
        });
        inserts.push(insert(FILE, row.clone()));
    }
    let imaged_insert = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_MULTI_INSERT,
        multi_insert_main(2, &[1, 2]),
        vec![block(
            MAIN_BLOCK,
            0,
            multi_insert_data(&rows),
            Some(image(page_bytes(&placed))),
        )],
    );
    assert_eq!(heap.apply(&imaged_insert), Ok(inserts));
}

/// A bulk insert that counts more rows than it lists is cut short.
#[test]
fn refuses_a_bulk_insert_that_lists_too_few_slots() {
    let mut heap = heap(18);
    five_rows(&mut heap);
    let short = record(
        RESOURCE_MANAGER_HEAP2,
        HEAP2_MULTI_INSERT,
        multi_insert_main(3, &[6]),
        vec![block(MAIN_BLOCK, 0, Vec::new(), None)],
    );
    assert_eq!(error(&mut heap, &short), Problem::ShortHeapRecord);
}
