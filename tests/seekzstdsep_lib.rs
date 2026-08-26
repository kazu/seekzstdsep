mod common;
use common::*;

use rand::Rng;
use seekzstdsep::InspectOptions;
use seekzstdsep::cat_data;
use seekzstdsep::convert_to_seekable_zst_reader;
use seekzstdsep::seekzstdsep_lib::{inspect_with_opts, seek_table_decomp_frames};

use std::fs::File;
use std::io::Write;
use tempfile::tempdir;
use zeekstd::Decoder;

fn generate_test_data_with_separator(content: &str, separator: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let separator_str = std::str::from_utf8(separator).unwrap_or("");
    let parts: Vec<&str> = content.split(separator_str).collect();

    for (i, part) in parts.iter().enumerate() {
        result.extend_from_slice(part.as_bytes());
        if i < parts.len() - 1 {
            result.extend_from_slice(separator);
        }
    }

    // Remove trailing separator if present (this seems to be the behavior of the compression function)
    if result.len() >= separator.len() && &result[result.len() - separator.len()..] == separator {
        result.truncate(result.len() - separator.len());
    }

    // Remove leading separator if present (this seems to be the behavior of the compression function)
    if result.len() >= separator.len() && &result[..separator.len()] == separator {
        result.drain(0..separator.len());
    }

    result
}

fn generate_random_string_with_separators(length: usize, separator: &str) -> String {
    let mut rng = rand::rng();
    let mut result = String::new();
    for i in 0..length {
        if i > 0 && i % 20 == 0 {
            result.push_str(separator);
        } else {
            let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
            result.push(chars[rng.random_range(0..chars.len())] as char);
        }
    }
    result
}

macro_rules! test_convert_text_to_seekable_zst {
    (
        name: $test_name:ident,
        content: $content:expr,
        separator: $separator:expr,
        frame_size: $frame_size:expr,
        should_succeed: $should_succeed:expr
    ) => {
        #[test]
        fn $test_name() {
            let temp_dir = tempdir().expect("Failed to create temp dir");
            let mut rng = rand::rng();
            let random_suffix: u32 = rng.random();

            let input_path = temp_dir.path().join(format!("input_{}.txt", random_suffix));
            let output_path = temp_dir
                .path()
                .join(format!("output_{}.zst", random_suffix));

            let separator_bytes = $separator.as_bytes();
            let test_data = generate_test_data_with_separator($content, separator_bytes);

            File::create(&input_path)
                .expect("Failed to create input file")
                .write_all(&test_data)
                .expect("Failed to write input file");

            let input_file = File::open(&input_path).expect("Failed to open input file");
            let output_file = File::create(&output_path).expect("Failed to create output file");

            let result = convert_to_seekable_zst_reader(
                input_file,
                output_file,
                $frame_size,
                true,
                separator_bytes,
                None,
            );

            if $should_succeed {
                assert!(
                    result.is_ok(),
                    "Expected success but got error: {:?}",
                    result.err()
                );

                let decompressed_result =
                    decompress_and_compare(output_path.to_str().unwrap(), &test_data);
                assert!(decompressed_result.is_ok(), "Failed to decompress file");
                assert!(
                    decompressed_result.unwrap(),
                    "Decompressed data doesn't match original"
                );
            } else {
                assert!(result.is_err(), "Expected error but got success");
            }
        }
    };
}

// Test cases for different separator lengths (1-10 bytes)
test_convert_text_to_seekable_zst!(
    name: test_separator_length_1,
    content: "line1\nline2\nline3\nline4\nline5",
    separator: "\n",
    frame_size: 1024,
    should_succeed: true
);

test_convert_text_to_seekable_zst!(
    name: test_separator_length_2,
    content: "line1\r\nline2\r\nline3\r\nline4\r\nline5",
    separator: "\r\n",
    frame_size: 1024,
    should_succeed: true
);

test_convert_text_to_seekable_zst!(
    name: test_separator_length_3,
    content: "line1---line2---line3---line4---line5",
    separator: "---",
    frame_size: 1024,
    should_succeed: true
);

test_convert_text_to_seekable_zst!(
    name: test_separator_length_5,
    content: "line1-----line2-----line3-----line4-----line5",
    separator: "-----",
    frame_size: 1024,
    should_succeed: true
);

test_convert_text_to_seekable_zst!(
    name: test_separator_length_10,
    content: "line1----------line2----------line3----------line4----------line5",
    separator: "----------",
    frame_size: 1024,
    should_succeed: true
);

#[test]
fn test_empty_separator() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let mut rng = rand::rng();
    let random_suffix: u32 = rng.random();

    let input_path = temp_dir.path().join(format!("input_{}.txt", random_suffix));
    let output_path = temp_dir
        .path()
        .join(format!("output_{}.zst", random_suffix));

    let separator_bytes = b"";
    let test_data = b"line1line2line3line4line5";

    File::create(&input_path)
        .expect("Failed to create input file")
        .write_all(test_data)
        .expect("Failed to write input file");

    let input_file = File::open(&input_path).expect("Failed to open input file");
    let output_file = File::create(&output_path).expect("Failed to create output file");

    // This should panic with "window size must be non-zero"

    let result =
        convert_to_seekable_zst_reader(input_file, output_file, 1024, true, separator_bytes, None);
    assert!(result.is_err());
}

// Test cases for different frame sizes
test_convert_text_to_seekable_zst!(
    name: test_frame_size_1024,
    content: &generate_random_string_with_separators(2000, "|"),
    separator: "|",
    frame_size: 1024,
    should_succeed: true
);

test_convert_text_to_seekable_zst!(
    name: test_frame_size_8192,
    content: &generate_random_string_with_separators(50000, "|"),
    separator: "|",
    frame_size: 8192,
    should_succeed: true
);

test_convert_text_to_seekable_zst!(
    name: test_frame_size_8192_times_5,
    content: &generate_random_string_with_separators(100000, "|"),
    separator: "|",
    frame_size: 8192 * 5,
    should_succeed: true
);

// Test case with fewer separators than frame_size * 4
test_convert_text_to_seekable_zst!(
    name: test_few_separators,
    content: "very long content without many separators just a few|here|and there",
    separator: "|",
    frame_size: 1024,
    should_succeed: true
);

// Test case with more separators than frame_size * 4 (should fail)
test_convert_text_to_seekable_zst!(
    name: test_too_many_separators,
    content: &"a|".repeat(5000),
    separator: "|",
    frame_size: 1024,
    should_succeed: false
);

// Additional edge cases
test_convert_text_to_seekable_zst!(
    name: test_empty_content,
    content: "",
    separator: "|",
    frame_size: 1024,
    should_succeed: true
);

test_convert_text_to_seekable_zst!(
    name: test_content_without_separator,
    content: "content without any separators at all",
    separator: "|",
    frame_size: 1024,
    should_succeed: true
);

test_convert_text_to_seekable_zst!(
    name: test_content_ending_with_separator,
    content: "line1|line2|line3|",
    separator: "|",
    frame_size: 1024,
    should_succeed: true
);

test_convert_text_to_seekable_zst!(
    name: test_content_starting_with_separator,
    content: "|line1|line2|line3",
    separator: "|",
    frame_size: 1024,
    should_succeed: true
);

#[test]
fn test_compress_fixture() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let raw = std::fs::read(fixture_path()).expect("Failed to read fixture");
    assert_eq!(raw.len() as u64, FIXTURE_BYTES, "fixture changed size");
    assert_eq!(
        fixture_records().len(),
        FIXTURE_RECORDS,
        "fixture changed record count"
    );

    let out_path = compress_fixture(temp_dir.path());

    let compressed = std::fs::metadata(&out_path)
        .expect("Failed to stat output file")
        .len();
    assert!(
        compressed < FIXTURE_BYTES,
        "compressed to {compressed} bytes, no smaller than the {FIXTURE_BYTES}-byte fixture"
    );

    let matched = decompress_and_compare(out_path.to_str().unwrap(), &raw)
        .expect("Failed to decompress file");
    assert!(matched, "Decompressed data doesn't match original");
}

#[test]
fn test_inspect_fixture() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    // fast_mode extrapolates the interior frames from frame 0, which is the property under test.
    let frames = inspect_with_opts(out_path, b"\n", InspectOptions { fast_mode: false })
        .expect("Failed to inspect zst file");

    assert!(
        frames.len() > 2,
        "expected the fixture to span several frames, got {}",
        frames.len()
    );
    assert_eq!(frames[0].decomp_start, 0);
    assert_eq!(frames[0].comp_start, 0);
    assert_eq!(frames.last().unwrap().decomp_end, FIXTURE_BYTES);
    assert_eq!(
        frames.iter().map(|f| f.cnt_of_sep).sum::<usize>(),
        FIXTURE_RECORDS,
        "the frames do not account for every record"
    );

    for (i, pair) in frames.windows(2).enumerate() {
        assert_eq!(
            pair[0].comp_end,
            pair[1].comp_start,
            "frames {i} and {} leave a gap in the compressed file",
            i + 1
        );
        assert_eq!(
            pair[0].decomp_end,
            pair[1].decomp_start,
            "frames {i} and {} leave a gap in the decompressed stream",
            i + 1
        );
    }
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f.sep, b"\n", "frame {i} reports the wrong separator");
        assert_eq!(f.comp_size, f.comp_end - f.comp_start, "frame {i}");
        assert_eq!(f.decomp_size, f.decomp_end - f.decomp_start, "frame {i}");
    }

    // Every frame holds the count frame 0 established, except the last, which holds the remainder.
    let (last, full) = frames.split_last().expect("no frame carries data");
    for (i, f) in full.iter().enumerate() {
        assert_eq!(
            f.cnt_of_sep, FIXTURE_RECORDS_PER_FRAME,
            "frame {i} holds {} records, not the {FIXTURE_RECORDS_PER_FRAME} frame 0 established",
            f.cnt_of_sep
        );
    }
    assert!(
        last.cnt_of_sep > 0 && last.cnt_of_sep <= FIXTURE_RECORDS_PER_FRAME,
        "the trailing frame holds {} records",
        last.cnt_of_sep
    );
}

#[test]
fn test_cat_fixture() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();

    // Inside frame 0, across the frame 0/1 boundary, at the start of frame 1, and inside a later
    // frame.
    for (from, cnt) in [(0usize, 3usize), (5, 1), (116, 3), (117, 2), (350, 10)] {
        assert_cat_returns(&out_path, &records, from, cnt);
    }
}

#[test]
fn test_cat_fixture_at_the_last_record() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();

    // The closing separator of the last record is the last byte of the stream. Reading it back has
    // to include it, separator and all.
    assert_cat_returns(&out_path, &records, FIXTURE_RECORDS - 1, 1);
    assert_cat_returns(&out_path, &records, FIXTURE_RECORDS - 4, 4);
}

#[test]
fn test_cat_fixture_past_the_last_record() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();

    // More records asked for than the file holds. What exists is the answer, as it is for
    // `tail -n +N | head -n CNT`.
    assert_cat_returns(&out_path, &records, FIXTURE_RECORDS - 5, 100);
    assert_cat_returns(&out_path, &records, 0, FIXTURE_RECORDS + 1);
}

#[test]
#[allow(deprecated)]
fn test_deprecated_wrapper_keeps_its_result() {
    use memchr::memmem::Finder;
    use seekzstdsep::seekzstdsep_lib::lines_betwee_by_separator_in_frame;

    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());
    let records = fixture_records();
    let finder = Finder::new(b"\n");

    let mut decoder =
        Decoder::new(File::open(&out_path).expect("Failed to open output")).expect("no decoder");
    let frames = seek_table_decomp_frames(&decoder).expect("no frames");

    // end_sep_cnt is inclusive, so 0 through 5 is six records. That is what this name has always
    // returned and what forwarding to lines_between_by_separator_in_frame has to keep returning.
    let (start, len) = frames[0];
    let got = lines_betwee_by_separator_in_frame(&mut decoder, start, len, 0, 5, &finder, b"\n")
        .expect("Failed to read records");
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&records[0..6].concat()),
        "the deprecated name stopped treating end_sep_cnt as inclusive"
    );

    // Asking for more separators than the region holds drops the last byte. That is a defect, kept
    // here on purpose; lines_between_by_separator_in_frame returns the byte.
    let (idx, &(start, len)) = frames
        .iter()
        .enumerate()
        .filter(|(_, (_, len))| *len > 0)
        .next_back()
        .expect("no frame carries data");
    let first = FIXTURE_RECORDS_PER_FRAME * idx;
    let mut expected = records[first..].concat();
    expected.pop();
    let got = lines_betwee_by_separator_in_frame(&mut decoder, start, len, 0, 1000, &finder, b"\n")
        .expect("Failed to read records");
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&expected),
        "the deprecated name stopped dropping the last byte"
    );
}

#[test]
fn test_cat_fixture_from_past_the_last_record() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    // `from` itself is out of range, so there is no record to start at. An error, not a panic.
    let result = cat_data(out_path, FIXTURE_RECORDS * 10, 1, b"\n");
    assert!(
        result.is_err(),
        "expected an error, got {} bytes",
        result.unwrap().len()
    );
}

#[test]
fn test_no_empty_frame_in_fixture() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(temp_dir.path());

    let (total, empty) = empty_frames(&out_path);
    assert!(
        empty.is_empty(),
        "frames {empty:?} of {total} carry no data"
    );
}

// 5 full frames and nothing over.
const FIXTURE_RECORDS_EVEN: usize = FIXTURE_RECORDS_PER_FRAME * 5;

/// Round trips `records`, and reads the last one back by index.
fn assert_boundary_is_sound(label: &str, records: &[Vec<u8>]) {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let body: Vec<u8> = records.concat();
    let out_path = compress_body(temp_dir.path(), label, &body);

    let matched = decompress_and_compare(out_path.to_str().unwrap(), &body)
        .expect("Failed to decompress file");
    assert!(matched, "{label}: decompressed data doesn't match original");

    let last = records.len() - 1;
    let got = cat_data(out_path, last, 1, b"\n")
        .unwrap_or_else(|e| panic!("{label}: failed to read record {last}: {e}"));
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(records.last().expect("no records")),
        "{label}: reading record {last} did not return the last record"
    );
}

#[test]
fn test_frames_divide_evenly_ending_with_separator() {
    assert_boundary_is_sound(
        "even_sep",
        &fixture_records_upto(FIXTURE_RECORDS_EVEN, true),
    );
}

#[test]
fn test_frames_divide_evenly_without_trailing_separator() {
    assert_boundary_is_sound(
        "even_nosep",
        &fixture_records_upto(FIXTURE_RECORDS_EVEN, false),
    );
}

#[test]
fn test_partial_last_frame_ending_with_separator() {
    assert_boundary_is_sound("partial_sep", &fixture_records_upto(FIXTURE_RECORDS, true));
}

#[test]
fn test_partial_last_frame_without_trailing_separator() {
    assert_boundary_is_sound(
        "partial_nosep",
        &fixture_records_upto(FIXTURE_RECORDS, false),
    );
}

#[test]
fn test_compress_writes_a_checksum_in_every_frame() {
    let dir = tempdir().expect("Failed to create temp dir");
    let out_path = compress_fixture(dir.path());

    let flags = frame_checksum_flags(&out_path);
    assert!(!flags.is_empty(), "the fixture compressed to no frames");
    assert!(
        flags.iter().all(|&on| on),
        "frames carry no checksum: {flags:?}"
    );
}

/// Records of random bytes, which zstd stores in raw blocks because nothing compresses them. A
/// flipped byte there changes the data zstd hands back rather than the encoding it reads, so the
/// checksum is the only thing that can object.
fn incompressible_records(count: usize, len: usize) -> Vec<u8> {
    let mut rng = rand::rng();
    let mut out = Vec::with_capacity(count * (len + 1));
    for _ in 0..count {
        for _ in 0..len {
            let b: u8 = rng.random();
            out.push(if b == b'\n' { b'\n' + 1 } else { b });
        }
        out.push(b'\n');
    }
    out
}

/// Flips the last byte of frame 0's compressed content, stepping over the checksum when there is
/// one so that both settings have the same byte of data corrupted.
fn corrupt_frame_zero(path: &std::path::Path, has_checksum: bool) {
    let mut file = File::open(path).expect("Failed to open compressed file");
    let table =
        zeekstd::SeekTable::from_seekable(&mut file).expect("Failed to read the seek table");
    let end = table.frame_end_comp(0).expect("no frame 0") as usize;
    let at = if has_checksum { end - 5 } else { end - 1 };

    let mut bytes = std::fs::read(path).expect("Failed to read compressed file");
    bytes[at] ^= 0xff;
    std::fs::write(path, &bytes).expect("Failed to write compressed file");
}

const CORRUPT_RECORDS: usize = 200;
const CORRUPT_RECORD_LEN: usize = 200;

#[test]
fn test_a_flipped_byte_in_a_frame_is_caught() {
    let dir = tempdir().expect("Failed to create temp dir");
    let body = incompressible_records(CORRUPT_RECORDS, CORRUPT_RECORD_LEN);
    let out_path = compress_body(dir.path(), "random", &body);
    corrupt_frame_zero(&out_path, true);

    let err = cat_data(out_path, 0, CORRUPT_RECORDS, b"\n")
        .expect_err("a corrupted frame decompressed without complaint");
    assert!(
        err.to_string().to_lowercase().contains("checksum"),
        "the failure was not the checksum: {err}"
    );
}

#[test]
fn test_a_flipped_byte_goes_unnoticed_without_a_checksum() {
    let dir = tempdir().expect("Failed to create temp dir");
    let body = incompressible_records(CORRUPT_RECORDS, CORRUPT_RECORD_LEN);
    let out_path = compress_body_with_checksum(dir.path(), "random", &body, false);
    corrupt_frame_zero(&out_path, false);

    let got = cat_data(out_path, 0, CORRUPT_RECORDS, b"\n").expect("Failed to cat data");
    assert_ne!(
        got, body,
        "the corruption did not reach the data, so this proves nothing about the checksum"
    );
}

#[test]
fn test_compress_options_default_writes_a_checksum() {
    let opts = seekzstdsep::CompressOptions::default();

    assert!(opts.checksum, "the default left the checksum out");
    assert_eq!(opts.max_of_separator, None);
    assert_eq!(opts.out_dir, None);
    assert_eq!(opts.out_path, None);
}

/// A frame holding more than 2 MiB of records is not cut at 2 MiB.
///
/// zeekstd ends a frame there by default, wherever that lands, so the frame gets split at a byte
/// count rather than at a record count and the interior of the file ends up with frames of
/// differing record counts — which `cat_data` resolves by dividing. See `docs/bugs.md`.
#[test]
fn test_compress_does_not_cut_a_frame_at_two_mib() {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    // Seven records of 400 KB, asked for seven to a frame: 2.8 MB in one frame.
    let records: Vec<Vec<u8>> = (0..14)
        .map(|i| {
            let mut r = format!("{i}").into_bytes();
            r.resize(399_999, b'x');
            r.push(b'\n');
            r
        })
        .collect();

    let raw = temp_dir.path().join("big.jsonl");
    File::create(&raw)
        .expect("Failed to create input")
        .write_all(&records.concat())
        .expect("Failed to write input");

    let out_path = temp_dir.path().join("big.seek.zst");
    let mut input = File::open(&raw).expect("Failed to open input");
    seekzstdsep::compress_to_seekable_zst_with_opts(
        &mut input,
        &mut std::io::sink(),
        65536,
        true,
        b"\n",
        Some(64),
        Some(seekzstdsep::CompressOptions {
            max_of_separator: Some(7),
            out_dir: Some(temp_dir.path().to_path_buf()),
            out_path: Some(out_path.clone()),
            ..Default::default()
        }),
    )
    .expect("Failed to compress");

    assert_framing(&out_path, &[7, 7]);
    assert_decompresses_to(&out_path, &records.concat());
}
