//! The standard iterator and borrowed fold share the range-read semantics.
mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{Arc, Mutex};

use common::{
    FIXTURE_RECORDS, FIXTURE_RECORDS_PER_FRAME, compress_fixture, compress_frames, fixture_records,
};
use seekzstdsep::{RecordReader, find};
use tempfile::tempdir;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations_of(body: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    body();
    ALLOCATIONS.with(Cell::get) - before
}

#[test]
fn standard_iterator_chain_matches_range_reads() {
    let dir = tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let expected = fixture_records();

    for (from, cnt) in [(0, 1), (0, 5), (1, 2), (116, 3), (117, 4), (598, 5)] {
        let actual: Vec<Vec<u8>> = RecordReader::open(path.clone(), b"\n")
            .unwrap()
            .into_records_from(from)
            .take(cnt)
            .collect::<anyhow::Result<_>>()
            .unwrap();
        assert_eq!(actual, expected[from..(from + cnt).min(expected.len())]);

        let mut reader = RecordReader::open(path.clone(), b"\n").unwrap();
        assert_eq!(actual.concat(), reader.records(from, cnt).unwrap());
        let mut written = Vec::new();
        reader.records_to(from, cnt, &mut written).unwrap();
        assert_eq!(written, actual.concat());
    }
}

#[test]
fn starting_later_does_not_decode_earlier_frames() {
    let dir = tempdir().unwrap();
    let path = compress_frames(
        dir.path(),
        "marked",
        &[
            b"aa\naa\n".to_vec(),
            b"bb\nbb\n".to_vec(),
            b"cc\ncc\n".to_vec(),
        ],
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let watched = Arc::clone(&seen);
    let reader = RecordReader::open_with(
        path,
        Box::new(move |data: &[u8]| {
            watched.lock().unwrap().extend_from_slice(data);
            find::by_separator(&memchr::memmem::Finder::new(b"\n"))(data)
        }),
    )
    .unwrap();

    // Opening counts frame 0; only calls made by the iterator count here.
    seen.lock().unwrap().clear();
    let records: Vec<Vec<u8>> = reader
        .into_records_from(4)
        .take(2)
        .collect::<anyhow::Result<_>>()
        .unwrap();
    assert_eq!(records.concat(), b"cc\ncc\n");
    let seen = seen.lock().unwrap();
    assert!(!seen.contains(&b'a') && !seen.contains(&b'b'));
}

#[test]
fn filter_map_and_collect_are_standard_iterator_adapters() {
    let dir = tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let expected: Vec<usize> = fixture_records()
        .into_iter()
        .filter(|record| record.len() % 2 == 0)
        .take(4)
        .map(|record| record.len())
        .collect();

    let actual: Vec<usize> = RecordReader::open(path, b"\n")
        .unwrap()
        .into_records()
        .filter_map(|record| match record {
            Ok(bytes) if bytes.len() % 2 == 0 => Some(Ok(bytes.len())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .take(4)
        .collect::<anyhow::Result<_>>()
        .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn borrowed_fold_can_copy_only_the_record_it_keeps() {
    let dir = tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let expected = fixture_records();
    let wanted = &expected[FIXTURE_RECORDS_PER_FRAME + 5];
    let mut reader = RecordReader::open(path, b"\n").unwrap();

    let kept = reader
        .fold_records(0, FIXTURE_RECORDS, Vec::new(), |mut kept, record| {
            if record == wanted {
                kept.extend_from_slice(record);
            }
            Ok(kept)
        })
        .unwrap();
    assert_eq!(kept, *wanted);
}

#[test]
fn borrowed_filter_allocates_less_than_owned_iterator() {
    let dir = tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let expected = fixture_records();
    let wanted = &expected[FIXTURE_RECORDS - 1];

    let borrowed = allocations_of(|| {
        let mut reader = RecordReader::open(path.clone(), b"\n").unwrap();
        let kept = reader
            .fold_records(0, FIXTURE_RECORDS, Vec::new(), |mut kept, record| {
                if record == wanted {
                    kept.extend_from_slice(record);
                }
                Ok(kept)
            })
            .unwrap();
        assert_eq!(kept, *wanted);
    });
    let owned = allocations_of(|| {
        let reader = RecordReader::open(path, b"\n").unwrap();
        let kept = reader
            .into_records()
            .find_map(|record| {
                let record = record.unwrap();
                (record == *wanted).then_some(record)
            })
            .unwrap();
        assert_eq!(kept, *wanted);
    });

    eprintln!("borrowed allocations: {borrowed}, owned allocations: {owned}");
    assert!(borrowed < owned);
}

#[test]
fn borrowed_fold_counts_one_record_per_step_across_frames() {
    let dir = tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let mut reader = RecordReader::open(path, b"\n").unwrap();

    for (from, cnt) in [(0, 0), (0, 1), (116, 3), (117, 4), (598, 5)] {
        let actual = reader
            .fold_records(from, cnt, 0, |count, _| Ok(count + 1))
            .unwrap();
        assert_eq!(actual, cnt.min(FIXTURE_RECORDS - from));
    }
}
