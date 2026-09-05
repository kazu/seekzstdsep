//! Records whose boundary is not a separator.
//!
//! Every format is put through the same round trip, so what is claimed for one is claimed for all
//! of them: compress, then read back by index, by range and by iteration, and count frame 0.
//! `flatbuffers` carries the frame-boundary operations as well, since those are the paths that
//! read a record count off a frame and then trust it.

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};

use seekzstdsep::find::{self, BoxFinder};
use seekzstdsep::seekzstdsep_lib::{count_records_in_buf, inspect_records_with_opts};
use seekzstdsep::{
    Alignment, CompressOptions, InspectOptions, OnMissingSeparator, RecordReader, SeparatorCheck,
    append_records_with, compress_records_to_seekable_zst_with_opts, copy_range_with,
    truncate_records,
};

// ------------------------------------------------------- the records

/// A FlatBuffers `FinishSizePrefixed` record of exactly `len` bytes: the `u32` length of what
/// follows, then that many.
fn le32_record(seq: usize, len: usize) -> Vec<u8> {
    let body = len - 4;
    let mut out = (body as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&payload(seq, body));
    out
}

/// A record of exactly `len` bytes and nothing marking its end, which is what `fixed` reads.
fn fixed_record(seq: usize, len: usize) -> Vec<u8> {
    payload(seq, len)
}

/// One MessagePack string of exactly `len` bytes, header included.
fn msgpack_record(seq: usize, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    if len <= 2 + u8::MAX as usize {
        out.push(0xd9);
        out.push((len - 2) as u8);
        out.extend_from_slice(&payload(seq, len - 2));
    } else {
        out.push(0xdb);
        out.extend_from_slice(&((len - 5) as u32).to_be_bytes());
        out.extend_from_slice(&payload(seq, len - 5));
    }
    out
}

/// `len` bytes that say which record they are.
fn payload(seq: usize, len: usize) -> Vec<u8> {
    let mut body = format!("record {seq} ").into_bytes();
    body.resize(len, b'.');
    body
}

/// A record format, as the tests below reach it.
struct Format {
    name: &'static str,
    /// The finder for records of `len` bytes. Only `fixed` reads the length.
    finder: fn(usize) -> BoxFinder,
    record: fn(usize, usize) -> Vec<u8>,
}

fn formats() -> Vec<Format> {
    vec![
        Format {
            name: "flatbuffers",
            finder: |_| Box::new(find::by_le32_prefix),
            record: le32_record,
        },
        Format {
            name: "fixed",
            finder: |len| Box::new(find::by_fixed(len)),
            record: fixed_record,
        },
        Format {
            name: "msgpack",
            finder: |_| Box::new(find::by_msgpack),
            record: msgpack_record,
        },
    ]
}

// ------------------------------------------------------- fixtures

/// `count` records of `len` bytes, and the bytes they make.
fn body(format: &Format, count: usize, len: usize) -> (Vec<Vec<u8>>, Vec<u8>) {
    let records: Vec<Vec<u8>> = (0..count).map(|seq| (format.record)(seq, len)).collect();
    let bytes = records.concat();
    (records, bytes)
}

/// Compresses `body` into `dir` and returns the path it was written to.
fn compress(dir: &Path, label: &str, body: &[u8], frame_size: usize, find: BoxFinder) -> PathBuf {
    let out = dir.join(format!("{label}.seek.zst"));
    compress_records_to_seekable_zst_with_opts(
        std::io::Cursor::new(body.to_vec()),
        &mut std::io::sink(),
        frame_size,
        true,
        &*find,
        None,
        Some(CompressOptions {
            out_dir: Some(dir.to_path_buf()),
            out_path: Some(out.clone()),
            ..Default::default()
        }),
    )
    .expect("failed to compress");
    out
}

/// The records each frame holds, counted rather than extrapolated.
fn frame_counts(path: &Path, find: BoxFinder) -> Vec<usize> {
    inspect_records_with_opts(
        path.to_path_buf(),
        &*find,
        InspectOptions { fast_mode: false },
    )
    .expect("failed to inspect")
    .iter()
    .map(|f| f.cnt_of_sep)
    .collect()
}

const RECORD_LEN: usize = 100;
const FRAME_SIZE: usize = 16384;
/// `FRAME_SIZE` rounded up to a whole record: a frame ends at the first record end at or after it.
const PER_FRAME: usize = FRAME_SIZE.div_ceil(RECORD_LEN);

// ------------------------------------------------------- the round trip

#[test]
fn test_every_format_reads_back_what_it_wrote() {
    for format in formats() {
        let dir = tempfile::tempdir().expect("no temp dir");
        let (records, bytes) = body(&format, 1000, RECORD_LEN);
        let path = compress(
            dir.path(),
            format.name,
            &bytes,
            FRAME_SIZE,
            (format.finder)(RECORD_LEN),
        );

        // By index, including the first and last of a frame and one that crosses into the next.
        let mut reader = RecordReader::open_with(path.clone(), (format.finder)(RECORD_LEN))
            .expect("failed to open");
        for i in [0, 1, PER_FRAME - 1, PER_FRAME, PER_FRAME + 1, 999] {
            assert_eq!(
                reader.record(i).expect("failed to read").as_ref(),
                Some(&records[i]),
                "{}: record {i}",
                format.name
            );
        }
        assert_eq!(
            reader.record(1000).expect("failed to read"),
            None,
            "{}: past the last record",
            format.name
        );

        // By range, across a frame boundary.
        let from = PER_FRAME - 2;
        let cnt = 5;
        assert_eq!(
            reader.records(from, cnt).expect("failed to read a range"),
            records[from..from + cnt].concat(),
            "{}: {cnt} records from {from}",
            format.name
        );

        // By iteration.
        let walked: Vec<Vec<u8>> = RecordReader::open_with(path, (format.finder)(RECORD_LEN))
            .expect("failed to open")
            .into_records()
            .collect::<anyhow::Result<_>>()
            .expect("failed to walk");
        assert_eq!(walked, records, "{}: every record in order", format.name);
    }
}

#[test]
fn test_every_format_counts_the_records_a_frame_holds() {
    for format in formats() {
        let dir = tempfile::tempdir().expect("no temp dir");
        let (_, bytes) = body(&format, 1000, RECORD_LEN);
        let path = compress(
            dir.path(),
            format.name,
            &bytes,
            FRAME_SIZE,
            (format.finder)(RECORD_LEN),
        );

        let counts = frame_counts(&path, (format.finder)(RECORD_LEN));
        let full = counts.len() - 1;
        assert_eq!(
            counts[..full],
            vec![PER_FRAME; full],
            "{}: every frame but the last holds the same count",
            format.name
        );
        assert_eq!(
            counts.iter().sum::<usize>(),
            1000,
            "{}: every record is in a frame",
            format.name
        );

        let reader =
            RecordReader::open_with(path, (format.finder)(RECORD_LEN)).expect("failed to open");
        assert_eq!(
            reader.records_per_frame(),
            PER_FRAME,
            "{}: records per frame",
            format.name
        );
        assert!(
            reader.separator().is_empty(),
            "{}: a finder is not a separator",
            format.name
        );
    }
}

#[test]
fn test_every_format_reads_a_record_longer_than_the_window() {
    // The window starts at 32768 bytes and doubles until a record fits, so this needs two of them.
    const LONG: usize = 100_000;
    for format in formats() {
        let dir = tempfile::tempdir().expect("no temp dir");
        let (records, bytes) = body(&format, 6, LONG);
        let path = compress(
            dir.path(),
            format.name,
            &bytes,
            65536,
            (format.finder)(LONG),
        );

        let mut reader =
            RecordReader::open_with(path.clone(), (format.finder)(LONG)).expect("failed to open");
        assert_eq!(
            reader.record(3).expect("failed to read").as_ref(),
            Some(&records[3]),
            "{}: a record longer than the window, by index",
            format.name
        );

        let walked: Vec<Vec<u8>> = RecordReader::open_with(path, (format.finder)(LONG))
            .expect("failed to open")
            .into_records()
            .collect::<anyhow::Result<_>>()
            .expect("failed to walk");
        assert_eq!(
            walked, records,
            "{}: every long record in order",
            format.name
        );
    }
}

// ------------------------------------------------------- the frame-boundary operations

/// A flatbuffers file of `count` records, in `dir`.
fn flatbuffers_file(dir: &Path, label: &str, count: usize) -> (Vec<Vec<u8>>, PathBuf) {
    let format = Format {
        name: "flatbuffers",
        finder: |_| Box::new(find::by_le32_prefix),
        record: le32_record,
    };
    let (records, bytes) = body(&format, count, RECORD_LEN);
    let path = compress(
        dir,
        label,
        &bytes,
        FRAME_SIZE,
        Box::new(find::by_le32_prefix),
    );
    (records, path)
}

#[test]
fn test_truncate_cuts_a_flatbuffers_file_at_a_frame_boundary() {
    let dir = tempfile::tempdir().expect("no temp dir");
    let (records, path) = flatbuffers_file(dir.path(), "truncate", 1000);

    let keep = PER_FRAME * 3;
    let mut f = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .expect("failed to open");
    truncate_records(&mut f, keep as u64, find::by_le32_prefix).expect("failed to truncate");
    drop(f);

    let walked: Vec<Vec<u8>> = RecordReader::open_with(path, Box::new(find::by_le32_prefix))
        .expect("failed to open")
        .into_records()
        .collect::<anyhow::Result<_>>()
        .expect("failed to walk");
    assert_eq!(walked, records[..keep], "the records that were kept");
}

#[test]
fn test_append_adds_flatbuffers_records_to_a_flatbuffers_file() {
    let dir = tempfile::tempdir().expect("no temp dir");
    let (records, path) = flatbuffers_file(dir.path(), "append", 1000);
    let added: Vec<Vec<u8>> = (1000..1060)
        .map(|seq| le32_record(seq, RECORD_LEN))
        .collect();

    let mut f = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .expect("failed to open");
    append_records_with(
        &mut f,
        std::io::Cursor::new(added.concat()),
        find::by_le32_prefix,
        OnMissingSeparator::Refuse,
        0,
    )
    .expect("failed to append");
    drop(f);

    let walked: Vec<Vec<u8>> =
        RecordReader::open_with(path.clone(), Box::new(find::by_le32_prefix))
            .expect("failed to open")
            .into_records()
            .collect::<anyhow::Result<_>>()
            .expect("failed to walk");
    assert_eq!(
        walked,
        [records, added].concat(),
        "the file after the append"
    );

    let counts = frame_counts(&path, Box::new(find::by_le32_prefix));
    let full = counts.len() - 1;
    assert_eq!(
        counts[..full],
        vec![PER_FRAME; full],
        "the appended frames hold the count the file was built with"
    );
}

#[test]
fn test_append_refuses_to_insert_a_separator_it_has_no_way_to_write() {
    let dir = tempfile::tempdir().expect("no temp dir");
    let (_, path) = flatbuffers_file(dir.path(), "insert", 1000);
    let mut f = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .expect("failed to open");
    let refused = append_records_with(
        &mut f,
        std::io::Cursor::new(le32_record(1000, RECORD_LEN)),
        find::by_le32_prefix,
        OnMissingSeparator::Insert,
        0,
    )
    .expect_err("Insert was accepted");
    assert!(
        refused.to_string().contains("nothing to insert"),
        "refused for the wrong reason: {refused}"
    );
}

#[test]
fn test_copy_range_takes_flatbuffers_frames_as_they_are() {
    let dir = tempfile::tempdir().expect("no temp dir");
    let (records, path) = flatbuffers_file(dir.path(), "copy-range", 1000);

    let from = PER_FRAME;
    let cnt = PER_FRAME * 2;
    let out = dir.path().join("copied.seek.zst");
    copy_range_with(
        &File::open(&path).expect("failed to open"),
        File::create(&out).expect("failed to create"),
        from as u64,
        Some(cnt as u64),
        find::by_le32_prefix,
        Alignment::Required,
        SeparatorCheck::FirstFrame,
    )
    .expect("failed to copy the range");

    let walked: Vec<Vec<u8>> = RecordReader::open_with(out, Box::new(find::by_le32_prefix))
        .expect("failed to open")
        .into_records()
        .collect::<anyhow::Result<_>>()
        .expect("failed to walk");
    assert_eq!(walked, records[from..from + cnt], "the copied records");
}

// ------------------------------------------------------- what a broken or wrong length does

#[test]
fn test_a_length_that_runs_past_its_frame_leaves_the_records_before_it() {
    let dir = tempfile::tempdir().expect("no temp dir");
    let whole: Vec<Vec<u8>> = (0..8).map(|seq| le32_record(seq, RECORD_LEN)).collect();

    // A header claiming more than the frame holds, which is a record cut off by whatever wrote
    // the file rather than by this crate: the frame ends in the middle of it.
    let mut torn = 10_000u32.to_le_bytes().to_vec();
    torn.extend_from_slice(&payload(8, 10));

    let mut last = whole[6..8].concat();
    last.extend_from_slice(&torn);
    let path = common::compress_frames(
        dir.path(),
        "torn",
        &[whole[0..3].concat(), whole[3..6].concat(), last],
    );

    let walked: Vec<Vec<u8>> = RecordReader::open_with(path, Box::new(find::by_le32_prefix))
        .expect("failed to open")
        .into_records()
        .collect::<anyhow::Result<_>>()
        .expect("failed to walk");
    assert_eq!(
        walked, whole,
        "the whole records of every frame, and the fragment dropped"
    );
}

#[test]
fn test_reading_a_file_with_the_wrong_format_disagrees_with_the_right_one() {
    let (_, bytes) = body(
        &Format {
            name: "flatbuffers",
            finder: |_| Box::new(find::by_le32_prefix),
            record: le32_record,
        },
        20,
        RECORD_LEN,
    );

    let right = count_records_in_buf(&bytes, find::by_le32_prefix).expect("failed to count");
    let wrong = count_records_in_buf(&bytes, find::by_fixed(RECORD_LEN)).expect("failed to count");
    assert_eq!(right, 20, "the format the bytes were written in");
    // Same count here, since the records happen to be the same length. What they hand back is not
    // the same: `fixed` starts each record where the previous one ended by arithmetic, so the
    // length header of the first is inside the record it reads.
    assert_eq!(wrong, 20, "the wrong format read them as records too");

    let mut torn = bytes.clone();
    torn.extend_from_slice(&le32_record(20, RECORD_LEN + 8));
    assert_eq!(
        count_records_in_buf(&torn, find::by_le32_prefix).expect("failed to count"),
        21,
        "a longer record is still one record"
    );
    assert_eq!(
        count_records_in_buf(&torn, find::by_fixed(RECORD_LEN)).expect("failed to count"),
        21,
        "the wrong format loses the tail of it: 2108 bytes read 100 at a time"
    );
}

// ------------------------------------------------------- the command line's names for them

#[test]
fn test_from_spec_names_the_formats() {
    use seekzstdsep::find::Boundary;

    assert!(matches!(
        find::from_spec("sep", None).expect("sep was refused"),
        Boundary::Separator(sep) if sep == b"\n"
    ));
    assert!(matches!(
        find::from_spec("sep", Some("-=-")).expect("sep was refused"),
        Boundary::Separator(sep) if sep == b"-=-"
    ));

    let fixed = match find::from_spec("fixed", Some("64")).expect("fixed was refused") {
        Boundary::Finder(find) => find,
        Boundary::Separator(_) => panic!("fixed came back as a separator"),
    };
    assert_eq!(fixed(&[0u8; 64]), Some(64));

    for (name, param, why) in [
        ("fixed", None, "needs --finder-arg"),
        ("fixed", Some("0"), "above 0"),
        ("fixed", Some("wide"), "not a record length"),
        ("flatbuffers", Some("4"), "takes no --finder-arg"),
        ("msgpack", Some("x"), "takes no --finder-arg"),
        ("jsonl", None, "unknown --finder"),
    ] {
        let refused = find::from_spec(name, param).expect_err("{name} was accepted");
        let message = format!("{refused:#}");
        assert!(
            message.contains(why),
            "--finder {name} --finder-arg {param:?} refused for the wrong reason: {message}"
        );
    }
}

#[test]
fn test_msgpack_walks_the_values_it_is_given() {
    // Every shape the walker has a branch for, one value at a time: nil, a container it has to
    // count children of, a fixed-width number, and a string whose length is in a header.
    for (name, value) in [
        ("nil", vec![0xc0]),
        ("fixarray of two fixints", vec![0x92, 0x01, 0x02]),
        ("fixmap of one pair", vec![0x81, 0xa1, b'k', 0x2a]),
        ("uint64", vec![0xcf, 0, 0, 0, 0, 0, 0, 0, 9]),
        ("float64", vec![0xcb, 0, 0, 0, 0, 0, 0, 0, 0]),
        ("fixext4", vec![0xd6, 1, 2, 3, 4, 5]),
        ("bin8", vec![0xc4, 2, 0xaa, 0xbb]),
        ("str8", vec![0xd9, 3, b'a', b'b', b'c']),
        ("array16 of three", vec![0xdc, 0, 3, 0x01, 0x02, 0x03]),
    ] {
        assert_eq!(
            find::by_msgpack(&value),
            Some(value.len()),
            "{name}: the whole value"
        );
        // Trailing bytes belong to the next record, not to this one.
        let mut with_more = value.clone();
        with_more.push(0xc0);
        assert_eq!(
            find::by_msgpack(&with_more),
            Some(value.len()),
            "{name}: one value, not two"
        );
        // A value that is only partly here has not ended.
        assert_eq!(
            find::by_msgpack(&value[..value.len() - 1]),
            None,
            "{name}: cut short"
        );
    }

    // 0xc1 is not a MessagePack value, so nothing ever ends.
    assert_eq!(find::by_msgpack(&[0xc1, 0xc0]), None, "the unused byte");
    assert_eq!(find::by_msgpack(&[]), None, "nothing at all");
}
