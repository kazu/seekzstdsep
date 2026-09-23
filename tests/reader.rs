//! `RecordReader`: the records a range read returns, plus the ones it cannot address one at a
//! time.
mod common;

use common::{
    FIXTURE_RECORDS, FIXTURE_RECORDS_PER_FRAME, compress_body, compress_fixture, compress_frames,
    fixture_records, fixture_records_upto, incompressible_records,
};
use seekzstdsep::RecordReader;
use tempfile::tempdir;

fn open_fixture(dir: &std::path::Path) -> RecordReader {
    RecordReader::open(compress_fixture(dir), b"\n").expect("Failed to open the reader")
}

#[test]
fn reports_the_framing_the_compressor_wrote() {
    let dir = tempdir().expect("Failed to create temp dir");
    let mut reader = open_fixture(dir.path());

    assert_eq!(reader.records_per_frame(), FIXTURE_RECORDS_PER_FRAME);
    assert_eq!(
        reader.total_records().expect("Failed to count records"),
        FIXTURE_RECORDS
    );
}

#[test]
fn reads_every_record_by_index() {
    let dir = tempdir().expect("Failed to create temp dir");
    let mut reader = open_fixture(dir.path());
    let expected = fixture_records();

    for (i, want) in expected.iter().enumerate() {
        let got = reader
            .record(i)
            .expect("Failed to read a record")
            .unwrap_or_else(|| panic!("record {i} came back missing"));
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(want),
            "record {i} did not match the fixture"
        );
    }
}

/// Records that do not compress, six to a frame, so each frame is far larger than the window a
/// lookup reads through. Anything the window gets wrong between two lookups shows here and not on
/// a fixture whose frames fit in one fill.
const LARGE_FRAME_RECORDS: usize = 24;
const LARGE_FRAME_PER_FRAME: usize = 6;

fn open_large_frames(dir: &std::path::Path) -> (Vec<Vec<u8>>, RecordReader) {
    let records = incompressible_records(LARGE_FRAME_RECORDS, 64 * 1024);
    let groups: Vec<Vec<u8>> = records
        .chunks(LARGE_FRAME_PER_FRAME)
        .map(<[Vec<u8>]>::concat)
        .collect();
    let path = compress_frames(dir, "large", &groups);
    let reader = RecordReader::open(path, b"\n").expect("Failed to open the reader");
    (records, reader)
}

fn assert_reads(reader: &mut RecordReader, records: &[Vec<u8>], index: usize) {
    let got = reader
        .record(index)
        .expect("Failed to read a record")
        .unwrap_or_else(|| panic!("record {index} came back missing"));
    assert_eq!(got, records[index], "record {index} did not match");
}

/// A lookup goes on from where the last one left the window, so what it hands out has to be right
/// whatever order the indices arrive in.
#[test]
fn reads_records_in_any_order_from_frames_larger_than_the_window() {
    let dir = tempdir().expect("Failed to create temp dir");
    let (records, mut reader) = open_large_frames(dir.path());
    assert_eq!(reader.records_per_frame(), LARGE_FRAME_PER_FRAME);

    let forwards: Vec<usize> = (0..records.len()).collect();
    let backwards: Vec<usize> = (0..records.len()).rev().collect();
    // On from the record before, back to one the window may still hold, and over to another frame.
    let about: Vec<usize> = vec![10, 11, 11, 10, 9, 6, 7, 8, 7, 23, 0, 5, 12, 12, 11];
    for order in [forwards, backwards, about] {
        for index in order {
            assert_reads(&mut reader, &records, index);
        }
    }
}

/// Everything else the reader does seeks the same file the window reads through. A lookup after
/// one of those must not read on from where it left the file.
///
/// Each move goes between a record and the next one in the same frame, which is the pair a window
/// is carried across. Reading the same index twice would not do: that is a lookup behind the walk,
/// which reads the frame again whether the window was given up or not.
#[test]
fn a_lookup_after_a_read_that_moved_the_file_reads_the_right_record() {
    let dir = tempdir().expect("Failed to create temp dir");
    let (records, mut reader) = open_large_frames(dir.path());

    for index in [0usize, 1, 6, 12, 18, 22] {
        assert_reads(&mut reader, &records, index);
        assert_eq!(
            reader.total_records().expect("Failed to count records"),
            LARGE_FRAME_RECORDS
        );
        assert_reads(&mut reader, &records, index + 1);

        assert_reads(&mut reader, &records, index);
        let mut written = Vec::new();
        reader
            .records_to(2, 3, &mut written)
            .expect("Failed to write records");
        assert_eq!(written, records[2..5].concat());
        assert_reads(&mut reader, &records, index + 1);

        assert_reads(&mut reader, &records, index);
        let gathered = reader.records(8, 2).expect("Failed to read records");
        assert_eq!(gathered, records[8..10].concat());
        assert_reads(&mut reader, &records, index + 1);
    }
}

/// Reading forwards and backwards returns the same records: what the window carries must not
/// depend on the order it is asked in.
#[test]
fn reads_records_backwards_too() {
    let dir = tempdir().expect("Failed to create temp dir");
    let mut reader = open_fixture(dir.path());
    let expected = fixture_records();

    for i in (0..expected.len()).rev() {
        let got = reader
            .record(i)
            .expect("Failed to read a record")
            .unwrap_or_else(|| panic!("record {i} came back missing"));
        assert_eq!(got, expected[i], "record {i} did not match the fixture");
    }
}

#[test]
fn range_reads_resume_after_output_errors_and_before_reverse_iteration() {
    let dir = tempdir().unwrap();
    macro_rules! check {
        ($reader:expr, $expected:expr) => {{
            let mut reader = $reader;
            let expected = $expected;
            assert_eq!(reader.record(5).unwrap().unwrap(), expected[5]);
            let mut short = &mut [0u8; 8][..];
            assert!(reader.records_to(4, 9, &mut short).is_err());
            assert_eq!(reader.record(6).unwrap().unwrap(), expected[6]);
            for (from, count) in [(10, 10), (1, 4), (5, 9)] {
                let mut written = Vec::new();
                reader.records_to(from, count, &mut written).unwrap();
                assert_eq!(written, expected[from..from + count].concat());
            }
            let got = reader
                .into_records()
                .rev()
                .collect::<anyhow::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(got, expected.into_iter().rev().collect::<Vec<_>>());
        }};
    }
    let (expected, reader) = open_large_frames(dir.path());
    check!(reader, expected);
    let (expected, reader) = open_large_frames(dir.path());
    check!(reader.verifying(), expected);
}

#[test]
fn an_index_past_the_last_record_is_none() {
    let dir = tempdir().expect("Failed to create temp dir");
    let mut reader = open_fixture(dir.path());

    assert!(
        reader
            .record(FIXTURE_RECORDS)
            .expect("Failed to read a record")
            .is_none()
    );
    assert!(
        reader
            .record(FIXTURE_RECORDS * 10)
            .expect("Failed to read a record")
            .is_none()
    );
}

/// A `cnt` that overflows the sum placing the end of the range means the end of the file, which is
/// what a count the file cannot fill means anywhere else.
#[test]
fn a_count_that_overflows_reads_to_the_end() {
    let dir = tempdir().expect("Failed to create temp dir");
    let mut reader = open_fixture(dir.path());
    let expected = fixture_records();

    for from in [
        0,
        FIXTURE_RECORDS_PER_FRAME,
        2 * FIXTURE_RECORDS_PER_FRAME,
        FIXTURE_RECORDS - 1,
    ] {
        let got = reader
            .records(from, usize::MAX)
            .expect("Failed to read records");
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&expected[from..].concat()),
            "records(from = {from}, cnt = usize::MAX) did not read to the end"
        );
    }
}

/// An index large enough to overflow the frame lookup is past the end like any other.
#[test]
fn an_index_that_overflows_the_frame_lookup_is_past_the_end() {
    let dir = tempdir().expect("Failed to create temp dir");
    let mut reader = open_fixture(dir.path());

    let err = reader
        .records(usize::MAX, 1)
        .expect_err("a record past the end came back");
    assert!(
        err.to_string().contains("past the end"),
        "the failure does not say the record is past the end: {err}"
    );
}

#[test]
fn iterating_returns_every_record_in_order() {
    let dir = tempdir().expect("Failed to create temp dir");
    let reader = open_fixture(dir.path());

    let got: Vec<Vec<u8>> = reader
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("Failed to iterate records");

    assert_eq!(got, fixture_records());
}

#[test]
fn iterating_backwards_returns_every_record() {
    let dir = tempdir().unwrap();
    let mut reader = open_fixture(dir.path());
    reader.record(7).unwrap();
    let got = reader
        .into_records()
        .rev()
        .collect::<anyhow::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(got, fixture_records().into_iter().rev().collect::<Vec<_>>());
}

#[test]
fn either_end_meets_without_repeating_records() {
    let dir = tempdir().unwrap();
    for groups in [
        vec![b"a\nb\nc\nd\ne\n".to_vec()],
        vec![
            b"a\nb\n".to_vec(),
            Vec::new(),
            b"fragment".to_vec(),
            b"c\nd\ne\ntrailing".to_vec(),
            Vec::new(),
        ],
    ] {
        let path = compress_frames(dir.path(), "mixed-ends", &groups);
        let expected = [b"a\n", b"b\n", b"c\n", b"d\n", b"e\n"];
        for schedule in 0..1 << expected.len() {
            let mut records = RecordReader::open(path.clone(), b"\n")
                .unwrap()
                .into_records();
            let mut remaining = expected.iter();
            for step in 0..expected.len() {
                let (got, want) = if schedule & (1 << step) == 0 {
                    (records.next(), remaining.next())
                } else {
                    (records.next_back(), remaining.next_back())
                };
                assert_eq!(got.unwrap().unwrap(), *want.unwrap(), "schedule {schedule}");
            }
            for _ in 0..2 {
                assert!(records.next().is_none());
                assert!(records.next_back().is_none());
            }
        }
    }
}

#[test]
fn reversing_crosses_windows_and_preserves_long_records() {
    let dir = tempdir().unwrap();
    let (expected, reader) = open_large_frames(dir.path());
    let got = reader
        .into_records()
        .rev()
        .collect::<anyhow::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(got, expected.iter().rev().cloned().collect::<Vec<_>>());

    let (_, reader) = open_large_frames(dir.path());
    let mut records = reader.into_records();
    let mut remaining = expected.iter();
    for step in 0..expected.len() {
        let (got, want) = if step % 3 == 0 {
            (records.next(), remaining.next())
        } else {
            (records.next_back(), remaining.next_back())
        };
        assert_eq!(got.unwrap().unwrap(), *want.unwrap());
    }
    assert!(records.next_back().is_none());
    assert!(records.next().is_none());
}

#[test]
fn forward_runs_resume_after_reverse_reads_across_windows() {
    let dir = tempdir().unwrap();
    let (expected, reader) = open_large_frames(dir.path());
    let mut remaining = expected.iter();
    let mut records = reader.into_records();
    for step in 0..expected.len() {
        let (got, want) = if step % 7 < 4 {
            (records.next(), remaining.next())
        } else {
            (records.next_back(), remaining.next_back())
        };
        assert_eq!(got.unwrap().unwrap(), *want.unwrap());
    }
    assert!(records.next().is_none());
    assert!(records.next_back().is_none());
}

#[test]
fn reversing_uses_forward_boundaries_for_overlapping_separators() {
    let dir = tempdir().unwrap();
    let path = compress_frames(
        dir.path(),
        "overlapping",
        &[b"aaaaxaa".to_vec(), b"aaayaaa".to_vec()],
    );
    let expected = [b"aa".as_slice(), b"aa", b"xaa", b"aa", b"ayaa"];
    let got = RecordReader::open(path, b"aa")
        .unwrap()
        .into_records()
        .rev()
        .collect::<anyhow::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(got, expected.into_iter().rev().collect::<Vec<_>>());
}

#[test]
fn a_single_empty_record_is_returned_only_once_from_either_end() {
    let dir = tempdir().unwrap();
    let path = compress_frames(dir.path(), "single", &[b"\n".to_vec(), Vec::new()]);
    for reverse in [false, true] {
        let mut records = RecordReader::open(path.clone(), b"\n")
            .unwrap()
            .verifying()
            .into_records();
        let record = if reverse {
            records.next_back()
        } else {
            records.next()
        };
        assert_eq!(record.unwrap().unwrap(), b"\n");
        assert!(records.next_back().is_none());
        assert!(records.next().is_none());
    }
}

/// A file whose last record carries no separator ends in a fragment. It is not a whole record, so
/// neither the iterator nor `record` hands it out.
#[test]
fn a_trailing_fragment_is_not_a_record() {
    let dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(FIXTURE_RECORDS_PER_FRAME + 5, false);
    let out_path = compress_body(dir.path(), "fragment", &records.concat());

    let whole = records.len() - 1;
    let mut reader =
        RecordReader::open(out_path.clone(), b"\n").expect("Failed to open the reader");
    assert!(
        reader
            .record(whole)
            .expect("Failed to read a record")
            .is_none(),
        "the fragment after the last separator was returned as record {whole}"
    );
    assert_eq!(reader.total_records().expect("Failed to count"), whole);

    let reader = RecordReader::open(out_path, b"\n").expect("Failed to open the reader");
    let got: Vec<Vec<u8>> = reader
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("Failed to iterate records");
    assert_eq!(got, records[..whole]);
}

#[test]
fn a_record_longer_than_the_read_window_comes_back_whole() {
    let dir = tempdir().expect("Failed to create temp dir");
    // Longer than the window a region is decoded through, so it arrives in pieces.
    let long = "x".repeat(100_000);
    let records: Vec<Vec<u8>> = [
        "first\n".to_string(),
        format!("{long}\n"),
        "last\n".to_string(),
    ]
    .iter()
    .map(|r| r.as_bytes().to_vec())
    .collect();
    let out_path = compress_body(dir.path(), "long-record", &records.concat());

    let mut reader =
        RecordReader::open(out_path.clone(), b"\n").expect("Failed to open the reader");
    let mut got = Vec::new();
    reader
        .records_to(0, 2, &mut got)
        .expect("Failed to write records");
    assert_eq!(got, [records[0].clone(), records[1].clone()].concat());

    let reader = RecordReader::open(out_path, b"\n").expect("Failed to open the reader");
    let iterated: Vec<Vec<u8>> = reader
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("Failed to iterate records");
    assert_eq!(iterated, records);
}

#[test]
fn the_byte_stream_is_the_whole_file() {
    let dir = tempdir().expect("Failed to create temp dir");
    let reader = open_fixture(dir.path());

    let mut got = Vec::new();
    std::io::Read::read_to_end(
        &mut reader.into_bytes().expect("Failed to rewind"),
        &mut got,
    )
    .expect("Failed to read the byte stream");

    assert_eq!(got, fixture_records().concat());
}

/// `into_bytes` rewinds a decoder that has been reading frames out of order, so what it returns
/// must still start at record 0.
#[test]
fn the_byte_stream_starts_from_the_beginning_after_seeking() {
    let dir = tempdir().expect("Failed to create temp dir");
    let mut reader = open_fixture(dir.path());
    reader
        .record(FIXTURE_RECORDS - 1)
        .expect("Failed to read a record");

    let mut got = Vec::new();
    std::io::Read::read_to_end(
        &mut reader.into_bytes().expect("Failed to rewind"),
        &mut got,
    )
    .expect("Failed to read the byte stream");

    assert_eq!(got, fixture_records().concat());
}

/// An empty separator ends no record: every scan would match at every byte and span none of them,
/// so [`RecordReader::into_records`] would hand out empty records forever.
#[test]
fn an_empty_separator_is_refused() {
    let dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(dir.path());

    let err = match RecordReader::open(out_path, b"") {
        Err(e) => e,
        Ok(_) => panic!("an empty separator was accepted"),
    };
    assert!(
        err.to_string().contains("separator must not be empty"),
        "the failure was not about the separator: {err}"
    );
}

/// `records_to` writes what `records` returns, over the whole corpus of positions and counts the
/// recorded fixture covers, including the ones that run past the end.
#[test]
fn records_to_writes_what_records_returns() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    for from in [0usize, 1, 116, 117, 118, 233, 300, 599, 600, 1000] {
        for cnt in [0usize, 1, 2, 117, 600, 10_000] {
            let gathered = RecordReader::open(out_path.clone(), b"\n")
                .expect("Failed to open reader")
                .records(from, cnt)
                .map_err(|e| e.to_string());
            let mut written = Vec::new();
            let streamed = RecordReader::open(out_path.clone(), b"\n")
                .expect("Failed to open reader")
                .records_to(from, cnt, &mut written)
                .map(|()| written)
                .map_err(|e| e.to_string());
            assert_eq!(gathered, streamed, "from = {from}, cnt = {cnt}");
        }
    }
}

/// Frame 0's separator count is taken as the record count of every frame, and every record range
/// is placed by dividing by it. A separator the file was not built with occurs in no frame, so the
/// count comes out 0 and the division has nothing to divide by.
#[test]
fn a_separator_that_occurs_nowhere_is_refused() {
    let dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(dir.path());

    let err = match RecordReader::open(out_path, b"ZZZZ") {
        Err(e) => e,
        Ok(_) => panic!("a separator that occurs in no record was accepted"),
    };
    assert!(
        err.to_string().contains("no record in frame 0"),
        "the failure did not name the separator as the cause: {err}"
    );
}

/// The same bytes held in memory read as the file does: `from_reader` is what `open` is built on,
/// with the source and its name handed in rather than a path.
#[test]
fn a_reader_over_any_source_reads_what_the_file_reader_does() {
    use std::io::{Cursor, Read};

    use seekzstdsep::find::Boundary;

    let dir = tempdir().expect("Failed to create temp dir");
    let path = compress_fixture(dir.path());
    let bytes = std::fs::read(&path).expect("Failed to read the fixture");
    let in_memory = || {
        RecordReader::from_reader(
            Cursor::new(bytes.clone()),
            "in-memory",
            Boundary::Separator(b"\n".to_vec()),
        )
        .expect("Failed to build the reader")
    };
    let on_file = || RecordReader::open(path.clone(), b"\n").expect("Failed to open the reader");

    assert_eq!(in_memory().label(), "in-memory");
    assert_eq!(on_file().label(), path.to_string_lossy());
    assert_eq!(in_memory().separator(), on_file().separator());
    assert_eq!(in_memory().frame_count(), on_file().frame_count());
    assert_eq!(in_memory().total_records().unwrap(), FIXTURE_RECORDS);
    for from in [0, 1, FIXTURE_RECORDS_PER_FRAME - 1, FIXTURE_RECORDS - 1] {
        let (mut memory, mut file) = (in_memory(), on_file());
        assert_eq!(memory.record(from).unwrap(), file.record(from).unwrap());
        assert_eq!(
            memory.records(from, 3).unwrap(),
            file.records(from, 3).unwrap()
        );
        let (mut memory, mut file) = (in_memory().verifying(), on_file().verifying());
        assert_eq!(memory.record(from).unwrap(), file.record(from).unwrap());
        assert_eq!(
            memory.records(from, 3).unwrap(),
            file.records(from, 3).unwrap()
        );
    }
    let all = |records: Vec<anyhow::Result<Vec<u8>>>| -> Vec<Vec<u8>> {
        records.into_iter().map(Result::unwrap).collect()
    };
    assert_eq!(
        all(in_memory().into_records().collect()),
        all(on_file().into_records().collect())
    );
    assert_eq!(
        all(in_memory().into_records().rev().collect()),
        all(on_file().into_records().rev().collect())
    );
    let (mut memory, mut file) = (Vec::new(), Vec::new());
    in_memory()
        .into_bytes()
        .unwrap()
        .read_to_end(&mut memory)
        .unwrap();
    on_file()
        .into_bytes()
        .unwrap()
        .read_to_end(&mut file)
        .unwrap();
    assert_eq!(memory, file);
}

/// A refusal names the source by the label it was built with, where the file reader names the path.
#[test]
fn a_refusal_names_the_source_by_its_label() {
    use std::io::Cursor;

    use seekzstdsep::find::Boundary;

    let dir = tempdir().expect("Failed to create temp dir");
    let groups = [b"a\nb\n".to_vec(), b"c\nd\ne\n".to_vec()];
    let path = compress_frames(dir.path(), "uneven", &groups);
    let bytes = std::fs::read(&path).expect("Failed to read the fixture");
    let in_memory = |label: &str| {
        RecordReader::from_reader(
            Cursor::new(bytes.clone()),
            label,
            Boundary::Separator(b"\n".to_vec()),
        )
        .expect("Failed to build the reader")
    };

    let past_the_end = in_memory("in-memory").records(100, 1).unwrap_err();
    assert!(
        past_the_end
            .to_string()
            .ends_with("past the end of in-memory"),
        "the refusal did not name the label: {past_the_end}"
    );
    let uneven = in_memory("uneven-frames")
        .verifying()
        .records(0, 5)
        .unwrap_err();
    assert!(
        uneven
            .to_string()
            .contains("frame 1 of uneven-frames holds"),
        "the refusal did not name the label: {uneven}"
    );
    let on_file = RecordReader::open(path.clone(), b"\n")
        .unwrap()
        .verifying()
        .records(0, 5)
        .unwrap_err();
    assert!(
        on_file
            .to_string()
            .contains(&format!("frame 1 of {} holds", path.display())),
        "the refusal did not name the path: {on_file}"
    );

    let not_seekable = RecordReader::from_reader(
        Cursor::new(Vec::new()),
        "in-memory",
        Boundary::Separator(b"\n".to_vec()),
    )
    .err()
    .expect("an empty source was accepted");
    assert!(
        not_seekable
            .to_string()
            .contains("failed to open in-memory as a seekable zst"),
        "the refusal did not name the label: {not_seekable}"
    );
    let no_record = RecordReader::from_reader(
        Cursor::new(bytes.clone()),
        "in-memory",
        Boundary::Separator(b"ZZZZ".to_vec()),
    )
    .err()
    .expect("a separator that occurs in no record was accepted");
    assert!(
        no_record.to_string().contains("frame 0 of in-memory"),
        "the refusal did not name the label: {no_record}"
    );
    let empty = RecordReader::from_reader(
        Cursor::new(bytes),
        "in-memory",
        Boundary::Separator(Vec::new()),
    )
    .err()
    .expect("an empty separator was accepted");
    let on_file = RecordReader::open(path, b"")
        .err()
        .expect("an empty separator was accepted");
    assert_eq!(empty.to_string(), on_file.to_string());
}
