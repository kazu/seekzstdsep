//! `append`. `docs/design/2026-08-24-truncate-append-split-concat.md` is the specification;
//! every refusal below is one it names.

mod common;
use common::*;

use seekzstdsep::{OnMissingSeparator, append, truncate};

use std::fs::{File, OpenOptions};
use std::path::Path;
use tempfile::tempdir;

/// Appends to the file at `path`, opening it for reading and writing as the operation needs.
fn append_file(
    path: &Path,
    data: &[u8],
    separator: &[u8],
    on_missing: OnMissingSeparator,
) -> anyhow::Result<()> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("Failed to open the compressed file");
    append(&mut f, data, separator, on_missing)
}

/// The common case: newline separated, and a file that ends with one.
fn append_records(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    append_file(path, data, b"\n", OnMissingSeparator::Refuse)
}

/// A refusal that has already written is worse than no refusal, so every one is checked to have
/// left the file byte for byte as it was. Returns the error, for the refusals whose reason matters.
fn assert_refused(
    path: &Path,
    data: &[u8],
    separator: &[u8],
    on_missing: OnMissingSeparator,
) -> anyhow::Error {
    let before = std::fs::read(path).expect("Failed to read compressed file");
    let err = append_file(path, data, separator, on_missing)
        .expect_err("append returned Ok where the specification requires a refusal");
    let after = std::fs::read(path).expect("Failed to read compressed file");
    assert_eq!(before, after, "a refused append rewrote the file: {err}");
    err
}

/// `records` records split into frames of [`FIXTURE_RECORDS_PER_FRAME`], the remainder last.
fn framing_for(records: usize) -> Vec<usize> {
    let full = records / FIXTURE_RECORDS_PER_FRAME;
    let mut expected = vec![FIXTURE_RECORDS_PER_FRAME; full];
    if records % FIXTURE_RECORDS_PER_FRAME != 0 {
        expected.push(records % FIXTURE_RECORDS_PER_FRAME);
    }
    expected
}

#[test]
fn test_append_fills_the_frame_the_file_ends_with() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();
    assert_framing(&out_path, &framing_for(FIXTURE_RECORDS));

    let added: Vec<u8> = records[..10].concat();
    append_records(&out_path, &added).expect("Failed to append");

    // 600 + 10 records, so the short frame the file ended with grows from 15 records to 25.
    assert_framing(&out_path, &framing_for(FIXTURE_RECORDS + 10));
    assert_decompresses_to(&out_path, &[records.concat(), added].concat());
}

#[test]
fn test_append_spans_several_frames() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();

    let added: Vec<u8> = records[..300].concat();
    append_records(&out_path, &added).expect("Failed to append");

    assert_framing(&out_path, &framing_for(FIXTURE_RECORDS + 300));
    assert_decompresses_to(&out_path, &[records.concat(), added].concat());
}

#[test]
fn test_append_keeps_records_addressable() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();

    let added = &records[..300];
    append_records(&out_path, &added.concat()).expect("Failed to append");

    let all: Vec<Vec<u8>> = records.iter().chain(added.iter()).cloned().collect();
    // Inside an untouched frame, across the boundary into the re-encoded one, at the join itself,
    // in a frame written wholly out of appended data, and at the last record.
    for (from, cnt) in [
        (0usize, 3usize),
        (583, 4),
        (599, 2),
        (700, 3),
        (all.len() - 1, 1),
    ] {
        assert_cat_returns(&out_path, &all, from, cnt);
    }
}

#[test]
fn test_append_keeps_the_prefix_byte_for_byte() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let before = std::fs::read(&out_path).expect("Failed to read compressed file");

    // Everything but the frame the file ends with, which is the only one append may rewrite.
    let untouched = frames_of(&out_path, b"\n")[4].comp_end as usize;

    append_records(&out_path, &records[..10].concat()).expect("Failed to append");

    let after = std::fs::read(&out_path).expect("Failed to read compressed file");
    assert!(
        after.len() >= untouched,
        "append cut into frames it does not affect"
    );
    assert_eq!(
        after[..untouched],
        before[..untouched],
        "append rewrote bytes before the frame it re-encodes"
    );
}

#[test]
fn test_append_onto_a_last_frame_that_is_already_full() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let kept = 4 * FIXTURE_RECORDS_PER_FRAME;
    let records = fixture_records_upto(kept, true);
    let out_path = compress_body(temp_dir.path(), "full", &records.concat());
    assert_framing(&out_path, &[FIXTURE_RECORDS_PER_FRAME; 4]);

    let added: Vec<u8> = fixture_records()[..5].concat();
    append_records(&out_path, &added).expect("Failed to append");

    // The full frame is still re-encoded, since cutting after it is what puts a short frame in the
    // interior. It comes back holding the same records.
    assert_framing(&out_path, &framing_for(kept + 5));
    assert_decompresses_to(&out_path, &[records.concat(), added].concat());
}

#[test]
fn test_append_of_nothing_rewrites_nothing() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let before = std::fs::read(&out_path).expect("Failed to read compressed file");

    append_records(&out_path, b"").expect("Failed to append nothing");

    let after = std::fs::read(&out_path).expect("Failed to read compressed file");
    assert_eq!(before, after, "appending nothing rewrote the file");
}

/// The fixture with the last record's separator removed, so the file ends in a fragment.
fn fragment_file(dir: &Path) -> (std::path::PathBuf, Vec<u8>) {
    let records = fixture_records_upto(FIXTURE_RECORDS, false);
    let body = records.concat();
    (compress_body(dir, "fragment", &body), body)
}

#[test]
fn test_append_refuses_a_file_that_ends_in_a_fragment() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (out_path, _) = fragment_file(temp_dir.path());

    // Without the refusal the fragment and the first appended record merge into one record, and
    // every record index after the join shifts by one.
    let err = assert_refused(&out_path, b"appended\n", b"\n", OnMissingSeparator::Refuse);
    assert!(
        err.to_string().contains("fragment"),
        "refused, but not as a file ending in a fragment: {err}"
    );
}

#[test]
fn test_append_inserts_a_separator_when_asked() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (out_path, body) = fragment_file(temp_dir.path());
    let added: Vec<u8> = fixture_records()[..10].concat();

    append_file(&out_path, &added, b"\n", OnMissingSeparator::Insert).expect("Failed to append");

    // The fragment becomes a record of its own, so the file holds 600 + 10.
    assert_framing(&out_path, &framing_for(FIXTURE_RECORDS + 10));
    assert_decompresses_to(&out_path, &[body, b"\n".to_vec(), added].concat());
}

#[test]
fn test_append_onto_a_file_whose_last_frame_carries_nothing() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records: Vec<Vec<u8>> = (0..9).map(|i| format!("rec{i}\n").into_bytes()).collect();
    // A last frame carrying nothing, which zeekstd's finish() writes after an end_frame(). The
    // file ends with a whole record all the same: the record is in the frame before it.
    let out_path = compress_frames(
        temp_dir.path(),
        "empty_tail",
        &[
            records[0..3].concat(),
            records[3..6].concat(),
            records[6..9].concat(),
            Vec::new(),
        ],
    );
    assert_framing(&out_path, &[3, 3, 3, 0]);

    let added: Vec<u8> = b"x\ny\n".to_vec();
    append_records(&out_path, &added).expect("Failed to append");

    // The empty frame is gone rather than carried along, and no record was invented at the join.
    assert_framing(&out_path, &[3, 3, 3, 2]);
    assert_decompresses_to(&out_path, &[records.concat(), added].concat());
}

// A separator that overlaps itself: the bytes of one match can begin inside the previous one, so
// the last two bytes of a buffer can be a separator without the buffer ending in a whole record.
const OVERLAPPING_SEPARATOR: &[u8] = b"\n\n";

/// Thirteen records in frames of three, `fragment` adding a byte of a fourteenth to the last one.
fn overlapping_file(dir: &Path, label: &str, fragment: bool) -> (std::path::PathBuf, Vec<u8>) {
    let records: Vec<Vec<u8>> = (0..13)
        .map(|i| format!("para{i:02}\n\n").into_bytes())
        .collect();
    let mut last = records[12].clone();
    if fragment {
        last.push(b'\n');
    }
    let path = compress_frames(
        dir,
        label,
        &[
            records[0..3].concat(),
            records[3..6].concat(),
            records[6..9].concat(),
            records[9..12].concat(),
            last,
        ],
    );
    let mut body = records.concat();
    if fragment {
        body.push(b'\n');
    }
    (path, body)
}

#[test]
fn test_append_refuses_a_fragment_a_separator_could_end_inside() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (out_path, _) = overlapping_file(temp_dir.path(), "overlapping", true);
    assert_framing_with(&out_path, OVERLAPPING_SEPARATOR, &[3, 3, 3, 3, 1]);

    // The file ends "para12\n\n\n": one record and a byte over. Its last two bytes are a separator
    // all the same, so a test that only looks at them accepts it and the byte merges with the
    // first appended record.
    let err = assert_refused(
        &out_path,
        b"NEW\n\n",
        OVERLAPPING_SEPARATOR,
        OnMissingSeparator::Refuse,
    );
    assert!(
        err.to_string().contains("fragment"),
        "refused, but not as a file ending in a fragment: {err}"
    );

    // Writing one separator leaves a fragment again, so there is nothing to insert here either.
    assert_refused(
        &out_path,
        b"NEW\n\n",
        OVERLAPPING_SEPARATOR,
        OnMissingSeparator::Insert,
    );
}

#[test]
fn test_append_with_a_separator_that_overlaps_itself() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (out_path, body) = overlapping_file(temp_dir.path(), "overlapping", false);
    assert_framing_with(&out_path, OVERLAPPING_SEPARATOR, &[3, 3, 3, 3, 1]);

    let added: Vec<u8> = b"NEW0\n\nNEW1\n\n".to_vec();
    append_file(
        &out_path,
        &added,
        OVERLAPPING_SEPARATOR,
        OnMissingSeparator::Refuse,
    )
    .expect("Failed to append");

    assert_framing_with(&out_path, OVERLAPPING_SEPARATOR, &[3, 3, 3, 3, 3]);
    assert_decompresses_to(&out_path, &[body, added].concat());
}

#[test]
fn test_append_refuses_a_separator_that_does_not_occur() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    let err = assert_refused(
        &out_path,
        b"appended\r\n",
        b"\r\n",
        OnMissingSeparator::Refuse,
    );
    assert!(
        err.to_string().contains("does not occur"),
        "refused, but not as a separator that does not occur: {err}"
    );
}

#[test]
fn test_append_refuses_a_separator_with_an_uneven_count() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    // 33 times in frame 0 and 28 in frame 4. Cutting with it would cut in the wrong place.
    assert_refused(
        &out_path,
        b"\"lvl\":\"info\"",
        b"\"lvl\":\"info\"",
        OnMissingSeparator::Refuse,
    );
}

#[test]
fn test_append_refuses_a_file_with_fewer_than_three_frames() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(2 * FIXTURE_RECORDS_PER_FRAME, true);
    let out_path = compress_body(temp_dir.path(), "two_frames", &records.concat());
    assert_framing(&out_path, &[FIXTURE_RECORDS_PER_FRAME; 2]);

    assert_refused(&out_path, b"appended\n", b"\n", OnMissingSeparator::Refuse);
}

/// A file that is nothing but a seek table holding no entries, which `SeekTable::from_seekable`
/// reads back as a table of zero frames.
fn empty_seek_table(dir: &Path) -> std::path::PathBuf {
    use std::io::Write;

    let path = dir.join("bare.zst");
    let mut out = Vec::new();
    out.extend_from_slice(&0x184D_2A5Eu32.to_le_bytes()); // skippable frame magic
    out.extend_from_slice(&9u32.to_le_bytes()); // the integrity field is all that follows
    out.extend_from_slice(&0u32.to_le_bytes()); // frames
    out.push(0); // descriptor
    out.extend_from_slice(&0x8F92_EAB1u32.to_le_bytes()); // seek table magic
    File::create(&path)
        .expect("Failed to create the seek table")
        .write_all(&out)
        .expect("Failed to write the seek table");
    path
}

#[test]
fn test_append_refuses_a_file_with_no_frames() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = empty_seek_table(temp_dir.path());

    // Refused for holding fewer than three frames. Reaching that refusal is the point: addressing
    // the last frame before validation subtracts one from zero.
    assert_refused(&out_path, b"appended\n", b"\n", OnMissingSeparator::Refuse);
}

#[test]
fn test_append_after_truncate() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();

    // Truncation leaves a short final frame, which is the state append has to handle anyway.
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&out_path)
        .expect("Failed to open the compressed file");
    truncate(&mut f, 250, b"\n").expect("Failed to truncate");
    drop(f);
    assert_framing(&out_path, &framing_for(250));

    let added: Vec<u8> = records[..20].concat();
    append_records(&out_path, &added).expect("Failed to append");

    assert_framing(&out_path, &framing_for(270));
    assert_decompresses_to(&out_path, &[records[..250].concat(), added].concat());
}

#[test]
fn test_append_then_truncate_restores_the_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let before = std::fs::read(&out_path).expect("Failed to read compressed file");

    append_records(&out_path, &records[..40].concat()).expect("Failed to append");
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&out_path)
        .expect("Failed to open the compressed file");
    truncate(&mut f, FIXTURE_RECORDS as u64, b"\n").expect("Failed to truncate");
    drop(f);

    assert_framing(&out_path, &framing_for(FIXTURE_RECORDS));
    assert_decompresses_to(&out_path, &records.concat());
    assert_eq!(
        std::fs::read(&out_path).expect("Failed to read compressed file"),
        before,
        "a round trip through append and truncate did not restore the file"
    );
}

#[test]
fn test_append_data_that_does_not_end_with_a_separator() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();

    // Ten records and a fragment after them. The fragment is not a record, so the record count is
    // the one ten records imply and the fragment rides in the frame the file ends with.
    let mut added: Vec<u8> = records[..10].concat();
    added.extend_from_slice(b"{\"ts\":\"a fragment with no separator\"}");
    append_records(&out_path, &added).expect("Failed to append");

    assert_framing(&out_path, &framing_for(FIXTURE_RECORDS + 10));
    assert_decompresses_to(&out_path, &[records.concat(), added].concat());

    // And the file now ends in a fragment, so appending to it again is the refusal above.
    assert_refused(&out_path, b"more\n", b"\n", OnMissingSeparator::Refuse);
}

#[test]
fn test_append_onto_a_last_frame_larger_than_the_others() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(20, true);
    // 5, 5 and 10 records: a last frame holding more than the frames before it, which the crate's
    // own compressor cannot produce.
    let out_path = compress_frames(
        temp_dir.path(),
        "uneven",
        &[
            records[0..5].concat(),
            records[5..10].concat(),
            records[10..20].concat(),
        ],
    );
    assert_framing(&out_path, &[5, 5, 10]);

    let added: Vec<u8> = records[..3].concat();
    append_records(&out_path, &added).expect("Failed to append");

    // The oversized frame is decoded and cut again, so the file comes back holding five records in
    // every frame but the last.
    assert_framing(&out_path, &[5, 5, 5, 5, 3]);
    assert_decompresses_to(&out_path, &[records.concat(), added].concat());
}

// A record of exactly 113 bytes puts the separator of record 289 at offset 32767 of the appended
// data, straddling the 32768-byte boundary the cutter reads at. A separator found only within one
// read would be missed there, and the frame would hold one record too few.
const STRADDLING_RECORD_LEN: usize = 113;
const STRADDLING_SEPARATOR: &[u8] = b"-=-";

fn straddling_records(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            let mut record = format!("{i:0>110}").into_bytes();
            record.extend_from_slice(STRADDLING_SEPARATOR);
            assert_eq!(record.len(), STRADDLING_RECORD_LEN);
            record
        })
        .collect()
}

#[test]
fn test_append_with_a_multi_byte_separator_across_a_read_boundary() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = straddling_records(610);
    assert_eq!(
        &records[289][110..],
        STRADDLING_SEPARATOR,
        "record 289 is where the straddled separator has to be"
    );
    assert_eq!(STRADDLING_RECORD_LEN * 290, 32770);

    let out_path = compress_frames(
        temp_dir.path(),
        "straddle",
        &[
            records[0..4].concat(),
            records[4..8].concat(),
            records[8..10].concat(),
        ],
    );
    assert_framing_with(&out_path, STRADDLING_SEPARATOR, &[4, 4, 2]);

    let added: Vec<u8> = records[10..].concat();
    append_file(
        &out_path,
        &added,
        STRADDLING_SEPARATOR,
        OnMissingSeparator::Refuse,
    )
    .expect("Failed to append");

    let mut expected = vec![4usize; 152];
    expected.push(2);
    assert_framing_with(&out_path, STRADDLING_SEPARATOR, &expected);
    assert_decompresses_to(&out_path, &records.concat());
}

#[test]
fn test_append_keeps_the_checksum_the_file_already_had() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture_with_checksum(temp_dir.path(), true);

    append_records(&out_path, &fixture_records()[..300].concat()).expect("Failed to append");

    let flags = frame_checksum_flags(&out_path);
    assert!(
        flags.iter().all(|&on| on),
        "append wrote a frame without the checksum the file carries: {flags:?}"
    );
}

#[test]
fn test_append_adds_no_checksum_to_a_file_written_without_one() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture_with_checksum(temp_dir.path(), false);

    append_records(&out_path, &fixture_records()[..300].concat()).expect("Failed to append");

    let flags = frame_checksum_flags(&out_path);
    assert!(
        flags.iter().all(|&on| !on),
        "append added a checksum to a frame: {flags:?}"
    );
}
