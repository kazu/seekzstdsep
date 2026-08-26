//! The two functions `cat_data` reads a frame through, before and after they were taken through
//! `record` and `decompressed_range`.
//!
//! Two rather than three: `cnt_of_separetor_in_frame_via_buf` is called by the second of them, and
//! `decompressed_range` by both, so neither needs a case of its own.
//!
//! The `before` cases below are the bodies as they stood at the branch point, kept here rather than
//! in the crate: if they measure the same, nothing has to be added to the crate to keep measuring
//! them.
//!
//!
//! **Read both orders before believing a difference.** criterion measures the cases in a group one
//! after the other, and the one measured second comes out slower by a percent or so whatever it is.
//! Swap the two `bench_function` calls: a difference that follows the code is real, one that stays
//! with the position is not. Instruction counts under callgrind do not have this problem.
//! ```sh
//! cargo bench --bench read
//! ```

use criterion::{Criterion, criterion_group, criterion_main};
use memchr::memmem::Finder;
use std::hint::black_box;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use seekzstdsep::seekzstdsep_lib::{
    cnt_of_separetor_in_frame, lines_between_by_separator_in_frame, seek_table_decomp_frames,
};
use seekzstdsep::{CompressOptions, compress_to_seekable_zst_with_opts};
use zeekstd::Decoder;

const RECORDS: usize = 200_000;
const FRAME_SIZE: usize = 65536;
const SEPARATOR: &[u8] = b"\n";

// ------------------------------------------------------- the bodies as they were

/// `lines_between_by_separator_in_frame` at the branch point: one `read`, and the record boundaries
/// spelled out. The discarded read amount is what it did; see `docs/bugs.md`.
#[allow(clippy::unused_io_amount)]
fn before_lines_between<'a>(
    decoder: &mut Decoder<'a, std::fs::File>,
    frame_start: u64,
    frame_len: u64,
    start_sep_cnt: u64,
    cnt_of_sep: u64,
    finder: &Finder,
    separator: &[u8],
) -> anyhow::Result<Vec<u8>> {
    decoder.seek(SeekFrom::Start(frame_start))?;
    let mut data = vec![0u8; frame_len as usize];
    decoder
        .read(&mut data[..])
        .expect("cannot read frame full data");

    let start = if start_sep_cnt == 0 {
        Some(0)
    } else {
        finder
            .find_iter(&data)
            .nth(start_sep_cnt as usize - 1)
            .map(|p| p + separator.len())
    };

    if start.is_none() {
        return Err(anyhow::anyhow!("No separator found in frame"));
    }
    if cnt_of_sep == 0 {
        return Ok(Vec::new());
    }

    let end = start.and_then(|s_pos| {
        finder
            .find_iter(&data[s_pos..])
            .nth(cnt_of_sep as usize - 1)
            .map(|p| s_pos + p)
    });
    let end_pos = match end {
        Some(pos) => pos + separator.len(),
        None => data.len(),
    };
    Ok(data[start.unwrap()..end_pos].to_vec())
}

/// `cnt_of_separetor_in_frame` at the branch point.
fn before_cnt_in_frame<'a>(
    decoder: &mut Decoder<'a, std::fs::File>,
    start: u64,
    len: u64,
    finder: &Finder,
) -> anyhow::Result<usize> {
    if len == 0 {
        return Ok(0);
    }
    decoder.seek(SeekFrom::Start(start))?;
    let mut data = vec![0u8; len as usize];
    let _n = decoder.read(&mut data[..])?;
    Ok(finder.find_iter(&data).count())
}

// ------------------------------------------------------- fixture

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
        FRAME_SIZE,
        true,
        SEPARATOR,
        None,
        Some(CompressOptions {
            out_dir: Some(dir.path().to_path_buf()),
            out_path: Some(out.clone()),
            ..Default::default()
        }),
    )
    .expect("failed to compress the fixture");
    (dir, out)
}

fn decoder(path: &PathBuf) -> Decoder<'static, std::fs::File> {
    Decoder::new(std::fs::File::open(path).expect("no fixture")).expect("no decoder")
}

fn read(c: &mut Criterion) {
    let (_dir, path) = fixture();
    let finder = Finder::new(SEPARATOR);

    // The frame in the middle of the file, and how many records it holds.
    let frames = {
        let d = decoder(&path);
        seek_table_decomp_frames(&d).expect("no frames")
    };
    let index = frames.len() / 2;
    let (frame_start, frame_len) = frames[index];
    let records = {
        let mut d = decoder(&path);
        cnt_of_separetor_in_frame(&mut d, frame_start, frame_len, &finder, SEPARATOR)
            .expect("cannot count") as u64
    };
    {
        let mut group = c.benchmark_group("lines_between_by_separator_in_frame");
        let mut d = decoder(&path);
        group.bench_function("before", |b| {
            b.iter(|| {
                before_lines_between(
                    &mut d,
                    black_box(frame_start),
                    black_box(frame_len),
                    black_box(records / 4),
                    black_box(records / 2),
                    &finder,
                    SEPARATOR,
                )
                .unwrap()
            })
        });
        let mut d = decoder(&path);
        group.bench_function("after", |b| {
            b.iter(|| {
                lines_between_by_separator_in_frame(
                    &mut d,
                    black_box(frame_start),
                    black_box(frame_len),
                    black_box(records / 4),
                    black_box(records / 2),
                    &finder,
                    SEPARATOR,
                )
                .unwrap()
            })
        });
        group.finish();
    }

    {
        let mut group = c.benchmark_group("cnt_of_separetor_in_frame");
        let mut d = decoder(&path);
        group.bench_function("before", |b| {
            b.iter(|| {
                before_cnt_in_frame(
                    &mut d,
                    black_box(frame_start),
                    black_box(frame_len),
                    &finder,
                )
                .unwrap()
            })
        });
        let mut d = decoder(&path);
        group.bench_function("after", |b| {
            b.iter(|| {
                cnt_of_separetor_in_frame(
                    &mut d,
                    black_box(frame_start),
                    black_box(frame_len),
                    &finder,
                    SEPARATOR,
                )
                .unwrap()
            })
        });
        group.finish();
    }
}

criterion_group!(benches, read);
criterion_main!(benches);
