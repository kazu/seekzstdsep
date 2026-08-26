//! The compressor before and after the buffering and the separator scan were taken out of it.
//!
//! Both are in this binary, so the two are measured in one process off one build. Nothing here
//! reads `EXTRACTED`; each case names the function it calls.
//!
//!
//! **Read both orders before believing a difference.** criterion measures the cases in a group one
//! after the other, and the one measured second comes out slower by a percent or so whatever it is.
//! Swap the two `bench_function` calls: a difference that follows the code is real, one that stays
//! with the position is not. Instruction counts under callgrind do not have this problem.
//! ```sh
//! cargo bench --bench compress
//! ```

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use seekzstdsep::CompressOptions;
#[allow(deprecated)]
use seekzstdsep::seekzstdsep_lib::{
    new_convert_to_seekable_zst_reader_with_opts, old_convert_to_seekable_zst_reader_with_opts,
};

/// Records of the shape the fixtures use, in a size that puts several frames in the input without
/// making a round take long enough to drown the difference in noise.
fn body(records: usize) -> Vec<u8> {
    const OPS: [&str; 8] = [
        "open", "read", "write", "close", "flush", "seek", "stat", "fsync",
    ];
    let mut out = Vec::new();
    for i in 0..records {
        out.extend_from_slice(
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
    out
}

const FRAME_SIZE: usize = 65536;

fn options() -> Option<CompressOptions> {
    Some(CompressOptions::default())
}

fn compress(c: &mut Criterion) {
    let input = body(100_000);
    let mut group = c.benchmark_group("compress");
    group.throughput(Throughput::Bytes(input.len() as u64));

    group.bench_function("old", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            #[allow(deprecated)]
            old_convert_to_seekable_zst_reader_with_opts(
                black_box(&input[..]),
                &mut out,
                FRAME_SIZE,
                true,
                b"\n",
                None,
                options(),
            )
            .unwrap();
            out
        })
    });

    group.bench_function("new", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            new_convert_to_seekable_zst_reader_with_opts(
                black_box(&input[..]),
                &mut out,
                FRAME_SIZE,
                true,
                b"\n",
                None,
                options(),
            )
            .unwrap();
            out
        })
    });

    group.finish();
}

criterion_group!(benches, compress);
criterion_main!(benches);
