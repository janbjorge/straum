//! Tests of the reader: WAL laid out byte for byte as each major writes it,
//! fed in pieces however the bytes come, and the records, waits and refusals
//! that come out. Seeded random tests cover every major in turn.

use super::*;
use crate::wal::heap::MAIN_FORK;
use crate::wal::record::{XLOG_FPI, XLOG_FPI_FOR_HINT};
use crate::wal::test_support::*;
use crate::wal::{
    Oid, RESOURCE_MANAGER_HEAP, RESOURCE_MANAGER_STANDBY, RESOURCE_MANAGER_STORAGE,
    RESOURCE_MANAGER_TRANSACTION, RESOURCE_MANAGER_XLOG, record_end,
};

/// The major a seeded test lays its WAL out as: each in turn, so the
/// seeds cover every major without running once per major.
fn major_of_seed(seed: u64) -> u32 {
    let index = usize::try_from(seed).unwrap() % MAJORS.len();
    MAJORS[index]
}

#[test]
fn reads_back_every_record_however_the_bytes_come() {
    for seed in 0..200 {
        let mut rng = Rng(seed);
        let major = major_of_seed(seed);
        let start = SEGMENT * (1 + rng.below(4));
        let mut specs = Vec::new();
        for _ in 0..=rng.below(40) {
            specs.push(random_spec(&mut rng));
        }
        if rng.below(3) == 0 {
            let at = rng.below_usize(specs.len());
            specs.insert(at, switch());
        }
        let assembled: Vec<Vec<u8>> = specs.iter().map(assemble).collect();
        let laid_out = lay_out(major, start, &assembled);
        let first = rng.below_usize(specs.len());
        let records = read(&mut rng, major, &laid_out.wal, start, laid_out.lsns[first]).unwrap();
        assert_eq!(records.len(), specs.len() - first, "seed {seed}");
        for (index, record) in records.iter().enumerate() {
            check(record, &specs[first + index], laid_out.lsns[first + index]);
        }
    }
}

#[test]
fn splits_a_record_header_across_pages() {
    let mut rng = Rng(1);
    // A main data length that leaves 8 bytes on the first page, so the
    // second record's header runs across the page boundary.
    let left_on_page = 8;
    let first_room = PAGE_SIZE - LONG_HEADER - left_on_page;
    let mut first = random_spec(&mut rng);
    first.blocks.clear();
    first.toplevel_xid = None;
    first.origin = false;
    // `XLR_BLOCK_ID_DATA_LONG`, then the main data's length.
    let main_data_header = size_of::<u8>() + size_of::<u32>();
    let main_length = usize::try_from(first_room).unwrap() - RECORD_HEADER - main_data_header;
    first.main = rng.bytes(main_length);
    let second = random_spec(&mut rng);
    let assembled = vec![assemble(&first), assemble(&second)];
    let laid_out = lay_out(MAJOR, SEGMENT, &assembled);
    assert_eq!(laid_out.lsns[1] % PAGE_SIZE, PAGE_SIZE - left_on_page);
    let records = read(&mut rng, MAJOR, &laid_out.wal, SEGMENT, laid_out.lsns[0]).unwrap();
    check(&records[1], &second, laid_out.lsns[1]);
}

#[test]
fn waits_for_the_rest_of_a_record() {
    let mut rng = Rng(2);
    let mut spec = random_spec(&mut rng);
    spec.main = rng.bytes(20_000);
    let LaidOut { wal, lsns } = lay_out(MAJOR, SEGMENT, &[assemble(&spec)]);
    let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
    records.feed(SEGMENT, &wal[..wal.len() - 1]).unwrap();
    assert_eq!(records.next_record(), Ok(None));
    records
        .feed(records.wanted(), &wal[wal.len() - 1..])
        .unwrap();
    check(records.next_record().unwrap().unwrap(), &spec, lsns[0]);
    assert_eq!(records.next_record(), Ok(None));
}

/// The records of `specs` laid out from the first segment, all fed
/// and read: where the reader has passed.
fn passed_after(specs: &[Spec]) -> (u64, LaidOut) {
    let assembled: Vec<Vec<u8>> = specs.iter().map(assemble).collect();
    let laid_out = lay_out(MAJOR, SEGMENT, &assembled);
    let mut records = Records::new(laid_out.lsns[0], SEGMENT, MAJOR);
    records.feed(SEGMENT, &laid_out.wal).unwrap();
    for _ in specs {
        assert!(records.next_record().unwrap().is_some());
    }
    assert_eq!(records.next_record(), Ok(None));
    (records.passed(), laid_out)
}

#[test]
fn passes_the_padding_a_switch_leaves() {
    let mut rng = Rng(5);
    let (passed, laid_out) = passed_after(&[random_spec(&mut rng), switch()]);
    let segment_end = 2 * SEGMENT;
    assert_eq!(SEGMENT + laid_out.wal.len() as u64, segment_end);
    assert_eq!(passed, segment_end);
    // An idle server's insert position after the switch, past the
    // next segment's long header.
    let inserted = straum_app::Lsn(segment_end + LONG_HEADER);
    assert_eq!(record_end(inserted, SEGMENT), straum_app::Lsn(passed));
}

#[test]
fn passes_no_further_than_a_record_mid_segment() {
    let mut rng = Rng(5);
    let (passed, laid_out) = passed_after(&[random_spec(&mut rng), random_spec(&mut rng)]);
    assert!(passed < 2 * SEGMENT);
    assert_eq!(passed, SEGMENT + laid_out.wal.len() as u64);
}

#[test]
fn gathers_a_record_fed_in_pieces_once() {
    let mut rng = Rng(4);
    let mut spec = random_spec(&mut rng);
    spec.main = rng.bytes(200_000);
    let LaidOut { wal, lsns } = lay_out(MAJOR, SEGMENT, &[assemble(&spec)]);
    let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
    let piece = 1000;
    let mut at = 0;
    while at + piece < wal.len() {
        records
            .feed(records.wanted(), &wal[at..at + piece])
            .unwrap();
        at += piece;
        assert_eq!(records.next_record(), Ok(None));
        let gathering = records.gathering.unwrap();
        assert!(records.wanted() - gathering.position < PAGE_SIZE, "at {at}");
    }
    records.feed(records.wanted(), &wal[at..]).unwrap();
    check(records.next_record().unwrap().unwrap(), &spec, lsns[0]);
    assert!(records.gathering.is_none());
}

#[test]
fn goes_on_with_a_record_whose_header_runs_across_pages() {
    let mut rng = Rng(5);
    let left_on_page = 8;
    let mut first = random_spec(&mut rng);
    first.blocks.clear();
    first.toplevel_xid = None;
    first.origin = false;
    let main_data_header = size_of::<u8>() + size_of::<u32>();
    let first_room = usize::try_from(PAGE_SIZE - LONG_HEADER - left_on_page).unwrap();
    first.main = rng.bytes(first_room - RECORD_HEADER - main_data_header);
    let mut second = random_spec(&mut rng);
    second.main = rng.bytes(30_000);
    let LaidOut { wal, lsns } = lay_out(MAJOR, SEGMENT, &[assemble(&first), assemble(&second)]);
    assert_eq!(lsns[1] % PAGE_SIZE, PAGE_SIZE - left_on_page);
    let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
    let mut read = Vec::new();
    for byte in wal.chunks(100) {
        records.feed(records.wanted(), byte).unwrap();
        while let Some(record) = records.next_record().unwrap() {
            read.push(record.clone());
        }
    }
    assert_eq!(read.len(), 2);
    check(&read[1], &second, lsns[1]);
}

/// Whether a stream reading `files` must read `record`: its resource
/// manager says something it reads, or it names one of their pages.
fn concerns(record: &Record, files: &FastSet<RelationFile>) -> bool {
    let always = [
        RESOURCE_MANAGER_XLOG,
        RESOURCE_MANAGER_TRANSACTION,
        RESOURCE_MANAGER_STORAGE,
        RESOURCE_MANAGER_STANDBY,
    ];
    let page_image = record.resource_manager == RESOURCE_MANAGER_XLOG
        && matches!(
            record.info & crate::wal::RESOURCE_MANAGER_INFO,
            XLOG_FPI | XLOG_FPI_FOR_HINT
        );
    if always.contains(&record.resource_manager) && !page_image {
        return true;
    }
    for block in &record.blocks {
        if block.fork == MAIN_FORK && files.contains(&block.file) {
            return true;
        }
    }
    false
}

#[test]
fn passes_by_exactly_the_records_of_other_files() {
    let mut passed_by = 0;
    for seed in 0..300 {
        let mut rng = Rng(seed);
        let mut chosen = Vec::new();
        for relation in 0..3 {
            if rng.below(2) == 0 {
                chosen.push(RelationFile {
                    relation: FIRST_USER_OID + relation,
                    ..FILE
                });
            }
        }
        let files: FastSet<RelationFile> = chosen.iter().copied().collect();
        let mut specs = Vec::new();
        for _ in 0..=rng.below(40) {
            specs.push(random_spec(&mut rng));
        }
        let assembled: Vec<Vec<u8>> = specs.iter().map(assemble).collect();
        let major = major_of_seed(seed);
        let laid_out = lay_out(major, SEGMENT, &assembled);
        let every = read(&mut rng, major, &laid_out.wal, SEGMENT, laid_out.lsns[0]).unwrap();
        let wanted = Some(chosen.into_iter().collect());
        let read = read_passing_by(
            &mut rng,
            major,
            &laid_out.wal,
            SEGMENT,
            laid_out.lsns[0],
            wanted,
        );
        let mut read = read.unwrap().into_iter().peekable();
        for record in every {
            let was_read = read.next_if_eq(&record).is_some();
            if concerns(&record, &files) {
                assert!(was_read, "seed {seed}: passed by {}", record.lsn);
            } else if headers_on_first_page(&record) {
                assert!(!was_read, "seed {seed}: read {}", record.lsn);
                passed_by += 1;
            }
        }
        assert_eq!(read.next(), None, "seed {seed}");
    }
    assert!(passed_by > 1000, "{passed_by}");
}

/// Whether `record`'s header and block headers end on the page it
/// starts on, where the reader judges it in place. One whose headers
/// run on is read whole.
fn headers_on_first_page(record: &Record) -> bool {
    let mut payload = record.main_data().len();
    for block in &record.blocks {
        payload += block.payload_length();
    }
    let headers = (record.total_length() - payload) as u64;
    record.lsn % PAGE_SIZE + headers <= PAGE_SIZE
}

/// A record of 100 bytes of main data naming one page of `relation`,
/// written by the heap, alone in its segment.
fn heap_record(relation: Oid) -> LaidOut {
    let mut spec = plain_block(None, vec![7; 20]);
    spec.resource_manager = RESOURCE_MANAGER_HEAP;
    spec.blocks[0].file.relation = relation;
    spec.main = vec![1; 100];
    lay_out(MAJOR, SEGMENT, &[assemble(&spec)])
}

/// The first record past `wal` to a reader of `files`.
fn first_read(wal: &[u8], start: u64, files: &[RelationFile]) -> Result<Option<Record>, WalError> {
    let mut records = Records::new(start, SEGMENT, MAJOR);
    records.pass_by_other_files(files.iter().copied().collect());
    records.feed(SEGMENT, wal).unwrap();
    Ok(records.next_record()?.cloned())
}

#[test]
fn checks_only_the_records_it_reads() {
    let LaidOut { mut wal, lsns } = heap_record(FIRST_USER_OID);
    // A byte of the block's data, past its headers, which the checksum
    // covers.
    wal[offset(lsns[0]) + RECORD_HEADER + 40] ^= 1;
    let theirs = RelationFile {
        relation: FIRST_USER_OID + 1,
        ..FILE
    };
    let ours = RelationFile {
        relation: FIRST_USER_OID,
        ..FILE
    };
    let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
    records.pass_by_other_files([theirs].into_iter().collect());
    records.feed(SEGMENT, &wal).unwrap();
    assert_eq!(records.next_record(), Ok(None));
    assert!(records.passed() > lsns[0]);
    assert_eq!(
        first_read(&wal, lsns[0], &[ours]),
        Err(at(lsns[0], Problem::Checksum))
    );
}

#[test]
fn checks_the_pages_a_record_passed_by_runs_across() {
    let mut spec = plain_block(None, vec![7; 20]);
    spec.resource_manager = RESOURCE_MANAGER_HEAP;
    spec.main = vec![1; 10_000];
    let LaidOut { mut wal, lsns } = lay_out(MAJOR, SEGMENT, &[assemble(&spec)]);
    let page = second_page();
    wal[page + PAGE_REMAINING.start] ^= 1;
    let theirs = RelationFile {
        relation: FIRST_USER_OID + 1,
        ..FILE
    };
    let page_lsn = SEGMENT + PAGE_SIZE;
    assert_eq!(
        first_read(&wal, lsns[0], &[theirs]),
        Err(at(page_lsn, Problem::Continuation))
    );
}

#[test]
fn holds_about_a_page_of_a_record_it_passes_by() {
    let mut passed_by = plain_block(None, vec![7; 20]);
    passed_by.main = vec![1; 100_000];
    let theirs = RelationFile {
        relation: FIRST_USER_OID + 1,
        ..FILE
    };
    let mut wanted = plain_block(None, vec![8; 20]);
    wanted.blocks[0].file = theirs;
    let LaidOut { wal, lsns } = lay_out(MAJOR, SEGMENT, &[assemble(&passed_by), assemble(&wanted)]);
    let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
    records.pass_by_other_files([theirs].into_iter().collect());
    let mut read = Vec::new();
    for page in wal.chunks(usize::try_from(PAGE_SIZE).unwrap()) {
        records.feed(records.wanted(), page).unwrap();
        while let Some(record) = records.next_record().unwrap() {
            read.push(record.clone());
        }
        let held = records.buffer.len() as u64;
        assert!(held <= 2 * PAGE_SIZE, "held {held} bytes");
    }
    assert_eq!(read.len(), 1);
    check(&read[0], &wanted, lsns[1]);
    assert_eq!(records.passed(), SEGMENT + wal.len() as u64);
}

/// One record of 100 bytes of main data, alone in its segment.
fn one_record() -> LaidOut {
    one_record_of(MAJOR)
}

/// As [`one_record`], laid out as PostgreSQL `major` does.
fn one_record_of(major: u32) -> LaidOut {
    let mut rng = Rng(3);
    let mut spec = random_spec(&mut rng);
    spec.main = rng.bytes(100);
    lay_out(major, SEGMENT, &[assemble(&spec)])
}

/// The first page's flags in `wal`.
fn first_page_flags(wal: &[u8]) -> u16 {
    read_u16(&wal[PAGE_FLAGS_FIELD])
}

/// `wal` with `flag` also set on its first page.
fn set_first_page_flag(wal: &mut [u8], flag: u16) {
    let flags = first_page_flags(wal) | flag;
    wal[PAGE_FLAGS_FIELD].copy_from_slice(&flags.to_le_bytes());
}

/// One record so long it runs on to a second page.
fn record_across_pages(seed: u64) -> LaidOut {
    let mut rng = Rng(seed);
    let mut spec = random_spec(&mut rng);
    spec.main = rng.bytes(10_000);
    lay_out(MAJOR, SEGMENT, &[assemble(&spec)])
}

/// Where the second page of a segment laid out by `lay_out` begins.
fn second_page() -> usize {
    offset(SEGMENT + PAGE_SIZE)
}

#[test]
fn refuses_a_gap() {
    let mut records = Records::new(SEGMENT + LONG_HEADER, SEGMENT, MAJOR);
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
    let LaidOut { mut wal, lsns } = one_record();
    // A byte of the main data, which the checksum covers.
    wal[offset(lsns[0]) + RECORD_HEADER + 6] ^= 1;
    assert_eq!(
        first_error(MAJOR, &wal, lsns[0]),
        at(lsns[0], Problem::Checksum)
    );
}

#[test]
fn refuses_a_page_in_the_wrong_place() {
    let LaidOut { mut wal, lsns } = one_record();
    wal[PAGE_ADDRESS_FIELD.start] ^= 1;
    assert_eq!(
        first_problem(MAJOR, &wal, lsns[0]),
        Some(Problem::PageAddress)
    );
}

#[test]
fn refuses_unknown_page_flags() {
    // No `XLP_` flag uses the eighth bit.
    let unknown_flag = 0x80;
    for major in MAJORS {
        let LaidOut { mut wal, lsns } = one_record_of(major);
        set_first_page_flag(&mut wal, unknown_flag);
        let problem = first_problem(major, &wal, lsns[0]);
        assert_eq!(problem, Some(Problem::PageFlags), "{major}");
    }
}

#[test]
fn refuses_the_overwrite_bit_of_18_on_19() {
    let LaidOut { mut wal, lsns } = one_record_of(19);
    set_first_page_flag(&mut wal, FIRST_IS_OVERWRITE_UNTIL_18);
    assert_eq!(first_problem(19, &wal, lsns[0]), Some(Problem::PageFlags));
}

#[test]
fn reads_past_backup_removable_before_19() {
    for major in [17, 18] {
        let LaidOut { wal, lsns } = one_record_of(major);
        assert_ne!(first_page_flags(&wal) & BACKUP_REMOVABLE, 0, "{major}");
        let mut records = Records::new(lsns[0], SEGMENT, major);
        records.feed(SEGMENT, &wal).unwrap();
        assert!(records.next_record().unwrap().is_some(), "{major}");
    }
}

#[test]
fn lays_out_19_without_backup_removable() {
    let LaidOut { wal, .. } = one_record_of(19);
    assert_eq!(first_page_flags(&wal) & BACKUP_REMOVABLE, 0);
}

#[test]
fn waits_for_the_rest_of_a_long_page_header() {
    let LaidOut { wal, lsns } = one_record();
    let mut records = Records::new(lsns[0], SEGMENT, MAJOR);
    let short_header = usize::try_from(SHORT_HEADER).unwrap();
    records.feed(SEGMENT, &wal[..short_header]).unwrap();
    assert_eq!(records.next_record(), Ok(None));
    records
        .feed(records.wanted(), &wal[short_header..])
        .unwrap();
    assert!(records.next_record().unwrap().is_some());
}

#[test]
fn refuses_another_segment_size() {
    let LaidOut { wal, lsns } = one_record();
    let mut records = Records::new(lsns[0], SEGMENT * 2, MAJOR);
    records.feed(SEGMENT, &wal).unwrap();
    assert!(matches!(records.next_record(), Err(WalError::Sizes { .. })));
}

#[test]
fn refuses_a_change_of_format() {
    let LaidOut { mut wal, lsns } = record_across_pages(4);
    wal[second_page() + PAGE_MAGIC.start] ^= 1;
    assert_eq!(
        first_problem(MAJOR, &wal, lsns[0]),
        Some(Problem::OtherFormat)
    );
}

#[test]
fn refuses_a_page_that_does_not_carry_on() {
    let LaidOut { mut wal, lsns } = record_across_pages(5);
    let flags = second_page() + PAGE_FLAGS_FIELD.start;
    wal[flags] &= !FIRST_IS_CONTINUATION.to_le_bytes()[0];
    assert_eq!(
        first_problem(MAJOR, &wal, lsns[0]),
        Some(Problem::Continuation)
    );
    let LaidOut { mut wal, lsns } = record_across_pages(5);
    wal[second_page() + PAGE_REMAINING.start] ^= 1;
    assert_eq!(
        first_problem(MAJOR, &wal, lsns[0]),
        Some(Problem::Continuation)
    );
}

#[test]
fn refuses_a_continuation_where_a_record_should_start() {
    let LaidOut { mut wal, .. } = one_record();
    wal[PAGE_FLAGS_FIELD.start] |= FIRST_IS_CONTINUATION.to_le_bytes()[0];
    assert_eq!(
        first_problem(MAJOR, &wal, SEGMENT),
        Some(Problem::Continuation)
    );
}

#[test]
fn refuses_a_start_inside_a_page_header() {
    let LaidOut { wal, .. } = one_record();
    let inside_header = SEGMENT + PAGE_ADDRESS_FIELD.start as u64;
    assert_eq!(
        first_problem(MAJOR, &wal, inside_header),
        Some(Problem::Continuation)
    );
}

#[test]
fn refuses_an_overwritten_record() {
    for major in [17, 18] {
        let LaidOut { mut wal, .. } = one_record_of(major);
        set_first_page_flag(&mut wal, FIRST_IS_OVERWRITE_UNTIL_18);
        let problem = first_problem(major, &wal, SEGMENT);
        assert_eq!(problem, Some(Problem::Overwritten), "{major}");
    }
}

#[test]
fn refuses_an_overwritten_record_where_19_moved_the_flag() {
    let LaidOut { mut wal, .. } = one_record_of(19);
    set_first_page_flag(&mut wal, FIRST_IS_OVERWRITE_FROM_19);
    assert_eq!(first_problem(19, &wal, SEGMENT), Some(Problem::Overwritten));
}

#[test]
fn refuses_an_impossible_length() {
    let LaidOut { mut wal, lsns } = one_record();
    let record = offset(lsns[0]);
    let length = record + RECORD_LENGTH.start..record + RECORD_LENGTH.end;
    let shorter_than_a_header = 3u32;
    wal[length].copy_from_slice(&shorter_than_a_header.to_le_bytes());
    assert_eq!(first_problem(MAJOR, &wal, lsns[0]), Some(Problem::Length));
}
