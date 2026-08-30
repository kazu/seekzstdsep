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
