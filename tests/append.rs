//! `append`. `docs/design/2026-08-24-truncate-append-split-concat.md` is the specification;
//! every refusal below is one it names.

mod common;
use common::*;

use seekzstdsep::{
    Alignment, AppendInput, OnMissingSeparator, RangeCheck, SeparatorCheck, append, copy_range,
    truncate,
};

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
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
    append(
        &mut f,
        AppendInput::Records {
            data,
            on_missing,
            level: 0,
        },
        separator,
    )
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

    // Truncation cuts at a boundary, so append finds a file whose last frame is full.
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&out_path)
        .expect("Failed to open the compressed file");
    truncate(&mut f, 351, b"\n").expect("Failed to truncate");
    drop(f);
    assert_framing(&out_path, &framing_for(351));

    let added: Vec<u8> = records[..20].concat();
    append_records(&out_path, &added).expect("Failed to append");

    assert_framing(&out_path, &framing_for(371));
    assert_decompresses_to(&out_path, &[records[..351].concat(), added].concat());
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

// The byte-copy path: `AppendInput::Frames`, which the CLI spells `--input-seekable`. It joins two
// files without decoding either, so what it has to establish first is that their frames already fit
// together.

/// The records the fixture fills whole frames with, which is what the byte-copy path requires of
/// the file it appends to: a last data frame that is full rather than short.
const ALIGNED_RECORDS: usize =
    FIXTURE_RECORDS_PER_FRAME * (FIXTURE_RECORDS / FIXTURE_RECORDS_PER_FRAME);

/// A frame boundary far enough from the end that both halves hold more than one frame, which is
/// what the separator check needs of the file whose frames are copied.
const DIVIDE_AT: usize = FIXTURE_RECORDS_PER_FRAME * 3;

/// The fixture compressed and then cut back to whole frames.
fn aligned_target(dir: &Path, label: &str) -> PathBuf {
    let path = compress_body(dir, label, &fixture_records().concat());
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("Failed to open the compressed file");
    truncate(&mut f, ALIGNED_RECORDS as u64, b"\n").expect("Failed to align the target");
    path
}

fn append_frames_to(
    target: &Path,
    input: &Path,
    from: u64,
    cnt: Option<u64>,
) -> anyhow::Result<()> {
    append_frames_checked(target, input, from, cnt, RangeCheck::FirstFrame)
}

fn append_frames_checked(
    target: &Path,
    input: &Path,
    from: u64,
    cnt: Option<u64>,
    check: RangeCheck,
) -> anyhow::Result<()> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(target)
        .expect("Failed to open the target");
    let src = File::open(input).expect("Failed to open the input");
    let frames: AppendInput<&[u8]> = AppendInput::Frames {
        input: &src,
        from,
        cnt,
        check,
    };
    append(&mut f, frames, b"\n")
}

fn assert_frames_refused(
    target: &Path,
    input: &Path,
    from: u64,
    cnt: Option<u64>,
) -> anyhow::Error {
    let before = std::fs::read(target).expect("Failed to read the target");
    let err = append_frames_to(target, input, from, cnt)
        .expect_err("append returned Ok where the specification requires a refusal");
    let after = std::fs::read(target).expect("Failed to read the target");
    assert_eq!(before, after, "a refused append rewrote the file: {err}");
    err
}

/// The byte the frames of `path` end at, which is where its seek table starts.
fn frames_end(path: &Path) -> usize {
    frames_of(path, b"\n")
        .last()
        .expect("a file of no frames")
        .comp_end as usize
}

#[test]
fn test_append_frames_joins_two_files() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    append_frames_to(&target, &input, 0, None).expect("Failed to append the frames");

    let records = fixture_records();
    let mut expected = framing_for(ALIGNED_RECORDS);
    expected.extend(framing_for(FIXTURE_RECORDS));
    assert_framing(&target, &expected);
    assert_decompresses_to(
        &target,
        &[records[..ALIGNED_RECORDS].concat(), records.concat()].concat(),
    );
}

#[test]
fn test_append_frames_keeps_records_addressable() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    append_frames_to(&target, &input, 0, None).expect("Failed to append the frames");

    let records = fixture_records();
    let joined: Vec<Vec<u8>> = records[..ALIGNED_RECORDS]
        .iter()
        .chain(records.iter())
        .cloned()
        .collect();
    // Either side of the seam, and the last record the result holds.
    for from in [0, ALIGNED_RECORDS - 1, ALIGNED_RECORDS, joined.len() - 1] {
        assert_cat_returns(&target, &joined, from, 1);
    }
}

#[test]
fn test_append_frames_copies_a_range() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());
    let from = FIXTURE_RECORDS_PER_FRAME;
    let cnt = 2 * FIXTURE_RECORDS_PER_FRAME;

    append_frames_to(&target, &input, from as u64, Some(cnt as u64))
        .expect("Failed to append the range");

    let records = fixture_records();
    let mut expected = framing_for(ALIGNED_RECORDS);
    expected.extend([FIXTURE_RECORDS_PER_FRAME; 2]);
    assert_framing(&target, &expected);
    assert_decompresses_to(
        &target,
        &[
            records[..ALIGNED_RECORDS].concat(),
            records[from..from + cnt].concat(),
        ]
        .concat(),
    );
}

#[test]
fn test_append_frames_keeps_the_prefix_byte_for_byte() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    let before = std::fs::read(&target).expect("Failed to read the target");
    let prefix = frames_end(&target);
    append_frames_to(&target, &input, 0, None).expect("Failed to append the frames");
    let after = std::fs::read(&target).expect("Failed to read the target");

    assert_eq!(
        before[..prefix],
        after[..prefix],
        "append rewrote a byte of the frames it was supposed to leave alone"
    );
}

#[test]
fn test_append_frames_of_nothing_rewrites_nothing() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    let before = std::fs::read(&target).expect("Failed to read the target");
    append_frames_to(&target, &input, 0, Some(0)).expect("Failed to append an empty range");
    let after = std::fs::read(&target).expect("Failed to read the target");

    assert_eq!(before, after, "an empty range rewrote the file");
}

#[test]
fn test_append_frames_leaves_a_short_input_frame_last() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    // The input ends in a short frame, which is legal: it becomes the last frame of the result.
    append_frames_to(&target, &input, 0, None).expect("Failed to append the frames");
    let framing = frames_of(&target, b"\n");
    let last = framing.last().expect("no frames").cnt_of_sep;
    assert_eq!(
        last,
        FIXTURE_RECORDS % FIXTURE_RECORDS_PER_FRAME,
        "the input's short frame did not survive as the last frame of the result"
    );

    // The result is no longer aligned, so a second one refuses.
    let err = assert_frames_refused(&target, &input, 0, None);
    assert!(
        err.to_string().contains("rather than"),
        "the refusal does not say the last frame is short: {err}"
    );
}

#[test]
fn test_append_frames_refuses_a_target_whose_last_frame_is_short() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = compress_body(temp_dir.path(), "target", &fixture_records().concat());
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    let err = assert_frames_refused(&target, &input, 0, None);

    assert!(
        err.to_string()
            .contains(&format!("{}", FIXTURE_RECORDS % FIXTURE_RECORDS_PER_FRAME)),
        "the refusal does not say what the last frame holds: {err}"
    );
}

#[test]
fn test_append_frames_refuses_a_target_that_ends_in_a_fragment() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (target, _) = fragment_file(temp_dir.path());
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    let err = assert_frames_refused(&target, &input, 0, None);

    assert!(
        err.to_string().contains("whole record"),
        "the refusal does not say the file ends in a fragment: {err}"
    );
}

#[test]
fn test_append_frames_refuses_a_different_record_count_per_frame() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let groups: Vec<Vec<u8>> = fixture_records()[..40]
        .chunks(10)
        .map(<[Vec<u8>]>::concat)
        .collect();
    let input = compress_frames(temp_dir.path(), "input", &groups);

    let err = assert_frames_refused(&target, &input, 0, None);

    assert!(
        err.to_string().contains("10") && err.to_string().contains("117"),
        "the refusal does not name both counts: {err}"
    );
}

#[test]
fn test_append_frames_refuses_a_from_that_is_not_a_frame_boundary() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    let err = assert_frames_refused(&target, &input, 1, Some(FIXTURE_RECORDS_PER_FRAME as u64));

    assert!(
        err.to_string().contains("first record of a frame"),
        "the refusal does not say the range has to start at a frame: {err}"
    );
}

#[test]
fn test_append_frames_refuses_an_end_that_is_not_a_frame_boundary() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    let err = assert_frames_refused(&target, &input, 0, Some(1));

    assert!(
        err.to_string()
            .contains("ends at the first record of a frame"),
        "the refusal does not say where a range may end: {err}"
    );
}

#[test]
fn test_append_frames_refuses_an_input_that_is_not_a_seekable_zst() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let plain = temp_dir.path().join("target.jsonl");

    let err = assert_frames_refused(&target, &plain, 0, None);

    assert!(
        err.to_string().contains("not a seekable zst"),
        "the refusal does not say what is wrong with the input: {err}"
    );
}

#[test]
fn test_copy_range_then_truncate_then_append_frames_restores_the_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let original = compress_body(temp_dir.path(), "original", &fixture_records().concat());
    let before = std::fs::read(&original).expect("Failed to read the original");

    // Divide at a frame boundary: the tail into a file of its own, then the front cut back to it.
    let back = temp_dir.path().join("back.seek.zst");
    copy_range(
        &File::open(&original).expect("Failed to open the original"),
        &mut File::create(&back).expect("Failed to create the back half"),
        DIVIDE_AT as u64,
        None,
        b"\n",
        Alignment::NotRequired,
        SeparatorCheck::FirstFrame,
    )
    .expect("Failed to copy the tail out");
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&original)
        .expect("Failed to open the original");
    truncate(&mut f, DIVIDE_AT as u64, b"\n").expect("Failed to truncate");
    drop(f);

    append_frames_to(&original, &back, 0, None).expect("Failed to join the halves back");

    let after = std::fs::read(&original).expect("Failed to read the original");
    assert_eq!(
        before, after,
        "dividing a file and joining it back did not reproduce it byte for byte"
    );
}

#[test]
fn test_append_frames_refuses_an_input_of_one_data_frame() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = temp_dir.path().join("one.seek.zst");
    copy_range(
        &File::open(&target).expect("Failed to open the target"),
        &mut File::create(&input).expect("Failed to create the one-frame file"),
        0,
        Some(FIXTURE_RECORDS_PER_FRAME as u64),
        b"\n",
        Alignment::Required,
        SeparatorCheck::FirstFrame,
    )
    .expect("Failed to copy one frame out");

    let err = assert_frames_refused(&target, &input, 0, None);

    assert!(
        err.to_string().contains("one data frame"),
        "the refusal does not say why one frame cannot be checked: {err}"
    );
}

/// An input whose interior frame holds a count of its own, which the compressor never writes but
/// another writer might.
fn drifting_input(dir: &Path) -> PathBuf {
    let records = fixture_records();
    let groups: Vec<Vec<u8>> = [
        &records[..FIXTURE_RECORDS_PER_FRAME],
        &records[FIXTURE_RECORDS_PER_FRAME..FIXTURE_RECORDS_PER_FRAME + 50],
        &records[FIXTURE_RECORDS_PER_FRAME + 50..2 * FIXTURE_RECORDS_PER_FRAME + 50],
    ]
    .iter()
    .map(|g| g.concat())
    .collect();
    compress_frames(dir, "drift", &groups)
}

#[test]
fn test_append_frames_refuses_a_drifting_range_when_every_frame_is_checked() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = drifting_input(temp_dir.path());

    let before = std::fs::read(&target).expect("Failed to read the target");
    let err = append_frames_checked(&target, &input, 0, None, RangeCheck::EveryFrame)
        .expect_err("append copied a frame holding a count of its own");
    assert_eq!(
        before,
        std::fs::read(&target).expect("Failed to read the target"),
        "a refused append rewrote the file: {err}"
    );
    assert!(
        err.to_string().contains("50"),
        "the refusal does not say what the frame holds: {err}"
    );
}

#[test]
fn test_append_frames_reads_frame_zero_alone_by_default() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = drifting_input(temp_dir.path());

    // The cheap check reads the count off frame 0 and takes the rest on trust, which is what
    // RangeCheck::EveryFrame above is there to stop paying for by default. Pinned so that making
    // the expensive check unconditional cannot pass unnoticed.
    append_frames_checked(&target, &input, 0, None, RangeCheck::FirstFrame)
        .expect("the default check refused a range it does not read");
}

#[test]
fn test_append_frames_onto_a_target_carrying_an_empty_frame() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records();
    let mut groups: Vec<Vec<u8>> = records[..3 * FIXTURE_RECORDS_PER_FRAME]
        .chunks(FIXTURE_RECORDS_PER_FRAME)
        .map(<[Vec<u8>]>::concat)
        .collect();
    // A frame carrying nothing, which `Encoder::finish` leaves behind an `end_frame` and which the
    // cut has to take with it rather than leave in the interior of the result.
    groups.push(Vec::new());
    let target = compress_frames(temp_dir.path(), "target", &groups);
    let (_, empty) = empty_frames(&target);
    assert!(
        !empty.is_empty(),
        "the target carries no empty frame, so this tests nothing"
    );
    let input = compress_body(temp_dir.path(), "input", &records.concat());

    append_frames_to(&target, &input, 0, None).expect("Failed to append the frames");

    let (_, empty) = empty_frames(&target);
    assert!(
        empty.is_empty(),
        "the target's empty frame survived into the result at {empty:?}"
    );
    assert_decompresses_to(
        &target,
        &[
            records[..3 * FIXTURE_RECORDS_PER_FRAME].concat(),
            records.concat(),
        ]
        .concat(),
    );
}

#[test]
fn test_append_frames_of_an_aligned_input_can_be_appended_to_again() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    let input = aligned_target(temp_dir.path(), "input");

    // Both aligned, so the result is aligned too and takes a second join.
    append_frames_to(&target, &input, 0, None).expect("Failed to append the frames");
    append_frames_to(&target, &input, 0, None).expect("an aligned result refused a second join");

    assert_framing(&target, &[FIXTURE_RECORDS_PER_FRAME; 15]);
    let records = fixture_records();
    let once = records[..ALIGNED_RECORDS].concat();
    assert_decompresses_to(&target, &[once.clone(), once.clone(), once].concat());
}

#[test]
fn test_append_frames_checking_every_frame_takes_a_uniform_range() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    // Ends in a short frame, which the check has to accept: it becomes the last frame of the
    // result, which is where a short frame belongs. Refusing it would make the flag unusable on
    // anything the compressor writes.
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());

    append_frames_checked(&target, &input, 0, None, RangeCheck::EveryFrame)
        .expect("checking every frame refused an ordinary file");

    let records = fixture_records();
    let mut expected = framing_for(ALIGNED_RECORDS);
    expected.extend(framing_for(FIXTURE_RECORDS));
    assert_framing(&target, &expected);
    assert_decompresses_to(
        &target,
        &[records[..ALIGNED_RECORDS].concat(), records.concat()].concat(),
    );
}

/// Records that do not compress, so a frame of them stays large after compression.
fn incompressible_records(count: usize, per_record: usize) -> Vec<Vec<u8>> {
    // A cheap xorshift rather than a dependency: what matters is only that zstd cannot shrink it.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..count)
        .map(|_| {
            let mut record = Vec::with_capacity(per_record + 1);
            while record.len() < per_record {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                record.extend_from_slice(format!("{state:016x}").as_bytes());
            }
            record.truncate(per_record);
            record.push(b'\n');
            record
        })
        .collect()
}

#[test]
fn test_append_onto_frames_larger_than_the_decoder_reads_at_once() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    // Frames of a few hundred kilobytes, which is past what the decoder holds in its input buffer.
    // A decoder kept open across frames only re-seeks when the frame it is asked for moves, so
    // anything that moves the file's position between two reads corrupts the second one — and on
    // frames small enough to sit in that buffer it does not show.
    let records = incompressible_records(24, 64 * 1024);
    let groups: Vec<Vec<u8>> = records.chunks(6).map(<[Vec<u8>]>::concat).collect();
    let path = compress_frames(temp_dir.path(), "large", &groups);

    append_records(&path, b"appended\n").expect("Failed to append to a file of large frames");

    let mut expected: Vec<Vec<u8>> = records.clone();
    expected.push(b"appended\n".to_vec());
    assert_decompresses_to(&path, &expected.concat());
    assert_cat_returns(&path, &expected, records.len(), 1);
}

#[test]
fn test_append_frames_refuses_a_short_frame_that_ends_the_range_but_not_the_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = aligned_target(temp_dir.path(), "target");
    // Framed [117, 50, 117]. A range of the first two ends at the short one, which would be the
    // last frame of the result and so lands nowhere illegal — but it holds 50 where 117 was asked
    // for, so the range delivers 167 records rather than the 234 named.
    let input = drifting_input(temp_dir.path());
    let cnt = (2 * FIXTURE_RECORDS_PER_FRAME) as u64;

    let before = std::fs::read(&target).expect("Failed to read the target");
    let err = append_frames_checked(&target, &input, 0, Some(cnt), RangeCheck::EveryFrame)
        .expect_err("append delivered fewer records than the range named");
    assert_eq!(
        before,
        std::fs::read(&target).expect("Failed to read the target"),
        "a refused append rewrote the file: {err}"
    );
    assert!(
        err.to_string().contains("50"),
        "the refusal does not say what the frame holds: {err}"
    );
}

/// A reader that hands back one byte per call, which a pipe is free to do and a file never does.
struct OneByteAtATime<R>(R);

impl<R: std::io::Read> std::io::Read for OneByteAtATime<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.0.read(&mut buf[..1])
    }
}

#[test]
fn test_append_refuses_a_zstd_stream_that_arrives_a_byte_at_a_time() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let target = compress_body(temp_dir.path(), "target", &fixture_records().concat());
    let input = compress_body(temp_dir.path(), "input", &fixture_records().concat());
    let compressed = std::fs::read(&input).expect("Failed to read the compressed input");

    // The magic number spans four bytes, so a reader handing back one at a time is what pins that
    // they are gathered rather than judged on whatever the first read happened to bring.
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)
        .expect("Failed to open the target");
    let before = std::fs::read(&target).expect("Failed to read the target");
    let err = append(
        &mut f,
        AppendInput::Records {
            data: OneByteAtATime(&compressed[..]),
            on_missing: OnMissingSeparator::Refuse,
            level: 0,
        },
        b"\n",
    )
    .expect_err("append took a zstd stream as records because it arrived a byte at a time");

    assert!(
        err.to_string().contains("zstd stream"),
        "the refusal does not say what the input is: {err}"
    );
    assert_eq!(
        before,
        std::fs::read(&target).expect("Failed to read the target"),
        "a refused append rewrote the file"
    );
}

/// The level given to append has to reach the encoder: appending the same records at levels 1 and
/// 19 disagrees on the appended bytes, while both results still hold the records.
#[test]
fn test_append_level_reaches_the_encoder() {
    let records = fixture_records();
    let added: Vec<u8> = records[..300].concat();

    let append_at = |level: i32| -> Vec<u8> {
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let out_path = compress_fixture(temp_dir.path());
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&out_path)
            .expect("Failed to open the compressed file");
        append(
            &mut f,
            AppendInput::Records {
                data: added.as_slice(),
                on_missing: OnMissingSeparator::Refuse,
                level,
            },
            b"\n",
        )
        .expect("Failed to append");
        drop(f);

        assert_decompresses_to(&out_path, &[records.concat(), added.clone()].concat());
        std::fs::read(&out_path).expect("Failed to read compressed file")
    };

    assert_ne!(
        append_at(1),
        append_at(19),
        "levels 1 and 19 wrote identical bytes, so the level never reached the encoder"
    );
}
