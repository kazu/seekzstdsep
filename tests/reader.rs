//! `RecordReader`: the same records `cat_data` returns, plus the ones it cannot address one at a
//! time.
mod common;

use common::{
    FIXTURE_RECORDS, FIXTURE_RECORDS_PER_FRAME, compress_body, compress_fixture, fixture_records,
    fixture_records_upto,
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

/// Reading forwards and backwards returns the same records: the one-frame cache must not depend on
/// the order it is asked in.
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
