//! `truncate`. `docs/design/2026-08-24-truncate-append-split-concat.md` is the specification;
//! every refusal below is one it names.

mod common;
use common::*;

use seekzstdsep::{InspectOptions, seekzstdsep_lib::inspect_with_opts, truncate};

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// Truncates the file at `path`, opening it for reading and writing as the operation needs.
fn truncate_file(path: &Path, record_len: u64, separator: &[u8]) -> anyhow::Result<()> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("Failed to open the compressed file");
    truncate(&mut f, record_len, separator)
}

/// A refusal that has already written is worse than no refusal, so every one is checked to have
/// left the file byte for byte as it was. Returns the error, for the refusals whose reason matters.
fn assert_refused(path: &Path, record_len: u64, separator: &[u8]) -> anyhow::Error {
    let before = std::fs::read(path).expect("Failed to read compressed file");
    let err = truncate_file(path, record_len, separator)
        .expect_err("truncate returned Ok where the specification requires a refusal");
    let after = std::fs::read(path).expect("Failed to read compressed file");
    assert_eq!(before, after, "a refused truncate rewrote the file: {err}");
    err
}

/// The state `truncate` has to leave: `expected` read back, and a final frame that holds at least
/// one record, since a truncation always cuts just after a separator.
fn assert_truncated_to(path: &Path, expected: &[u8]) {
    assert_truncated_to_with(path, b"\n", expected);
}

fn assert_truncated_to_with(path: &Path, separator: &[u8], expected: &[u8]) {
    let frames = frames_of(path, separator);
    let last = frames.last().expect("the file has no frames");
    assert!(
        last.cnt_of_sep > 0,
        "the last frame of {} holds no record, only {} bytes of fragment",
        path.display(),
        last.decomp_size
    );

    assert_decompresses_to(path, expected);
}

#[test]
fn test_truncate_at_a_frame_boundary() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let kept = 2 * FIXTURE_RECORDS_PER_FRAME;

    truncate_file(&out_path, kept as u64, b"\n").expect("Failed to truncate");

    assert_framing(&out_path, &[FIXTURE_RECORDS_PER_FRAME; 2]);
    assert_truncated_to(&out_path, &records[..kept].concat());
}

#[test]
fn test_truncate_keeps_records_addressable() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let kept = 2 * FIXTURE_RECORDS_PER_FRAME;

    truncate_file(&out_path, kept as u64, b"\n").expect("Failed to truncate");

    // Inside frame 0, across the 0/1 boundary, at the start of frame 1, and the last record kept.
    for (from, cnt) in [(0usize, 3usize), (116, 3), (117, 2), (kept - 1, 1)] {
        assert_cat_returns(&out_path, &records[..kept], from, cnt);
    }
}

#[test]
fn test_truncate_keeps_the_prefix_byte_for_byte() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let before = std::fs::read(&out_path).expect("Failed to read compressed file");

    let frames = inspect_with_opts(out_path.clone(), b"\n", InspectOptions { fast_mode: false })
        .expect("Failed to inspect zst file");
    let untouched = frames[1].comp_end as usize;

    truncate_file(&out_path, 2 * FIXTURE_RECORDS_PER_FRAME as u64, b"\n")
        .expect("Failed to truncate");

    let after = std::fs::read(&out_path).expect("Failed to read compressed file");
    assert!(
        after.len() >= untouched,
        "truncate cut into frames it does not affect"
    );
    assert_eq!(
        after[..untouched],
        before[..untouched],
        "truncate rewrote bytes before the cut"
    );
}

#[test]
fn test_truncate_drops_a_trailing_fragment() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    // The fixture with the last record's separator removed: 599 records and a fragment after them.
    let records = fixture_records_upto(FIXTURE_RECORDS, false);
    let out_path = compress_body(temp_dir.path(), "fragment", &records.concat());
    let kept = 5 * FIXTURE_RECORDS_PER_FRAME;

    // The fragment shares the last frame with the records before it, so dropping it is dropping
    // that frame: the cut is the last boundary before the fragment.
    truncate_file(&out_path, kept as u64, b"\n").expect("Failed to truncate");

    assert_framing(&out_path, &[FIXTURE_RECORDS_PER_FRAME; 5]);
    assert_truncated_to(&out_path, &records[..kept].concat());
}

#[test]
fn test_truncate_refuses_a_cut_inside_a_frame() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    // Two whole frames and 16 records of the third.
    let err = assert_refused(&out_path, 250, b"\n");
    assert!(
        err.to_string().contains("frame boundary"),
        "refused, but not for the cut missing a frame boundary: {err}"
    );
}

#[test]
fn test_truncate_refuses_zero() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    // A zero-frame file makes seek_table_decomp_frames return None and panics every reader.
    assert_refused(&out_path, 0, b"\n");
}

#[test]
fn test_truncate_refuses_past_the_last_record() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    assert_refused(&out_path, FIXTURE_RECORDS as u64 + 1, b"\n");
    // A whole number of frames past the end, which without the check lands on a frame boundary and
    // quietly leaves the file at its current length instead.
    assert_refused(&out_path, 6 * FIXTURE_RECORDS_PER_FRAME as u64, b"\n");
}

#[test]
fn test_truncate_refuses_a_separator_that_does_not_occur() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    // The reason matters: a separator that occurs nowhere also makes the record count read as
    // zero, and the length check would then refuse the same call for the wrong reason.
    let err = assert_refused(&out_path, 234, b"\r\n");
    assert!(
        err.to_string().contains("does not occur"),
        "refused, but not as a separator that does not occur: {err}"
    );
}

#[test]
fn test_truncate_refuses_a_separator_with_an_uneven_count() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    // Present in both compared frames, 33 times in frame 0 and 28 in frame 4. Cutting with it
    // would cut in the wrong place.
    assert_refused(&out_path, 20, b"\"lvl\":\"info\"");
}

#[test]
fn test_truncate_refuses_a_count_that_holds_only_near_the_front() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    // 28 times in frames 0 through 3 and 32 in frame 4. Comparing frame 0 against frame 1 accepts
    // this; the comparison reaches the last frame that has to hold a full count instead.
    assert_refused(&out_path, 20, b"\"lvl\":\"debug\"");
}

#[test]
fn test_truncate_refuses_a_file_with_fewer_than_three_frames() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    // Two whole frames, so validation would find matching counts and accept. It refuses anyway:
    // with two frames the second is the one allowed to be short, and a file cut with the wrong
    // separator is indistinguishable from this one.
    let records = fixture_records_upto(2 * FIXTURE_RECORDS_PER_FRAME, true);
    let out_path = compress_body(temp_dir.path(), "two_frames", &records.concat());
    assert_framing(&out_path, &[FIXTURE_RECORDS_PER_FRAME; 2]);

    assert_refused(&out_path, 150, b"\n");
}

#[test]
fn test_truncate_refuses_less_than_one_frame() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    // The first boundary is a whole frame, so no shorter length can remain.
    assert_refused(&out_path, 10, b"\n");
}

#[test]
fn test_truncate_drops_a_last_frame_that_holds_no_records() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(2 * FIXTURE_RECORDS_PER_FRAME, true);

    // Two whole frames, so a fragment after them gets a third frame to itself and that frame holds
    // no record at all. Truncating to the records the file reports has to leave it behind.
    let mut body: Vec<u8> = records.concat();
    body.extend_from_slice(b"{\"ts\":\"a fragment with no separator\"}");
    let out_path = compress_body(temp_dir.path(), "no_records", &body);
    assert_framing(
        &out_path,
        &[FIXTURE_RECORDS_PER_FRAME, FIXTURE_RECORDS_PER_FRAME, 0],
    );

    truncate_file(&out_path, 2 * FIXTURE_RECORDS_PER_FRAME as u64, b"\n")
        .expect("Failed to truncate");

    assert_framing(&out_path, &[FIXTURE_RECORDS_PER_FRAME; 2]);
    assert_truncated_to(&out_path, &records.concat());
    assert_cat_returns(&out_path, &records, 2 * FIXTURE_RECORDS_PER_FRAME - 1, 1);
}

/// 5, 5 and 10 records: a last frame holding more than the frames before it.
fn uneven_file(dir: &Path, label: &str, records: &[Vec<u8>]) -> PathBuf {
    compress_frames(
        dir,
        label,
        &[
            records[0..5].concat(),
            records[5..10].concat(),
            records[10..20].concat(),
        ],
    )
}

#[test]
fn test_truncate_a_last_frame_larger_than_the_others() {
    let records = fixture_records_upto(20, true);
    let temp_dir = tempdir().expect("Failed to create temp dir");
    assert_framing(
        &uneven_file(temp_dir.path(), "uneven", &records),
        &[5, 5, 10],
    );

    // The boundaries are the ends of the two uniform frames. A multiple of 5 inside the oversized
    // last frame is not one, and neither is the end of the file it makes.
    for keep in [5usize, 10] {
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let out_path = uneven_file(temp_dir.path(), &format!("keep-{keep}"), &records);

        truncate_file(&out_path, keep as u64, b"\n")
            .unwrap_or_else(|e| panic!("truncate to {keep} records failed: {e}"));

        assert_truncated_to(&out_path, &records[..keep].concat());
    }
    for keep in [3u64, 15, 20] {
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let out_path = uneven_file(temp_dir.path(), &format!("refuse-{keep}"), &records);
        assert_refused(&out_path, keep, b"\n");
    }
}

#[test]
fn test_truncate_a_last_frame_larger_than_the_others_refuses_out_of_range() {
    let records = fixture_records_upto(20, true);
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = uneven_file(temp_dir.path(), "uneven", &records);

    assert_refused(&out_path, 0, b"\n");
    assert_refused(&out_path, 21, b"\n");
}

#[test]
fn test_truncate_with_a_multi_byte_separator() {
    const SEP: &[u8] = b"-=-";
    let records: Vec<Vec<u8>> = (0..10)
        .map(|i| format!("record {i}-=-").into_bytes())
        .collect();

    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_frames(
        temp_dir.path(),
        "multibyte",
        &[
            records[0..4].concat(),
            records[4..8].concat(),
            records[8..10].concat(),
        ],
    );
    assert_framing_with(&out_path, SEP, &[4, 4, 2]);

    truncate_file(&out_path, 4, SEP).expect("Failed to truncate");

    assert_framing_with(&out_path, SEP, &[4]);
    assert_truncated_to_with(&out_path, SEP, &records[..4].concat());
}

#[test]
fn test_truncate_with_one_record_per_frame() {
    let records = fixture_records_upto(5, true);
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_frames(
        temp_dir.path(),
        "single",
        &records.iter().cloned().collect::<Vec<_>>(),
    );
    assert_framing(&out_path, &[1; 5]);

    // Every frame holds one record, so every count is a frame boundary.
    truncate_file(&out_path, 3, b"\n").expect("Failed to truncate");

    assert_framing(&out_path, &[1; 3]);
    assert_truncated_to(&out_path, &records[..3].concat());
}
