//! `copy_range` over a few frames of a file against every frame of it.
//!
//! What the two cases are for: copying a range is a byte copy of the frames it names plus a fixed
//! cost that does not depend on the range — the seek table, and the two frames validation
//! decompresses. An implementation that decoded and re-encoded instead would still pass the tests,
//! and would show up here as `range` growing towards `whole`.
//!
//!
//! **Read both orders before believing a difference.** criterion measures the cases in a group one
//! after the other, and the one measured second comes out slower by a percent or so whatever it is.
//! Swap the two `bench_function` calls: a difference that follows the code is real, one that stays
//! with the position is not. Instruction counts under callgrind do not have this problem.
//! ```sh
//! cargo bench --bench edit
//! ```

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::fs::File;
use std::hint::black_box;
use std::path::PathBuf;

use seekzstdsep::{
    Alignment, CompressOptions, SeparatorCheck, compress_to_seekable_zst_with_opts, copy_range,
};

const RECORDS: usize = 200_000;
const RECORDS_PER_FRAME: usize = 1_000;
const SEPARATOR: &[u8] = b"\n";

/// The frames a `range` case copies, out of the 200 the fixture holds.
const FRAMES_COPIED: usize = 8;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    const OPS: [&str; 8] = [
        "open", "read", "write", "close", "flush", "seek", "stat", "fsync",
    ];
    let mut body = Vec::new();
    for i in 0..RECORDS {
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

criterion_group!(benches, copy);
criterion_main!(benches);
