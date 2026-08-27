//! The `truncate`, `append` and `compress` subcommands, exercised through the binary.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod common;
use common::{
    FIXTURE_RECORDS, compress_body, compress_frames, fixture_records_upto, frame_checksum_flags,
};

use tempfile::tempdir;

const BIN: &str = env!("CARGO_BIN_EXE_seekzstdsep");

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/records.jsonl")
}

/// Copies the fixture into `dir` and compresses it through the binary, returning the output path.
fn compress_fixture(dir: &Path) -> PathBuf {
    let raw = dir.join("records.jsonl");
    std::fs::copy(fixture_path(), &raw).expect("Failed to copy fixture");
    let out_path = dir.join("records.seek.zst");

    let out = Command::new(BIN)
        .args([
            "compress",
            raw.to_str().unwrap(),
            out_path.to_str().unwrap(),
        ])
        .args(["--frame-size", "16384"])
        .output()
        .expect("Failed to run compress");
    assert!(
        out.status.success(),
        "compress failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    out_path
}

fn run_truncate(path: &Path, records: &str) -> Output {
    Command::new(BIN)
        .args(["truncate", path.to_str().unwrap(), "--records", records])
        .output()
        .expect("Failed to run truncate")
}

fn cat_output(path: &Path, from: usize, cnt: usize) -> Output {
    Command::new(BIN)
        .args(["cat", path.to_str().unwrap()])
        .args(["--from", &from.to_string(), "--cnt", &cnt.to_string()])
        .output()
        .expect("Failed to run cat")
}

/// The records the file holds, read back through `cat`.
fn cat(path: &Path, from: usize, cnt: usize) -> String {
    let out = cat_output(path, from, cnt);
    assert!(
        out.status.success(),
        "cat failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn test_truncate_subcommand_shortens_the_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let expected = cat(&out_path, 233, 1);

    let out = run_truncate(&out_path, "234");
    assert!(
        out.status.success(),
        "truncate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Record 233 is the last one kept and 234 is past the end. The pair pins the count the
    // subcommand forwarded, which neither alone does.
    assert_eq!(cat(&out_path, 233, 1), expected, "the last record changed");
    assert!(
        !cat_output(&out_path, 234, 1).status.success(),
        "record 234 is still readable, so more than 234 records were kept"
    );
}

#[test]
fn test_truncate_subcommand_exits_non_zero_on_a_refusal() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let before = std::fs::read(&out_path).expect("Failed to read compressed file");

    let out = run_truncate(&out_path, "0");

    assert!(!out.status.success(), "truncate to 0 records exited zero");
    assert_eq!(
        std::fs::read(&out_path).expect("Failed to read compressed file"),
        before,
        "a refused truncate rewrote the file"
    );
}

fn run_append(path: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(["append", path.to_str().unwrap()])
        .args(args)
        .output()
        .expect("Failed to run append")
}

#[test]
fn test_append_subcommand_from_a_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let body = cat(&out_path, 0, 3);
    let added = temp_dir.path().join("added.jsonl");
    std::fs::write(&added, &body).expect("Failed to write the records to append");

    let out = run_append(&out_path, &[added.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "append failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The fixture holds 600 records, so the three appended ones are 600 through 602. Asking for a
    // fourth returns the same three, which is what pins how many were appended.
    assert_eq!(
        cat(&out_path, 600, 3),
        body,
        "the appended records did not come back"
    );
    assert_eq!(
        cat(&out_path, 600, 4),
        body,
        "a fourth record came back, so more than the three were appended"
    );
}

#[test]
fn test_append_subcommand_from_stdin() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let body = cat(&out_path, 0, 3);

    let mut child = Command::new(BIN)
        .args(["append", out_path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn append");
    child
        .stdin
        .as_mut()
        .expect("no stdin")
        .write_all(body.as_bytes())
        .expect("Failed to write to stdin");
    let out = child.wait_with_output().expect("Failed to run append");
    assert!(
        out.status.success(),
        "append failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        cat(&out_path, 600, 3),
        body,
        "the appended records did not come back"
    );
}

#[test]
fn test_append_subcommand_refuses_a_file_that_ends_in_a_fragment() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    // 599 records and a fragment after them, since the last record lost its separator.
    let records = fixture_records_upto(FIXTURE_RECORDS, false);
    let out_path = compress_body(temp_dir.path(), "fragment", &records.concat());
    let added = temp_dir.path().join("added.jsonl");
    std::fs::write(&added, b"appended\n").expect("Failed to write the records to append");
    let before = std::fs::read(&out_path).expect("Failed to read compressed file");

    let out = run_append(&out_path, &[added.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "append onto a file ending in a fragment exited zero"
    );
    assert_eq!(
        std::fs::read(&out_path).expect("Failed to read compressed file"),
        before,
        "a refused append rewrote the file"
    );

    let out = run_append(&out_path, &[added.to_str().unwrap(), "--insert-separator"]);
    assert!(
        out.status.success(),
        "append with --insert-separator failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The fragment became record 599, so the appended record is 600.
    assert_eq!(cat(&out_path, 600, 1), "appended\n");
}

// The `compress` subcommand across its input, output, framing and --rm choices.

use seekzstdsep::seekzstdsep_lib::InspectResult;

/// 20 fixture records, and a frame size small enough to cut them into several frames.
const SMALL_RECORDS: usize = 20;
const SMALL_FRAME_SIZE: &str = "1024";

fn small_input(dir: &Path, name: &str) -> (PathBuf, Vec<u8>) {
    let raw = std::fs::read(fixture_path()).expect("Failed to read fixture");
    let body: Vec<u8> = raw
        .split_inclusive(|b| *b == b'\n')
        .take(SMALL_RECORDS)
        .flatten()
        .copied()
        .collect();
    let path = dir.join(name);
    std::fs::write(&path, &body).expect("Failed to write input file");
    (path, body)
}

fn inspect_frames(path: &Path) -> Vec<InspectResult> {
    let out = Command::new(BIN)
        .args(["inspect", path.to_str().unwrap()])
        .args(["--format", "json", "--no-fast-mode"])
        .output()
        .expect("Failed to run inspect");
    assert!(
        out.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "inspect did not print JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// A compressed file both readers can use: plain zstd returns the input, and the seek table
/// accounts for every record in frames that all carry data.
fn assert_usable(path: &Path, expected: &[u8], records: usize) {
    let plain = zstd::decode_all(File::open(path).expect("Failed to open output"))
        .expect("plain zstd failed to decompress the output");
    assert_eq!(plain, expected, "the output does not hold the input bytes");

    let frames = inspect_frames(path);
    assert_eq!(
        frames.iter().map(|f| f.cnt_of_sep).sum::<usize>(),
        records,
        "the seek table accounts for {} records, not {records}: {:?}",
        frames.iter().map(|f| f.cnt_of_sep).sum::<usize>(),
        frames.iter().map(|f| f.cnt_of_sep).collect::<Vec<_>>()
    );
    assert!(
        frames.iter().all(|f| f.decomp_size > 0),
        "a frame carries no data: {:?}",
        frames.iter().map(|f| f.decomp_size).collect::<Vec<_>>()
    );
}

fn compress_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.arg("compress").args(args);
    cmd
}

#[test]
fn test_compress_file_to_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (input, body) = small_input(temp_dir.path(), "in.jsonl");
    let out_path = temp_dir.path().join("out.seek.zst");

    let out = compress_cmd(&[
        input.to_str().unwrap(),
        out_path.to_str().unwrap(),
        "--frame-size",
        SMALL_FRAME_SIZE,
    ])
    .output()
    .expect("Failed to run compress");
    assert!(out.status.success(), "compress failed");

    assert_usable(&out_path, &body, SMALL_RECORDS);
}

#[test]
fn test_compress_file_to_the_default_output_path() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (input, body) = small_input(temp_dir.path(), "in.jsonl");

    let out = compress_cmd(&[input.to_str().unwrap(), "--frame-size", SMALL_FRAME_SIZE])
        .output()
        .expect("Failed to run compress");
    assert!(out.status.success(), "compress failed");

    // With OUTPUT omitted but INPUT given, the destination is INPUT + ".seek.zst".
    let out_path = temp_dir.path().join("in.jsonl.seek.zst");
    assert!(out_path.exists(), "compress wrote no default output file");
    assert_usable(&out_path, &body, SMALL_RECORDS);
}

#[test]
fn test_compress_with_records_per_frame_pinned() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (input, body) = small_input(temp_dir.path(), "in.jsonl");
    let out_path = temp_dir.path().join("out.seek.zst");

    let out = compress_cmd(&[
        input.to_str().unwrap(),
        out_path.to_str().unwrap(),
        "--cnt-of-separator-per-frame",
        "5",
    ])
    .output()
    .expect("Failed to run compress");
    assert!(out.status.success(), "compress failed");

    assert_usable(&out_path, &body, SMALL_RECORDS);
    let counts: Vec<usize> = inspect_frames(&out_path)
        .iter()
        .map(|f| f.cnt_of_sep)
        .collect();
    assert_eq!(
        counts,
        vec![5; 4],
        "the pinned records per frame was not used"
    );
}

#[test]
fn test_compress_removes_the_input_with_rm() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (input, body) = small_input(temp_dir.path(), "in.jsonl");
    let out_path = temp_dir.path().join("out.seek.zst");

    let out = compress_cmd(&[
        input.to_str().unwrap(),
        out_path.to_str().unwrap(),
        "--frame-size",
        SMALL_FRAME_SIZE,
        "--rm",
    ])
    .output()
    .expect("Failed to run compress");
    assert!(out.status.success(), "compress failed");

    assert!(!input.exists(), "--rm left the input file behind");
    assert_usable(&out_path, &body, SMALL_RECORDS);
}

#[test]
fn test_compress_keeps_the_input_without_rm() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (input, body) = small_input(temp_dir.path(), "in.jsonl");
    let out_path = temp_dir.path().join("out.seek.zst");

    let out = compress_cmd(&[
        input.to_str().unwrap(),
        out_path.to_str().unwrap(),
        "--frame-size",
        SMALL_FRAME_SIZE,
    ])
    .output()
    .expect("Failed to run compress");
    assert!(out.status.success(), "compress failed");

    assert_eq!(
        std::fs::read(&input).expect("Failed to read input file"),
        body,
        "compress changed the input file"
    );
    assert_usable(&out_path, &body, SMALL_RECORDS);
}

// `compress` driven in this process, which the subcommand tests above cannot do: they spawn the
// binary, and `main` is a separate crate.

use clap::Parser;
use seekzstdsep::cli::{ConvertArgs, run_compress};

#[test]
fn test_compress_stdin_to_stdout() {
    // Both positionals omitted: stdin in, stdout out.
    let args = ConvertArgs::parse_from(["compress", "--frame-size", "1024"]);
    //let args = ConvertArgs::parse_from(["compress"]);

    let body: &[u8] = b"a\nb\nc\nd\ne\nf\ng\nh\n";
    let mut out = Vec::new();

    run_compress(&args, body, &mut out).expect("compress failed");

    let plain = zstd::decode_all(out.as_slice()).expect("plain zstd failed to decompress");
    assert_eq!(plain, body, "stdout does not hold the input bytes");
}

#[test]
fn test_compress_stdin_to_stdout_through_the_binary() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (_, body) = small_input(temp_dir.path(), "in.jsonl");

    // The same case as above, but through the binary, which also has a logger writing somewhere.
    let mut child = compress_cmd(&["--frame-size", SMALL_FRAME_SIZE])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Failed to run compress");
    child
        .stdin
        .take()
        .expect("no stdin")
        .write_all(&body)
        .expect("Failed to write stdin");
    let out = child.wait_with_output().expect("Failed to wait");
    assert!(out.status.success(), "compress failed");

    let out_path = temp_dir.path().join("stdout.seek.zst");
    std::fs::write(&out_path, &out.stdout).expect("Failed to write output file");
    assert_usable(&out_path, &body, SMALL_RECORDS);
}

#[test]
fn test_compress_stdin_to_stdout_with_the_fixture() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let body = std::fs::read(fixture_path()).expect("Failed to read fixture");
    let records = body.iter().filter(|b| **b == b'\n').count();

    // The whole fixture, so the stream spans several frames rather than one.
    let args = ConvertArgs::parse_from(["compress", "--frame-size", "16384"]);
    let mut out = Vec::new();
    run_compress(&args, body.as_slice(), &mut out).expect("compress failed");

    let out_path = temp_dir.path().join("stdout.seek.zst");
    std::fs::write(&out_path, &out).expect("Failed to write output file");
    assert!(
        inspect_frames(&out_path).len() > 2,
        "the fixture did not span several frames"
    );
    assert_usable(&out_path, &body, records);
}

#[test]
fn test_compress_writes_checksums_by_default() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (input, body) = small_input(temp_dir.path(), "in.jsonl");
    let out_path = temp_dir.path().join("out.seek.zst");

    let out = compress_cmd(&[
        input.to_str().unwrap(),
        out_path.to_str().unwrap(),
        "--frame-size",
        SMALL_FRAME_SIZE,
    ])
    .output()
    .expect("Failed to run compress");
    assert!(out.status.success(), "compress failed");

    assert_usable(&out_path, &body, SMALL_RECORDS);
    let flags = frame_checksum_flags(&out_path);
    assert!(
        flags.iter().all(|&on| on),
        "frames carry no checksum: {flags:?}"
    );
}

#[test]
fn test_compress_no_check_leaves_the_checksum_out() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let (input, body) = small_input(temp_dir.path(), "in.jsonl");
    let out_path = temp_dir.path().join("out.seek.zst");

    let out = compress_cmd(&[
        input.to_str().unwrap(),
        out_path.to_str().unwrap(),
        "--frame-size",
        SMALL_FRAME_SIZE,
        "--no-check",
    ])
    .output()
    .expect("Failed to run compress");
    assert!(
        out.status.success(),
        "compress failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_usable(&out_path, &body, SMALL_RECORDS);
    let flags = frame_checksum_flags(&out_path);
    assert!(
        !flags.is_empty() && flags.iter().all(|&on| !on),
        "--no-check still wrote checksums: {flags:?}"
    );
}

fn run_copy_range(input: &Path, output: &str, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(["copy-range", input.to_str().unwrap(), output])
        .args(args)
        .output()
        .expect("Failed to run copy-range")
}

#[test]
fn test_copy_range_subcommand_to_a_file() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let expected = cat(&input, 117, 234);
    let out_path = temp_dir.path().join("range.seek.zst");

    let out = run_copy_range(
        &input,
        out_path.to_str().unwrap(),
        &["--from", "117", "--cnt", "234"],
    );

    assert!(
        out.status.success(),
        "copy-range failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(cat(&out_path, 0, 234), expected);
}

#[test]
fn test_copy_range_subcommand_to_stdout() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let expected = cat(&input, 117, 117);

    let out = run_copy_range(&input, "-", &["--from", "117", "--cnt", "117"]);

    assert!(
        out.status.success(),
        "copy-range failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // What lands on stdout is a seekable file, so it reads back through cat once it is on disk.
    let written = temp_dir.path().join("stdout.seek.zst");
    std::fs::write(&written, &out.stdout).expect("Failed to write what copy-range produced");
    assert_eq!(cat(&written, 0, 117), expected);
}

#[test]
fn test_copy_range_subcommand_to_the_end_needs_no_align() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let expected = cat(&input, 234, FIXTURE_RECORDS - 234);
    let out_path = temp_dir.path().join("tail.seek.zst");

    let refused = run_copy_range(&input, out_path.to_str().unwrap(), &["--from", "234"]);
    assert!(
        !refused.status.success(),
        "copy-range copied a short final frame without --no-align"
    );

    let out = run_copy_range(
        &input,
        out_path.to_str().unwrap(),
        &["--from", "234", "--no-align"],
    );

    assert!(
        out.status.success(),
        "copy-range failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(cat(&out_path, 0, FIXTURE_RECORDS - 234), expected);
}

#[test]
fn test_copy_range_subcommand_exits_non_zero_on_a_refusal() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let before = std::fs::read(&input).expect("Failed to read compressed file");
    let out_path = temp_dir.path().join("range.seek.zst");

    let out = run_copy_range(
        &input,
        out_path.to_str().unwrap(),
        &["--from", "1", "--cnt", "117"],
    );

    assert!(
        !out.status.success(),
        "a range that starts inside a frame exited zero"
    );
    assert_eq!(
        std::fs::read(&input).expect("Failed to read compressed file"),
        before,
        "a refused copy-range rewrote its input"
    );
}

#[test]
fn test_copy_range_subcommand_leaves_the_output_alone_on_a_refusal() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let input = compress_fixture(temp_dir.path());
    let out_path = temp_dir.path().join("range.seek.zst");
    std::fs::write(&out_path, b"not the output of a copy").expect("Failed to write");

    // Refused because the range does not start at the first record of a frame.
    let out = run_copy_range(
        &input,
        out_path.to_str().unwrap(),
        &["--from", "1", "--cnt", "117"],
    );

    assert!(
        !out.status.success(),
        "a range starting inside a frame exited zero"
    );
    assert_eq!(
        std::fs::read(&out_path).expect("Failed to read the output file"),
        b"not the output of a copy",
        "a refused copy-range emptied the file it was going to write"
    );
}

#[test]
fn test_copy_range_subcommand_check_uniform_catches_a_drifting_count() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let records = fixture_records_upto(20, true);
    // Frames of [5, 5, 7, 3]: frame 0 and frame 1 agree, so only a second frame catches the drift.
    let input = compress_frames(
        temp_dir.path(),
        "drift",
        &[
            records[0..5].concat(),
            records[5..10].concat(),
            records[10..17].concat(),
            records[17..20].concat(),
        ],
    );
    let out_path = temp_dir.path().join("copy.seek.zst");

    let accepted = run_copy_range(
        &input,
        out_path.to_str().unwrap(),
        &["--from", "5", "--cnt", "5"],
    );
    assert!(
        accepted.status.success(),
        "copy-range failed without --check-uniform: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );

    let refused = run_copy_range(
        &input,
        out_path.to_str().unwrap(),
        &["--from", "5", "--cnt", "5", "--check-uniform"],
    );
    assert!(
        !refused.status.success(),
        "--check-uniform accepted a file whose frames hold different counts"
    );
}
