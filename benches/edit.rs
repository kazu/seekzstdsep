//! What the operations in `edit` cost: `copy_range` over a range against the whole file, reading
//! frames, and the two halves of `append`.
//!
//! What the `copy_range` cases are for: copying a range is a byte copy of the frames it names plus
//! a fixed cost that does not depend on the range — the seek table, and the two frames validation
//! decompresses. An implementation that decoded and re-encoded instead would still pass the tests,
//! and would show up here as `range` growing towards `whole`. `append/frames` moves the same eight
//! frames onto an existing file rather than into a new one, so it reads against `copy_range/range`;
//! `append/records` is the path that does re-encode, and is the cost `append/frames` exists to
//! avoid.
//!
//!
//! **Read both orders before believing a difference.** criterion measures the cases in a group one
//! after the other, and the one measured second comes out slower by a percent or so whatever it is.
//! Swap the two `bench_function` calls: a difference that follows the code is real, one that stays
//! with the position is not. Instruction counts under callgrind do not have this problem.
//! ```sh
//! cargo bench --bench edit
//! ```

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::fs::File;
use std::hint::black_box;
use std::path::PathBuf;

use seekzstdsep::{
    Alignment, CompressOptions, CompressionLevel, OnMissingSeparator, RangeCheck, SeparatorCheck,
    append_frames, append_records, compress_to_seekable_zst_with_opts, copy_range, count_frames,
};
use zeekstd::SeekTable;

const RECORDS: usize = 200_000;
const RECORDS_PER_FRAME: usize = 1_000;
const SEPARATOR: &[u8] = b"\n";

/// The frames a `range` case copies, out of the 200 the fixture holds.
const FRAMES_COPIED: usize = 8;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    fixture_of(RECORDS)
}

fn fixture_of(records: usize) -> (tempfile::TempDir, PathBuf) {
    const OPS: [&str; 8] = [
        "open", "read", "write", "close", "flush", "seek", "stat", "fsync",
    ];
    let mut body = Vec::new();
    for i in 0..records {
        body.extend_from_slice(
            format!(
                "{{\"ts\":\"2026-08-24T00:00:{:02}Z\",\"lvl\":\"info\",\"seq\":{i},\"op\":\"{}\",\
                 \"path\":\"/var/log/app.log\",\"took_us\":{},\"msg\":\"done\"}}\n",
                i % 60,
                OPS[i % 8],
                i % 1000
            )
            .as_bytes(),
        );
    }

    let dir = tempfile::tempdir().expect("no temp dir");
    let out = dir.path().join("bench.seek.zst");
    compress_to_seekable_zst_with_opts(
        std::io::Cursor::new(body),
        &mut std::io::sink(),
        65536,
        true,
        SEPARATOR,
        None,
        Some(CompressOptions {
            out_dir: Some(dir.path().to_path_buf()),
            out_path: Some(out.clone()),
            max_of_separator: Some(RECORDS_PER_FRAME),
            ..Default::default()
        }),
    )
    .expect("failed to compress the fixture");
    (dir, out)
}

fn copy(c: &mut Criterion) {
    let (_dir, path) = fixture();
    let input = File::open(&path).expect("no fixture");

    let run = |from: u64, cnt: Option<u64>, align| {
        let mut out = Vec::new();
        copy_range(
            &input,
            &mut out,
            from,
            cnt,
            SEPARATOR,
            align,
            SeparatorCheck::FirstFrame,
        )
        .expect("failed to copy");
        out
    };

    let from = (RECORDS / 2) as u64;
    let cnt = (FRAMES_COPIED * RECORDS_PER_FRAME) as u64;
    let mut group = c.benchmark_group("copy_range");

    // The compressed bytes each case moves, measured rather than estimated: what is copied is the
    // frames as they sit in the file, not the records they decompress to.
    group.throughput(Throughput::Bytes(
        run(from, Some(cnt), Alignment::Required).len() as u64,
    ));
    group.bench_function("range", |b| {
        b.iter(|| run(black_box(from), black_box(Some(cnt)), Alignment::Required))
    });

    group.throughput(Throughput::Bytes(
        run(0, None, Alignment::NotRequired).len() as u64,
    ));
    group.bench_function("whole", |b| {
        b.iter(|| run(black_box(0), black_box(None), Alignment::NotRequired))
    });

    group.finish();
}

/// Frames enough for the largest `count_frames` case below, with the frame size held at the one the
/// `copy_range` cases use so that only the frame count varies.
const COUNT_RECORDS: usize = 400_000;

/// Reading a range of frames and counting the records in each, which is what every operation in
/// `edit` is built out of: two frames for separator validation, one for the frame a file ends with,
/// and the whole range under `RangeCheck::EveryFrame`.
///
/// Throughput is in frames, so the number to read is the **per-frame time**. It is flat in `K` when
/// the cost of reading a frame does not depend on how many are read, and grows with `K` when
/// something per-frame is being rebuilt out of the whole file — which is what the seek table being
/// cloned once per frame does.
fn count(c: &mut Criterion) {
    let (_dir, path) = fixture_of(COUNT_RECORDS);
    let file = File::open(&path).expect("no fixture");
    let mut src = &file;
    let table = SeekTable::from_seekable(&mut src).expect("no seek table");

    let mut group = c.benchmark_group("count_frames");
    for k in [1u32, 2, 16, 256] {
        group.throughput(Throughput::Elements(u64::from(k)));
        group.bench_with_input(BenchmarkId::from_parameter(k), &k, |b, &k| {
            b.iter(|| {
                count_frames(&file, &table, SEPARATOR, black_box(0..k)).expect("failed to count")
            })
        });
    }
    group.finish();
}

/// The two halves of `append`, each called directly rather than through the `AppendInput` that
/// picks between them.
///
/// Both rewrite the end of the file, so every iteration gets a fresh copy of the fixture; making
/// that copy is setup and is not timed. The `frames` case takes the same eight frames
/// `copy_range/range` copies, so the two read against each other: the same frames moved, once into
/// a second file and once onto the end of an existing one.
fn append_to(c: &mut Criterion) {
    let (dir, path) = fixture();
    let input = File::open(&path).expect("no fixture");
    let mut src = &input;
    let table = SeekTable::from_seekable(&mut src).expect("no seek table");

    // One frame's worth of records, which is what the `records` case adds.
    let mut body = Vec::new();
    for i in 0..RECORDS_PER_FRAME {
        body.extend_from_slice(format!("{{\"seq\":{i},\"msg\":\"appended\"}}\n").as_bytes());
    }

    let target = dir.path().join("append-target.seek.zst");
    let fresh = || {
        std::fs::copy(&path, &target).expect("failed to copy the fixture");
        File::options()
            .read(true)
            .write(true)
            .open(&target)
            .expect("failed to open the target")
    };

    let from = (RECORDS / 2) as u64;
    let cnt = (FRAMES_COPIED * RECORDS_PER_FRAME) as u64;
    let k = (from / RECORDS_PER_FRAME as u64) as u32;
    let moved = table
        .frame_end_comp(k + FRAMES_COPIED as u32 - 1)
        .expect("no such frame")
        - table.frame_start_comp(k).expect("no such frame");

    let mut group = c.benchmark_group("append");

    group.throughput(Throughput::Bytes(body.len() as u64));
    group.bench_function("records", |b| {
        b.iter_batched(
            fresh,
            |mut f| {
                append_records(
                    &mut f,
                    body.as_slice(),
                    SEPARATOR,
                    OnMissingSeparator::Refuse,
                    CompressionLevel::default(),
                )
                .expect("failed to append records")
            },
            BatchSize::PerIteration,
        )
    });

    // The compressed bytes moved, which is the number `copy_range/range` reports as well.
    group.throughput(Throughput::Bytes(moved));
    group.bench_function("frames", |b| {
        b.iter_batched(
            fresh,
            |mut f| {
                append_frames(
                    &mut f,
                    &input,
                    black_box(from),
                    black_box(Some(cnt)),
                    SEPARATOR,
                    RangeCheck::FirstFrame,
                )
                .expect("failed to append frames")
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

criterion_group!(benches, copy, count, append_to);
criterion_main!(benches);
