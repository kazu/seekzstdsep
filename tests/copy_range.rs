//! `copy_range`. `docs/design/2026-08-24-truncate-append-split-concat.md` is the specification;
//! every refusal below is one it names.

mod common;
use common::*;

use seekzstdsep::{Alignment, SeparatorCheck, copy_range, truncate};

use std::fs::File;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// Copies a record range out of `input` into `dir/label.seek.zst`, which is the shape the CLI uses.
fn copy_to(
    dir: &Path,
    label: &str,
    input: &Path,
    from: u64,
    cnt: Option<u64>,
) -> anyhow::Result<PathBuf> {
    copy_to_with(dir, label, input, from, cnt, b"\n", Alignment::Required)
}

fn copy_to_with(
    dir: &Path,
    label: &str,
    input: &Path,
    from: u64,
    cnt: Option<u64>,
    separator: &[u8],
    align: Alignment,
) -> anyhow::Result<PathBuf> {
    let out_path = dir.join(format!("{label}.seek.zst"));
    let src = File::open(input).expect("Failed to open the input file");
    let mut out = File::create(&out_path).expect("Failed to create the output file");
    copy_range(
        &src,
        &mut out,
        from,
        cnt,
        separator,
        align,
        SeparatorCheck::FirstFrame,
    )?;
    Ok(out_path)
}

/// A refusal writes nothing and leaves the input byte for byte as it was. Returns the error, for
/// the refusals whose reason matters.
fn assert_refused(input: &Path, from: u64, cnt: Option<u64>, align: Alignment) -> anyhow::Error {
    assert_refused_with(input, from, cnt, b"\n", align, SeparatorCheck::FirstFrame)
}

fn assert_refused_with(
    input: &Path,
    from: u64,
    cnt: Option<u64>,
    separator: &[u8],
    align: Alignment,
    check: SeparatorCheck,
) -> anyhow::Error {
    let before = std::fs::read(input).expect("Failed to read the input file");
    let src = File::open(input).expect("Failed to open the input file");
    let mut out = Vec::new();
    let err = copy_range(&src, &mut out, from, cnt, separator, align, check)
        .expect_err("copy_range returned Ok where the specification requires a refusal");
    let after = std::fs::read(input).expect("Failed to read the input file");
    assert_eq!(
        before, after,
        "a refused copy_range rewrote its input: {err}"
    );
    err
}

/// The frames the fixture is compressed into: five full ones and a short last one.
fn fixture_framing() -> Vec<usize> {
    let mut framing = vec![FIXTURE_RECORDS_PER_FRAME; FIXTURE_RECORDS / FIXTURE_RECORDS_PER_FRAME];
    framing.push(FIXTURE_RECORDS % FIXTURE_RECORDS_PER_FRAME);
    framing
}

#[test]
fn test_copy_range_copies_whole_frames() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let from = FIXTURE_RECORDS_PER_FRAME;
    let cnt = 2 * FIXTURE_RECORDS_PER_FRAME;

    let out_path = copy_to(
        temp_dir.path(),
        "range",
        &input,
        from as u64,
        Some(cnt as u64),
    )
    .expect("Failed to copy the range");

    assert_framing(&out_path, &[FIXTURE_RECORDS_PER_FRAME; 2]);
    assert_decompresses_to(&out_path, &records[from..from + cnt].concat());
}

#[test]
fn test_copy_range_refuses_a_from_that_is_not_a_frame_boundary() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());

    let err = assert_refused(&input, 1, Some(117), Alignment::Required);

    assert!(
        err.to_string().contains("frame"),
        "the refusal does not say the range has to start at a frame: {err}"
    );
}

#[test]
fn test_copy_range_refuses_an_end_that_is_not_a_frame_boundary() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());

    assert_refused(
        &input,
        FIXTURE_RECORDS_PER_FRAME as u64,
        Some(16),
        Alignment::Required,
    );
}

#[test]
fn test_copy_range_refuses_a_count_of_zero() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());

    assert_refused(&input, 0, Some(0), Alignment::Required);
}

#[test]
fn test_copy_range_refuses_a_range_past_the_last_record() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());

    assert_refused(
        &input,
        0,
        Some(6 * FIXTURE_RECORDS_PER_FRAME as u64),
        Alignment::NotRequired,
    );
}

#[test]
fn test_copy_range_refuses_a_start_past_the_last_record() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());

    assert_refused(
        &input,
        6 * FIXTURE_RECORDS_PER_FRAME as u64,
        Some(FIXTURE_RECORDS_PER_FRAME as u64),
        Alignment::NotRequired,
    );
}

/// A `cnt` that wraps the sum with `from` is a count past the end like any other, and gets the
/// refusal that says so. The second value below wraps to exactly a frame boundary, which the
/// arithmetic placing the end would otherwise take for one.
#[test]
fn test_copy_range_refuses_a_count_that_wraps_the_sum() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let from = FIXTURE_RECORDS_PER_FRAME as u64;

    for cnt in [u64::MAX, u64::MAX - from + 1] {
        let err = assert_refused(&input, from, Some(cnt), Alignment::NotRequired);
        assert!(
            err.to_string()
                .contains("a range ends at the first record of a frame"),
            "the refusal of {cnt} records does not say where a range ends: {err}"
        );
    }
}

#[test]
fn test_copy_range_refuses_the_short_final_frame_by_default() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());

    let err = assert_refused(
        &input,
        2 * FIXTURE_RECORDS_PER_FRAME as u64,
        None,
        Alignment::Required,
    );

    assert!(
        err.to_string().contains("15"),
        "the refusal does not say how many records the short frame holds: {err}"
    );
}

#[test]
fn test_copy_range_copies_the_short_final_frame_when_allowed() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let from = 2 * FIXTURE_RECORDS_PER_FRAME;

    let out_path = copy_to_with(
        temp_dir.path(),
        "tail",
        &input,
        from as u64,
        None,
        b"\n",
        Alignment::NotRequired,
    )
    .expect("Failed to copy the range");

    assert_framing(&out_path, &fixture_framing()[2..]);
    assert_decompresses_to(&out_path, &records[from..].concat());
}

#[test]
fn test_copy_range_ends_at_the_last_full_frame() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let cnt = 5 * FIXTURE_RECORDS_PER_FRAME;

    let out_path = copy_to(temp_dir.path(), "head", &input, 0, Some(cnt as u64))
        .expect("Failed to copy the range");

    assert_framing(&out_path, &[FIXTURE_RECORDS_PER_FRAME; 5]);
    assert_decompresses_to(&out_path, &records[..cnt].concat());
}

#[test]
fn test_copy_range_takes_the_record_count_of_the_whole_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let records = fixture_records();

    let out_path = copy_to_with(
        temp_dir.path(),
        "all",
        &input,
        0,
        Some(FIXTURE_RECORDS as u64),
        b"\n",
        Alignment::NotRequired,
    )
    .expect("Failed to copy the range");

    assert_framing(&out_path, &fixture_framing());
    assert_decompresses_to(&out_path, &records.concat());
}

#[test]
fn test_copy_range_keeps_records_addressable() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let from = FIXTURE_RECORDS_PER_FRAME;
    let cnt = 3 * FIXTURE_RECORDS_PER_FRAME;

    let out_path = copy_to(
        temp_dir.path(),
        "range",
        &input,
        from as u64,
        Some(cnt as u64),
    )
    .expect("Failed to copy the range");

    let copied = &records[from..from + cnt];
    // The first record of the range, across the first frame boundary of the result, and the last.
    for (at, take) in [(0usize, 3usize), (116, 3), (117, 2), (cnt - 1, 1)] {
        assert_cat_returns(&out_path, copied, at, take);
    }
}

#[test]
fn test_copy_range_leaves_the_input_byte_for_byte() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let before = std::fs::read(&input).expect("Failed to read the input file");

    copy_to(
        temp_dir.path(),
        "range",
        &input,
        FIXTURE_RECORDS_PER_FRAME as u64,
        Some(FIXTURE_RECORDS_PER_FRAME as u64),
    )
    .expect("Failed to copy the range");

    let after = std::fs::read(&input).expect("Failed to read the input file");
    assert_eq!(before, after, "copy_range rewrote the file it reads");
}

#[test]
fn test_copy_range_keeps_the_checksum_the_file_was_written_with() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    for checksum in [true, false] {
        let input = compress_fixture_with_checksum(temp_dir.path(), checksum);
        let label = format!("copy_{checksum}");

        let out_path = copy_to(
            temp_dir.path(),
            &label,
            &input,
            FIXTURE_RECORDS_PER_FRAME as u64,
            Some(2 * FIXTURE_RECORDS_PER_FRAME as u64),
        )
        .expect("Failed to copy the range");

        assert_eq!(
            frame_checksum_flags(&out_path),
            vec![checksum; 2],
            "a copied frame carries a checksum the input frame does not, or the other way round"
        );
    }
}

/// A file whose every frame holds the same record count, which is what `copy_range` produces and
/// what the compressor cannot: its last frame holds whatever is left over.
fn aligned_file(dir: &Path, label: &str, records: &[Vec<u8>], per_frame: usize) -> PathBuf {
    let groups: Vec<Vec<u8>> = records.chunks(per_frame).map(|g| g.concat()).collect();
    compress_frames(dir, label, &groups)
}

#[test]
fn test_copy_range_reaches_the_end_of_an_aligned_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(4 * 10, true);
    let input = aligned_file(temp_dir.path(), "aligned", &records, 10);

    let out_path = copy_to(temp_dir.path(), "copy", &input, 10, None)
        .expect("Failed to copy to the end of an aligned file");

    assert_framing(&out_path, &[10; 3]);
    assert_decompresses_to(&out_path, &records[10..].concat());
}

#[test]
fn test_copy_range_then_truncate_divides_a_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let boundary = 3 * FIXTURE_RECORDS_PER_FRAME;

    let back = copy_to_with(
        temp_dir.path(),
        "back",
        &input,
        boundary as u64,
        None,
        b"\n",
        Alignment::NotRequired,
    )
    .expect("Failed to copy the back half");
    let mut front = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&input)
        .expect("Failed to open the compressed file");
    truncate(&mut front, boundary as u64, b"\n").expect("Failed to truncate the front half");

    assert_decompresses_to(&input, &records[..boundary].concat());
    assert_decompresses_to(&back, &records[boundary..].concat());
}

#[test]
fn test_copy_range_writes_to_a_writer_that_is_not_a_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let src = File::open(&input).expect("Failed to open the input file");
    let mut out = Vec::new();

    copy_range(
        &src,
        &mut out,
        0,
        Some(FIXTURE_RECORDS_PER_FRAME as u64),
        b"\n",
        Alignment::Required,
        SeparatorCheck::FirstFrame,
    )
    .expect("Failed to copy the range");

    let same = temp_dir.path().join("stdout.seek.zst");
    std::fs::write(&same, &out).expect("Failed to write what the copy produced");
    assert_framing(&same, &[FIXTURE_RECORDS_PER_FRAME]);
}

#[test]
fn test_copy_range_refuses_a_separator_that_does_not_occur() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());

    let src = File::open(&input).expect("Failed to open the input file");
    let err = copy_range(
        &src,
        &mut Vec::new(),
        0,
        Some(117),
        b"\r\n",
        Alignment::Required,
        SeparatorCheck::FirstFrame,
    )
    .expect_err("copied with a separator the file does not hold");

    assert!(
        err.to_string().contains("does not occur"),
        "refused, but not as a separator that does not occur: {err}"
    );
}

#[test]
fn test_copy_range_refuses_a_separator_that_does_not_end_frame_0() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());

    // Occurs 33 times in frame 0, so counting alone accepts it. A frame ends immediately after the
    // separator it was cut with, and frame 0 ends with a newline.
    let err = assert_refused_with(
        &input,
        0,
        Some(33),
        b"\"lvl\":\"info\"",
        Alignment::Required,
        SeparatorCheck::FirstFrame,
    );

    assert!(
        err.to_string().contains("does not end with the separator"),
        "refused, but not as a separator that does not end frame 0: {err}"
    );
}

/// Frames of `[5, 5, 7, 3]`: frame 0 and frame 1 agree, so a check that stops at the first frame
/// accepts the file. The count drifts at frame 2, which only a second frame catches.
fn drifting_file(dir: &Path, label: &str, records: &[Vec<u8>]) -> PathBuf {
    compress_frames(
        dir,
        label,
        &[
            records[0..5].concat(),
            records[5..10].concat(),
            records[10..17].concat(),
            records[17..20].concat(),
        ],
    )
}

#[test]
fn test_copy_range_takes_the_count_of_frame_0_without_the_second_frame_check() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(20, true);
    let input = drifting_file(temp_dir.path(), "drift", &records);
    assert_framing(&input, &[5, 5, 7, 3]);

    // Frame 0 holds 5 and ends with the separator, so 5 is taken as the count for every frame.
    let out_path = copy_to(temp_dir.path(), "copy", &input, 5, Some(5))
        .expect("Failed to copy a range of a file whose count drifts later on");

    assert_framing(&out_path, &[5]);
    assert_decompresses_to(&out_path, &records[5..10].concat());
}

#[test]
fn test_copy_range_catches_a_drifting_count_with_the_second_frame_check() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(20, true);
    let input = drifting_file(temp_dir.path(), "drift", &records);

    let err = assert_refused_with(
        &input,
        5,
        Some(5),
        b"\n",
        Alignment::Required,
        SeparatorCheck::TwoFrames,
    );

    assert!(
        err.to_string().contains("frame 2 holds 7"),
        "the refusal does not name the frame whose count differs: {err}"
    );
}

#[test]
fn test_copy_range_from_a_file_of_two_frames() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(2 * FIXTURE_RECORDS_PER_FRAME, true);
    let input = compress_body(temp_dir.path(), "two_frames", &records.concat());
    assert_framing(&input, &[FIXTURE_RECORDS_PER_FRAME; 2]);

    // Frame 0 is not the frame the file ends with, so the separator can be checked against it.
    let out_path = copy_to(
        temp_dir.path(),
        "copy",
        &input,
        0,
        Some(FIXTURE_RECORDS_PER_FRAME as u64),
    )
    .expect("Failed to copy from a file of two frames");

    assert_framing(&out_path, &[FIXTURE_RECORDS_PER_FRAME]);
    assert_decompresses_to(&out_path, &records[..FIXTURE_RECORDS_PER_FRAME].concat());
}

#[test]
fn test_copy_range_refuses_the_second_frame_check_on_a_file_of_two_frames() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(2 * FIXTURE_RECORDS_PER_FRAME, true);
    let input = compress_body(temp_dir.path(), "two_frames", &records.concat());

    // Frame 1 is the one allowed to be short, so there is no second frame to compare against.
    assert_refused_with(
        &input,
        0,
        Some(FIXTURE_RECORDS_PER_FRAME as u64),
        b"\n",
        Alignment::Required,
        SeparatorCheck::TwoFrames,
    );
}

#[test]
fn test_copy_range_refuses_a_file_of_one_frame() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(5, true);
    let input = compress_frames(temp_dir.path(), "one_frame", &[records.concat()]);
    assert_framing(&input, &[5]);

    // The only frame is the one the file ends with, which is allowed to end anywhere.
    let err = assert_refused(&input, 0, Some(5), Alignment::Required);
    assert!(
        err.to_string().contains("one data frame"),
        "refused, but not for having a single frame: {err}"
    );
}

#[test]
fn test_copy_range_refuses_a_last_frame_larger_than_the_others() {
    let records = fixture_records_upto(20, true);
    let temp_dir = tempdir().expect("Failed to create temp dir");
    // A shape the compressor never writes: the last frame holds more than the count validation
    // establishes, not fewer.
    let input = compress_frames(
        temp_dir.path(),
        "uneven",
        &[
            records[0..5].concat(),
            records[5..10].concat(),
            records[10..20].concat(),
        ],
    );
    assert_framing(&input, &[5, 5, 10]);

    let err = assert_refused(&input, 0, None, Alignment::Required);
    assert!(
        err.to_string().contains("10"),
        "the refusal does not say how many records the last frame holds: {err}"
    );

    let out_path = copy_to_with(
        temp_dir.path(),
        "copy",
        &input,
        0,
        None,
        b"\n",
        Alignment::NotRequired,
    )
    .expect("Failed to copy a file whose last frame is larger");
    assert_framing(&out_path, &[5, 5, 10]);
    assert_decompresses_to(&out_path, &records.concat());
}

#[test]
fn test_copy_range_with_a_multi_byte_separator() {
    const SEP: &[u8] = b"-=-";
    let records: Vec<Vec<u8>> = (0..10)
        .map(|i| format!("record {i}-=-").into_bytes())
        .collect();

    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_frames(
        temp_dir.path(),
        "multibyte",
        &[
            records[0..4].concat(),
            records[4..8].concat(),
            records[8..10].concat(),
        ],
    );
    assert_framing_with(&input, SEP, &[4, 4, 2]);

    let out_path = copy_to_with(
        temp_dir.path(),
        "copy",
        &input,
        4,
        Some(4),
        SEP,
        Alignment::Required,
    )
    .expect("Failed to copy the range");

    assert_framing_with(&out_path, SEP, &[4]);
    assert_decompresses_to(&out_path, &records[4..8].concat());
}

#[test]
fn test_copy_range_refuses_the_record_count_of_the_whole_file_by_default() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());

    // The same end as the range above, named by record count rather than by leaving --cnt off.
    let err = assert_refused(&input, 0, Some(FIXTURE_RECORDS as u64), Alignment::Required);

    assert!(
        err.to_string().contains("15"),
        "the refusal does not say how many records the short frame holds: {err}"
    );
}

#[test]
fn test_copy_range_past_a_last_frame_that_carries_nothing() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records: Vec<Vec<u8>> = (0..9).map(|i| format!("rec{i}\n").into_bytes()).collect();
    // The frame zeekstd's finish() writes after an end_frame(). No record range reaches it.
    let input = compress_frames(
        temp_dir.path(),
        "empty_tail",
        &[
            records[0..3].concat(),
            records[3..6].concat(),
            records[6..9].concat(),
            Vec::new(),
        ],
    );
    assert_framing(&input, &[3, 3, 3, 0]);

    let out_path = copy_to(temp_dir.path(), "copy", &input, 3, None)
        .expect("Failed to copy to the end of a file whose last frame carries nothing");

    assert_framing(&out_path, &[3, 3]);
    assert_decompresses_to(&out_path, &records[3..9].concat());
}
