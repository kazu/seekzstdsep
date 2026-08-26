//! Fixture and assertion helpers shared by the integration tests.
//!
//! Each test binary compiles this module on its own, so whatever only one of them uses reads as
//! dead code here.
#![allow(dead_code)]

use seekzstdsep::CompressOptions;
use seekzstdsep::InspectOptions;
use seekzstdsep::cat_data;
use seekzstdsep::compress_to_seekable_zst_with_opts;
use seekzstdsep::edit::frame_has_checksum;
use seekzstdsep::seekzstdsep_lib::{inspect_with_opts, seek_table_decomp_frames};

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zeekstd::{Decoder, SeekTable};

/// Whether each frame carries a content checksum.
pub fn frame_checksum_flags(path: &Path) -> Vec<bool> {
    let mut file = File::open(path).expect("Failed to open compressed file");
    let table = SeekTable::from_seekable(&mut file).expect("Failed to read the seek table");
    (0..table.num_frames())
        .map(|i| {
            frame_has_checksum(&file, &table, i)
                .unwrap_or_else(|e| panic!("frame {i} of {}: {e}", path.display()))
        })
        .collect()
}

/// `(frame count, indices of frames that carry no data)`. Read from the seek table, so it needs no
/// separator and decompresses nothing.
pub fn empty_frames(compressed_path: &Path) -> (usize, Vec<usize>) {
    let file = File::open(compressed_path).expect("Failed to open compressed file");
    let decoder = Decoder::new(file).expect("Failed to create decoder");
    let frames = seek_table_decomp_frames(&decoder).expect("no frames");
    let empty = frames
        .iter()
        .enumerate()
        .filter(|(_, (_, len))| *len == 0)
        .map(|(i, _)| i)
        .collect();
    (frames.len(), empty)
}

/// Also fails the caller if any frame carries no data, which no compressed output should have.
pub fn decompress_and_compare(compressed_path: &str, original_data: &[u8]) -> anyhow::Result<bool> {
    assert_no_empty_frame(Path::new(compressed_path), original_data.is_empty());

    let file = File::open(compressed_path)?;
    let mut decoder = Decoder::new(file)?;
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed == original_data)
}

/// Empty input has nothing but an empty frame. Anything else must not carry one.
pub fn assert_no_empty_frame(compressed_path: &Path, input_was_empty: bool) {
    if input_was_empty {
        return;
    }
    let (total, empty) = empty_frames(compressed_path);
    assert!(
        empty.is_empty(),
        "frames {empty:?} of {total} carry no data in {}",
        compressed_path.display()
    );
}

// The three tests below cover compress_to_seekable_zst_with_opts, inspect and the cat path. They
// were written when this crate lived inside the polars-logfmt workspace and opened absolute paths
// from that machine; they run against a fixture in the repository instead.
//
// The fixture stands in for the JSONL that crate produced: 600 JSON log records, one per line, 114
// to 176 bytes each. Record sizes vary, so a uniform separator count per frame is something the
// compressor establishes rather than something the data hands it.
pub const FIXTURE_BYTES: u64 = 85_133;

pub const FIXTURE_RECORDS: usize = 600;

// convert_to_seekable_zst_reader_with_opts checks its limit after each read of the 32768-byte
// internal buffer, so frame_size * 4 has to reach 32768. Below that the compressor keeps halving
// the count until it settles at one record per frame, which still round-trips but leaves the
// output larger than the input. 16384 is the smallest frame size that avoids it here.
pub const FIXTURE_FRAME_SIZE: usize = 16384;

// Frame 0 ends at the first separator at or after FIXTURE_FRAME_SIZE bytes, and every later frame
// takes its record count from that.
pub const FIXTURE_RECORDS_PER_FRAME: usize = 117;

pub fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/records.jsonl")
}

pub fn fixture_records() -> Vec<Vec<u8>> {
    let raw = std::fs::read(fixture_path()).expect("Failed to read fixture");
    raw.split_inclusive(|b| *b == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// Compresses the fixture into `dir` and returns the path it was written to.
pub fn compress_fixture(dir: &Path) -> PathBuf {
    compress_fixture_with_checksum(dir, true)
}

/// [`compress_fixture`] with the per-frame checksum chosen, for the operations that have to match
/// whichever setting the file they are handed was written with.
pub fn compress_fixture_with_checksum(dir: &Path, checksum: bool) -> PathBuf {
    let out_path = dir.join("records.seek.zst");
    let mut input = File::open(fixture_path()).expect("Failed to open fixture");
    let mut sink = std::io::sink();

    compress_to_seekable_zst_with_opts(
        &mut input,
        &mut sink,
        FIXTURE_FRAME_SIZE,
        true,
        b"\n",
        None,
        Some(CompressOptions {
            out_dir: Some(dir.to_path_buf()),
            out_path: Some(out_path.clone()),
            checksum,
            ..Default::default()
        }),
    )
    .expect("Failed to compress fixture");

    out_path
}

pub fn assert_cat_returns(out_path: &Path, records: &[Vec<u8>], from: usize, cnt: usize) {
    assert_no_empty_frame(out_path, records.is_empty());
    let got = cat_data(out_path.to_path_buf(), from, cnt, b"\n").expect("Failed to cat data");
    let end = (from + cnt).min(records.len());
    let expected: Vec<u8> = records[from..end].concat();
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&expected),
        "cat_data(from = {from}, cnt = {cnt}) returned the wrong records"
    );
}

/// The first `n` fixture records, optionally with the closing separator of the last one removed.
pub fn fixture_records_upto(n: usize, trailing_separator: bool) -> Vec<Vec<u8>> {
    let mut records = fixture_records();
    records.truncate(n);
    if !trailing_separator {
        let last = records.last_mut().expect("no records");
        assert_eq!(
            last.pop(),
            Some(b'\n'),
            "fixture record did not end with one"
        );
    }
    records
}

/// Compresses `body` into `dir` and returns the path it was written to.
pub fn compress_body(dir: &Path, label: &str, body: &[u8]) -> PathBuf {
    compress_body_with_checksum(dir, label, body, true)
}

/// [`compress_body`] with the per-frame checksum chosen.
pub fn compress_body_with_checksum(
    dir: &Path,
    label: &str,
    body: &[u8],
    checksum: bool,
) -> PathBuf {
    let raw = dir.join(format!("{label}.jsonl"));
    File::create(&raw)
        .expect("Failed to create input file")
        .write_all(body)
        .expect("Failed to write input file");

    let out_path = dir.join(format!("{label}.seek.zst"));
    let mut input = File::open(&raw).expect("Failed to open input file");
    let mut sink = std::io::sink();
    compress_to_seekable_zst_with_opts(
        &mut input,
        &mut sink,
        FIXTURE_FRAME_SIZE,
        true,
        b"\n",
        None,
        Some(CompressOptions {
            out_dir: Some(dir.to_path_buf()),
            out_path: Some(out_path.clone()),
            checksum,
            ..Default::default()
        }),
    )
    .expect("Failed to compress");

    out_path
}

/// The invariant from `docs/format.md`, counted rather than extrapolated: `expected[i]` records in
/// frame `i`, and no frame beyond.
pub fn assert_framing(path: &Path, expected: &[usize]) {
    assert_framing_with(path, b"\n", expected);
}

pub fn assert_framing_with(path: &Path, separator: &[u8], expected: &[usize]) {
    let counts: Vec<usize> = frames_of(path, separator)
        .iter()
        .map(|f| f.cnt_of_sep)
        .collect();
    assert_eq!(counts, expected, "records per frame in {}", path.display());
}

/// Every frame counted, which is what `--no-fast-mode` is for.
pub fn frames_of(
    path: &Path,
    separator: &[u8],
) -> Vec<seekzstdsep::seekzstdsep_lib::InspectResult> {
    inspect_with_opts(
        path.to_path_buf(),
        separator,
        InspectOptions { fast_mode: false },
    )
    .expect("Failed to inspect zst file")
}

/// Reads `path` back both through the seek table and by plain zstd. The second ignores the table,
/// so it is the check that catches stale bytes between the last frame and a rewritten table.
pub fn assert_decompresses_to(path: &Path, expected: &[u8]) {
    assert!(
        decompress_and_compare(path.to_str().unwrap(), expected)
            .expect("Failed to decompress file"),
        "{} does not decompress to the records it should hold",
        path.display()
    );
    let plain = zstd::decode_all(File::open(path).expect("Failed to open output"))
        .expect("plain zstd failed to decompress the file");
    assert_eq!(
        plain, expected,
        "plain zstd read back different bytes than the seek table does"
    );
}

/// Compresses one frame per group. The crate's own compressor cannot produce this: it closes every
/// frame at the same record count, so its last frame always holds fewer than the rest.
pub fn compress_frames(dir: &Path, label: &str, groups: &[Vec<u8>]) -> PathBuf {
    use zeekstd::Encoder;

    let path = dir.join(format!("{label}.seek.zst"));
    let mut encoder = Encoder::new(File::create(&path).expect("Failed to create output file"))
        .expect("no encoder");
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            encoder.end_frame().expect("Failed to end frame");
        }
        encoder.write_all(group).expect("Failed to compress");
    }
    encoder.finish().expect("Failed to finish");
    path
}
