//! `RecordReaderVerify`: what a read refuses once it counts the frames it walks, and what it still
//! cannot see. Every fixture is cut frame by frame, which the crate's own compressor cannot do.
mod common;

use std::path::{Path, PathBuf};

use common::{compress_fixture, compress_frames, fixture_records, incompressible_records};
use seekzstdsep::find::by_fixed;
use seekzstdsep::{RecordReader, RecordReaderVerify};
use tempfile::tempdir;

/// One frame per entry of `sizes`, each holding that many records of the fixture.
fn compress_sizes(dir: &Path, label: &str, sizes: &[usize]) -> PathBuf {
    let records = fixture_records();
    let mut at = 0;
    let groups: Vec<Vec<u8>> = sizes
        .iter()
        .map(|n| {
            let group = records[at..at + n].concat();
            at += n;
            group
        })
        .collect();
    compress_frames(dir, label, &groups)
}

fn plain(path: PathBuf) -> RecordReader {
    RecordReader::open(path, b"\n").expect("Failed to open the reader")
}

fn judging(path: PathBuf) -> RecordReaderVerify {
    plain(path).verifying()
}

/// Frame 2 holds the tail of a record whose head is in frame 1, so the counts match and only the
/// bytes after frame 1's last record say so.
fn split_record(dir: &Path) -> PathBuf {
    let records = fixture_records();
    let mut middle = records[10..20].concat();
    middle.extend_from_slice(&records[20][..40]);
    let mut tail = records[20][40..].to_vec();
    tail.extend_from_slice(&records[21..30].concat());
    compress_frames(dir, "split-record", &[records[..10].concat(), middle, tail])
}

/// Frame 1 holds four records where frame 0 holds ten, so every index from record 14 on is placed
/// six records too far.
const DRIFTING: &[usize] = &[10, 4, 10, 10];

fn drifting(dir: &Path) -> PathBuf {
    compress_sizes(dir, "drifting", DRIFTING)
}

#[test]
fn a_uniform_file_reads_the_same_either_way() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = compress_fixture(dir.path());

    for from in [
        0usize,
        1,
        116,
        117,
        118,
        233,
        300,
        599,
        600,
        601,
        650,
        702,
        1000,
        usize::MAX,
    ] {
        for cnt in [0usize, 1, 2, 117, 600, 10_000] {
            let trusted = plain(path.clone())
                .records(from, cnt)
                .map_err(|e| e.to_string());
            let checked = judging(path.clone())
                .records(from, cnt)
                .map_err(|e| e.to_string());
            assert_eq!(trusted, checked, "from = {from}, cnt = {cnt}");

            let mut written = Vec::new();
            let streamed = judging(path.clone())
                .records_to(from, cnt, &mut written)
                .map(|()| written)
                .map_err(|e| e.to_string());
            assert_eq!(checked, streamed, "from = {from}, cnt = {cnt}");
        }
    }

    let records = fixture_records();
    let mut reader = judging(path.clone());
    for (i, want) in records.iter().enumerate() {
        assert_eq!(
            reader.record(i).expect("Failed to read a record").as_ref(),
            Some(want),
            "record {i} did not match the fixture"
        );
    }
    assert_eq!(
        reader
            .record(records.len())
            .expect("Failed to read past the end"),
        None
    );
    assert_eq!(
        reader.total_records().expect("Failed to count records"),
        records.len()
    );
    let walked = judging(path)
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("Failed to walk the records");
    assert_eq!(walked, records);
}

#[test]
fn a_read_that_crosses_a_short_frame_is_refused() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());

    let err = judging(path)
        .records(8, 8)
        .expect_err("a read across a frame of four records was answered");
    assert!(
        err.to_string().contains("frame 1 of")
            && err.to_string().contains("holds 4 records rather than 10"),
        "the refusal did not name the frame and the counts: {err}"
    );
}

/// A read that stops inside a frame does not reach the end of it, so nothing counted it.
#[test]
fn a_read_that_stops_inside_the_short_frame_is_not_judged() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());
    let records = fixture_records();

    let got = judging(path)
        .records(10, 2)
        .expect("a read inside one frame was refused");
    assert_eq!(got, records[10..12].concat());
}

#[test]
fn an_index_past_a_short_frames_end_is_refused() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());

    let err = judging(path)
        .record(18)
        .expect_err("an index past the end of frame 1 was answered");
    assert!(
        err.to_string().contains("holds 4 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
}

#[test]
fn the_iterator_names_the_frame_that_breaks_the_count() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());

    let err = judging(path)
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()
        .expect_err("a walk over a drifting file reported nothing");
    assert!(
        err.to_string().contains("frame 1 of"),
        "the refusal did not name the frame: {err}"
    );
}

/// Records past the last index frame 0's count can form, which `record` cannot reach whatever
/// the index.
#[test]
fn a_last_frame_holding_more_than_frame_zero_is_refused() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = compress_sizes(dir.path(), "long-tail", &[10, 10, 15]);

    let err = judging(path.clone())
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()
        .expect_err("a walk over a long last frame reported nothing");
    assert!(
        err.to_string().contains("holds 15 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
    // The walk reaches the frame's end either way: asking for exactly the 35 records it holds
    // lands on it, and asking for more runs off it.
    for cnt in [35, 40] {
        let err = judging(path.clone())
            .records(0, cnt)
            .expect_err("a range read that reached the long last frame's end was answered");
        assert!(
            err.to_string().contains("holds 15 records rather than 10"),
            "the refusal did not name the counts: {err}"
        );
    }
    // What `docs/bugs.md` records: the file holds 35 records, `total_records` counts all of them,
    // and `record` can form no index past 29.
    let mut trusted = plain(path);
    assert_eq!(
        trusted.total_records().expect("Failed to count records"),
        35
    );
    assert_eq!(
        trusted.record(30).expect("Failed to read past the end"),
        None
    );
}

#[test]
fn a_short_last_frame_is_what_every_file_ends_with() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = compress_sizes(dir.path(), "short-tail", &[10, 10, 3]);
    let records = fixture_records();

    let mut reader = judging(path.clone());
    assert_eq!(reader.total_records().expect("Failed to count records"), 23);
    assert_eq!(
        reader
            .records(8, 6)
            .expect("a read across the file was refused"),
        records[8..14].concat()
    );
    assert_eq!(
        reader.record(23).expect("Failed to read past the end"),
        None
    );
    let walked = judging(path)
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("Failed to walk the records");
    assert_eq!(walked, records[..23].to_vec());
}

/// Frames far larger than the window a walk reads through, so the count crosses slides of it.
#[test]
fn a_frame_larger_than_the_window_is_counted_whole() {
    let dir = tempdir().expect("Failed to create temp dir");
    let records = incompressible_records(21, 64 * 1024);
    let groups: Vec<Vec<u8>> = [
        &records[..6],
        &records[6..12],
        &records[12..15],
        &records[15..21],
    ]
    .iter()
    .map(|group| group.concat())
    .collect();
    let path = compress_frames(dir.path(), "large", &groups);

    let mut reader = judging(path.clone());
    assert_eq!(
        reader.record(7).expect("Failed to read a record").as_ref(),
        Some(&records[7])
    );
    let err = reader
        .record(17)
        .expect_err("an index past the end of frame 2 was answered");
    assert!(
        err.to_string().contains("frame 2 of") && err.to_string().contains("holds 3 records"),
        "the refusal did not name the frame and the count: {err}"
    );

    let err = judging(path)
        .records(0, 21)
        .expect_err("a read across frame 2 was answered");
    assert!(
        err.to_string().contains("holds 3 records rather than 6"),
        "the refusal did not name the counts: {err}"
    );
}

/// The frame holding the wrong count is behind the read, so nothing walks it and nothing counts
/// it. What the check covers is the frames a read touches, and this is the other side of that.
#[test]
fn a_read_placed_by_a_frame_it_never_walks_is_not_judged() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());
    let records = fixture_records();

    let got = judging(path.clone())
        .records(20, 2)
        .expect("a read starting past the drift was refused");
    assert_eq!(got, records[14..16].concat());
    assert_eq!(
        judging(path)
            .record(20)
            .expect("an index past the drift was refused"),
        Some(records[14].clone())
    );
}

/// The count comes from frame 0 and the last frame is the only one counted, so a middle frame
/// holding more is out of reach of both.
#[test]
fn a_middle_frame_holding_more_is_counted_by_the_read_and_not_by_the_total() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = compress_sizes(dir.path(), "wide-middle", &[10, 15, 10]);

    assert_eq!(
        judging(path.clone())
            .total_records()
            .expect("a middle frame of fifteen records was refused by the count"),
        30
    );
    let err = judging(path)
        .records(0, 35)
        .expect_err("a read across the middle frame was answered");
    assert!(
        err.to_string().contains("holds 15 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
}

/// `records_to` writes as it walks rather than gathering, so it reaches the check by its own path.
#[test]
fn records_to_refuses_what_records_refuses() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());

    let mut written = Vec::new();
    let err = judging(path)
        .records_to(8, 8, &mut written)
        .expect_err("a streamed read across a frame of four records was answered");
    assert!(
        err.to_string().contains("holds 4 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
}

/// The index lands exactly where the short frame ends: the walk reaches it, finds no record, and
/// that is where the count is known.
#[test]
fn an_index_at_a_short_frames_end_is_refused() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());

    let err = judging(path)
        .record(14)
        .expect_err("an index at the end of frame 1 was answered as missing");
    assert!(
        err.to_string().contains("holds 4 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
}

/// Records found by a finder rather than by a separator. The count is the same question, and the
/// walk that answers it is the same walk.
#[test]
fn a_drifting_file_of_fixed_width_records_is_refused() {
    let dir = tempdir().expect("Failed to create temp dir");
    let records: Vec<Vec<u8>> = (0..24)
        .map(|i| format!("record{i:03}").into_bytes())
        .collect();
    let groups: Vec<Vec<u8>> = [&records[..10], &records[10..14], &records[14..24]]
        .iter()
        .map(|group| group.concat())
        .collect();
    let path = compress_frames(dir.path(), "fixed", &groups);

    let mut reader = RecordReader::open_with(path, Box::new(by_fixed(9)))
        .expect("Failed to open the reader with a finder")
        .verifying();

    assert_eq!(reader.records_per_frame(), 10);
    let err = reader
        .records(8, 8)
        .expect_err("a read across a frame of four records was answered");
    assert!(
        err.to_string().contains("holds 4 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
}

/// The file every compressor writes: the last frame holds what was left over, and what follows its
/// last record goes out with a read that runs past the end.
#[test]
fn a_file_ending_in_a_fragment_is_read_to_its_last_whole_record() {
    let dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records();
    let mut tail = records[20..23].concat();
    tail.extend_from_slice(b"an input that ended mid-record");
    let groups = vec![records[..10].concat(), records[10..20].concat(), tail];
    let path = compress_frames(dir.path(), "tail-fragment", &groups);

    let mut reader = judging(path.clone());
    assert_eq!(reader.total_records().expect("Failed to count records"), 23);
    assert_eq!(
        reader
            .records(8, 6)
            .expect("a read across the file was refused"),
        records[8..14].concat()
    );
    // A read that runs past the end hands back what follows the last record, as a read of the
    // whole span did before any of this.
    let over = judging(path.clone())
        .records(20, 10)
        .expect("a read past the end was refused");
    let trusted = plain(path.clone())
        .records(20, 10)
        .expect("Failed to read past the end");
    assert_eq!(over, trusted);
    assert!(over.ends_with(b"an input that ended mid-record"));

    let walked = judging(path)
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("Failed to walk the records");
    assert_eq!(walked, records[..23].to_vec());
}

/// A refusal is the end of the walk: the iterator does not hand out the same error for ever.
#[test]
fn the_iterator_is_spent_once_it_refuses() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());

    let mut records = judging(path).into_records();
    while let Some(item) = records.next() {
        if item.is_err() {
            break;
        }
    }
    assert!(
        records.next().is_none(),
        "the iterator went on after refusing"
    );
}

#[test]
fn reverse_iteration_verifies_each_frame_and_stops_both_ends_on_error() {
    let dir = tempdir().unwrap();
    let expected = fixture_records();
    let path = compress_sizes(dir.path(), "reverse-valid", &[10, 10, 3]);
    let got = judging(path)
        .into_records()
        .rev()
        .collect::<anyhow::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        got,
        expected[..23].iter().rev().cloned().collect::<Vec<_>>()
    );

    for (path, message) in [
        (drifting(dir.path()), "holds 4 records rather than 10"),
        (
            split_record(dir.path()),
            "holds bytes after its last record",
        ),
        (
            compress_sizes(dir.path(), "reverse-long-last", &[10, 11]),
            "holds 11 records rather than 10",
        ),
    ] {
        let mut records = judging(path).into_records();
        let error = records
            .by_ref()
            .rev()
            .find_map(Result::err)
            .expect("invalid frame was accepted");
        assert!(error.to_string().contains(message), "{error}");
        assert!(records.next().is_none());
        assert!(records.next_back().is_none());
    }
}

#[test]
fn mixed_iteration_checks_the_frame_where_both_ends_meet() {
    let dir = tempdir().unwrap();
    let path = compress_sizes(dir.path(), "reverse-meet", &[10, 4, 10]);
    let mut records = judging(path).into_records();
    for _ in 0..10 {
        records.next().unwrap().unwrap();
        records.next_back().unwrap().unwrap();
    }
    records.next().unwrap().unwrap();
    let error = records.next_back().unwrap().unwrap_err();
    assert!(error.to_string().contains("holds 4 records rather than 10"));
    assert!(records.next().is_none());
    assert!(records.next_back().is_none());
}

/// The skip runs out inside the short frame rather than reaching it, which is the other way into
/// the count refusal.
#[test]
fn a_read_whose_skip_runs_out_in_a_short_frame_is_refused() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());

    let err = judging(path)
        .records(16, 2)
        .expect_err("a read whose skip ran out in frame 1 was answered");
    assert!(
        err.to_string().contains("holds 4 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
}

/// A record split across two frames. The counts match — frame 1 holds ten whole records and the
/// head of an eleventh, frame 2 its tail and nine more — so only the bytes left after frame 1's
/// last record say so.
#[test]
fn a_frame_holding_bytes_after_its_last_record_is_refused() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = split_record(dir.path());
    let records = fixture_records();

    let err = judging(path.clone())
        .records(0, 25)
        .expect_err("a read across a record split between two frames was answered");
    assert!(
        err.to_string().contains("frame 1 of")
            && err
                .to_string()
                .contains("holds bytes after its last record"),
        "the refusal did not name the frame and the cause: {err}"
    );
    let err = judging(path.clone())
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()
        .expect_err("a walk over a record split between two frames reported nothing");
    assert!(
        err.to_string()
            .contains("holds bytes after its last record"),
        "the refusal did not name the cause: {err}"
    );

    // `record` reads the frame the index falls in and no other, so the frame that broke the file
    // is never walked: it answers with the half that ends where it looked.
    assert_eq!(
        judging(path.clone())
            .record(20)
            .expect("an index in the frame after the split was refused")
            .map(|r| r.len()),
        Some(records[20].len() - 40)
    );

    // Both walks join the two halves, so the bytes are there either way; the refusal is about the
    // file, not about what the read could return.
    let trusted = plain(path)
        .records(0, 25)
        .expect("a read under the default was refused");
    assert_eq!(trusted, records[..25].concat());
}

/// A refusal comes after the records the walk had already written, since this writes as it walks.
#[test]
fn a_streamed_read_leaves_what_it_wrote_before_the_refusal() {
    let dir = tempdir().expect("Failed to create temp dir");
    // Frames far larger than the window this walks through, so the run that reaches the frame's
    // end is not the run that starts the read: what came before it is out before the refusal.
    let records = incompressible_records(21, 64 * 1024);
    let groups: Vec<Vec<u8>> = [&records[..6], &records[6..15], &records[15..21]]
        .iter()
        .map(|group| group.concat())
        .collect();
    let path = compress_frames(dir.path(), "wide-middle-large", &groups);

    let mut written = Vec::new();
    judging(path)
        .records_to(0, 21, &mut written)
        .expect_err("a read across a frame of nine records was answered");
    assert!(
        !written.is_empty() && records.concat().starts_with(&written),
        "what went out before the refusal was not a prefix of the records: {} bytes",
        written.len()
    );
}

/// A frame holding more than frame 0 is judged like any other — but only where the read walks it
/// to the end. One that stops inside it is answered.
#[test]
fn a_read_that_stops_inside_a_long_frame_is_not_judged() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = compress_sizes(dir.path(), "wide-middle", &[10, 15, 10]);
    let records = fixture_records();

    let got = judging(path.clone())
        .records(10, 3)
        .expect("a read inside the long frame was refused");
    assert_eq!(got, records[10..13].concat());

    let err = judging(path)
        .records(10, 20)
        .expect_err("a read that walked the long frame to its end was answered");
    assert!(
        err.to_string().contains("holds 15 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
}

/// What the check exists for: without it the same read answers, and what it answers is wrong.
#[test]
fn the_unchecked_read_answers_with_the_wrong_records() {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = drifting(dir.path());
    let records = fixture_records();

    let got = plain(path.clone())
        .records(14, 2)
        .expect("Failed to read a record range");
    assert_ne!(got, records[14..16].concat());

    // The other three ways in answer too, and the default is what they answer under.
    let mut written = Vec::new();
    plain(path.clone())
        .records_to(14, 2, &mut written)
        .expect("a streamed read over a drifting file was refused");
    assert_eq!(written, got);
    assert_eq!(
        RecordReader::open(path.clone(), b"\n")
            .expect("Failed to open the reader")
            .record(18)
            .expect("an index past the end of frame 1 was refused"),
        None
    );
    assert_eq!(
        RecordReader::open(path.clone(), b"\n")
            .expect("Failed to open the reader")
            .into_records()
            .collect::<anyhow::Result<Vec<_>>>()
            .expect("a walk over a drifting file was refused")
            .len(),
        34
    );

    let err = judging(path)
        .records(14, 2)
        .expect_err("the same read was answered with the check on");
    assert!(
        err.to_string().contains("holds 4 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
}

/// `AsRead::watch` is reachable from outside the crate, through the same [`Verifier`] and
/// [`Judge`] the library exports, so what it does with arguments a reader would never hand it is
/// part of what it promises. A walk with no frame end to reach is watched against none of them,
/// rather than dividing by zero or running off the frame list.
mod watch_is_total {
    use seekzstdsep::{AsRead, Judge, Verifier};

    fn judge() -> <AsRead as Verifier>::Judge {
        <AsRead as Verifier>::Judge::new("nowhere.seek.zst".to_owned(), 0, 10)
    }

    #[test]
    fn a_file_with_no_records_to_a_frame_is_watched_against_nothing() {
        let judge = judge();
        let frames = [(0u64, 100u64)];
        <AsRead as Verifier>::watch(&judge, &frames, 0, 0, 0, 100);
    }

    #[test]
    fn a_read_placed_past_the_last_frame_is_watched_against_nothing() {
        let judge = judge();
        let frames = [(0u64, 100u64)];
        <AsRead as Verifier>::watch(&judge, &frames, 10, 1_000, 0, 100);
    }

    #[test]
    fn a_frame_ending_before_the_read_starts_is_watched_against_nothing() {
        let judge = judge();
        let frames = [(0u64, 100u64), (100, 100)];
        <AsRead as Verifier>::watch(&judge, &frames, 10, 0, 1_000, 100);
    }
}

/// A frame that ends with the head of a record, holding fewer records than frame 0, is refused by
/// a read that stops in it — not only by one that walks on past it.
///
/// The walk reports a frame end when a run reaches it. A frame ending in a fragment has no run
/// that reaches its end, so a read stopping there passes the frame by without counting it.
#[test]
fn a_read_stopping_in_a_frame_that_ends_in_a_fragment_is_refused() {
    let dir = tempdir().expect("Failed to make a temp dir");
    let records = fixture_records();
    let mut middle = records[10..13].concat();
    middle.extend_from_slice(&records[13][..40]);
    let mut tail = records[13][40..].to_vec();
    tail.extend_from_slice(&records[14..24].concat());
    let path = compress_frames(
        dir.path(),
        "short-frame-with-fragment",
        &[records[..10].concat(), middle, tail],
    );

    let err = judging(path)
        .records(0, 15)
        .expect_err("a read that ran out inside the short frame was answered");
    assert!(
        err.to_string().contains("frame 1"),
        "the refusal did not name the frame the read stopped in: {err}"
    );
}

/// The skip runs out inside a frame that ends with the head of a record. The walk reaches no
/// offset there either, so the refusal has to come from where it stopped rather than from the
/// frame end it never passed.
#[test]
fn a_read_whose_skip_runs_out_in_a_frame_that_ends_in_a_fragment_is_refused() {
    let dir = tempdir().expect("Failed to make a temp dir");
    let records = fixture_records();
    let mut middle = records[10..13].concat();
    middle.extend_from_slice(&records[13][..40]);
    let mut tail = records[13][40..].to_vec();
    tail.extend_from_slice(&records[14..24].concat());
    let path = compress_frames(
        dir.path(),
        "skip-into-fragment",
        &[records[..10].concat(), middle, tail],
    );

    let err = judging(path)
        .records(15, 2)
        .expect_err("a read whose skip ran out in the short frame was answered");
    assert!(
        err.to_string().contains("holds 3 records rather than 10"),
        "the refusal did not name the counts: {err}"
    );
}
