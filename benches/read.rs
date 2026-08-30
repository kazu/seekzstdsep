//! The readers a range read and `into_records` go through, before and after.
//!
//! The `before` cases are the bodies as they stood at the branch point, kept here rather than in
//! the crate: if they measure the same, nothing has to be added to the crate to keep measuring
//! them. Comparing against an older implementation than that means building it from its own
//! checkout and running the two binaries alternately.
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
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use seekzstdsep::seekzstdsep_lib::{
    cnt_of_separetor_in_frame, records_between_by_separator_in_frame, seek_table_decomp_frames,
};
use seekzstdsep::{CompressOptions, RecordReader, compress_to_seekable_zst_with_opts};
use zeekstd::Decoder;

const RECORDS: usize = 200_000;
const FRAME_SIZE: usize = 65536;
const SEPARATOR: &[u8] = b"\n";

// ------------------------------------------------------- the bodies as they were

/// `records_between_by_separator_in_frame` at the branch point: one `read`, and the record boundaries
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

/// `into_records` at the branch point: a frame decompressed into one buffer and kept, records cut
/// out of it one at a time, the next frame decompressed over it. How many records that is, so the
/// case matches the `after` one, which counts what it hands out.
#[allow(clippy::unused_io_amount)]
fn before_into_records(path: &PathBuf, finder: &Finder) -> anyhow::Result<usize> {
    let mut d = decoder(path);
    let frames = seek_table_decomp_frames(&d).expect("no frames");
    let mut count = 0usize;
    for (start, len) in frames {
        d.seek(SeekFrom::Start(start))?;
        let mut data = vec![0u8; len as usize];
        d.read(&mut data[..])?;
        let mut offset = 0usize;
        while let Some(end) = finder.find(&data[offset..]).map(|p| p + SEPARATOR.len()) {
            black_box(data[offset..offset + end].to_vec());
            offset += end;
            count += 1;
        }
    }
    Ok(count)
}

/// `cat` at the branch point: open the file once, read the seek table, place the range over
/// frames, decode the whole of that span into one buffer, cut the records out of it and hand back
/// the `Vec`. `cat_data` was this, down to reading frame 0 and the range through the one decoder
/// it opened — opening a second one here would charge this case an open and a seek table the
/// `after` case does not pay.
#[allow(clippy::unused_io_amount)]
fn before_cat(path: &PathBuf, from: usize, cnt: usize, finder: &Finder) -> anyhow::Result<Vec<u8>> {
    let mut d = decoder(path);
    let frames = seek_table_decomp_frames(&d).expect("no frames");
    let sep_cnt = {
        let (start, len) = frames[0];
        before_cnt_in_frame(&mut d, start, len, finder)?
    };

    // Where the range lands, as the reader placed it before this branch.
    let total = sep_cnt * frames.len();
    let frame_idx = frames.len() * from / total;
    let idx_in_frame = from % sep_cnt;
    let start = frames[frame_idx].0;
    let end_idx = (frames.len() * (from + cnt + 1) / total).min(frames.len() - 1);
    let len = frames[end_idx].0 + frames[end_idx].1 - start;

    // The whole span in one buffer, one read, as decompressed_range did.
    d.seek(SeekFrom::Start(start))?;
    let mut data = vec![0u8; len as usize];
    d.read(&mut data[..])?;

    let begin = if idx_in_frame == 0 {
        0
    } else {
        finder
            .find_iter(&data)
            .nth(idx_in_frame - 1)
            .map(|p| p + SEPARATOR.len())
            .unwrap_or(data.len())
    };
    let end = finder
        .find_iter(&data[begin..])
        .nth(cnt - 1)
        .map(|p| begin + p + SEPARATOR.len())
        .unwrap_or(data.len());
    Ok(data[begin..end].to_vec())
}

/// `RecordReader` at the branch point, as far as `record` needed it: the decoder and the frame
/// list held open, and the one frame a lookup decompressed kept in a `Vec` for the next one.
struct BeforeReader {
    decoder: Decoder<'static, std::fs::File>,
    frames: Vec<(u64, u64)>,
    sep_cnt: usize,
    cache: Option<(usize, Vec<u8>)>,
}

impl BeforeReader {
    fn open(path: &PathBuf, finder: &Finder) -> Self {
        let mut decoder = decoder(path);
        let frames = seek_table_decomp_frames(&decoder).expect("no frames");
        let (start, len) = frames[0];
        let sep_cnt = before_cnt_in_frame(&mut decoder, start, len, finder).expect("cannot count");
        Self {
            decoder,
            frames,
            sep_cnt,
            cache: None,
        }
    }

    /// The body `RecordReader::record` had: the whole frame in one buffer, then the record cut out
    /// of it by counting separators from the top.
    #[allow(clippy::unused_io_amount)]
    fn record(&mut self, index: usize, finder: &Finder) -> Option<Vec<u8>> {
        let frame = index / self.sep_cnt;
        if frame >= self.frames.len() {
            return None;
        }
        let in_frame = index % self.sep_cnt;
        if self.cache.as_ref().map(|c| c.0) != Some(frame) {
            let (start, len) = self.frames[frame];
            self.decoder.seek(SeekFrom::Start(start)).expect("no seek");
            let mut data = vec![0u8; len as usize];
            self.decoder.read(&mut data[..]).expect("no read");
            self.cache = Some((frame, data));
        }
        let data = &self.cache.as_ref().expect("no frame").1;
        let start = match in_frame.checked_sub(1) {
            None => 0,
            Some(before) => finder
                .find_iter(data)
                .nth(before)
                .map(|p| p + SEPARATOR.len())?,
        };
        finder
            .find(&data[start..])
            .map(|p| data[start..start + p + SEPARATOR.len()].to_vec())
    }
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
        let mut group = c.benchmark_group("records_between_by_separator_in_frame");
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
                records_between_by_separator_in_frame(
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

    {
        // The whole file, record by record.
        let mut group = c.benchmark_group("into_records");
        group.sample_size(20);
        group.bench_function("before", |b| {
            b.iter(|| black_box(before_into_records(&path, &finder).unwrap()))
        });
        group.bench_function("after", |b| {
            b.iter(|| {
                let reader =
                    RecordReader::open(path.clone(), SEPARATOR).expect("Failed to open reader");
                black_box(reader.into_records().filter(|r| r.is_ok()).count())
            })
        });
        group.finish();
    }

    {
        // What `$h.10` and `$h | get 10 11` do: one or two indices into a frame, through a reader
        // held open between them. Each iteration moves to the next frame, so neither the frame the
        // `before` reader cached nor the window the `after` one carried is the one asked for —
        // which is what a lookup arriving on its own looks like.
        let mut group = c.benchmark_group("record");
        let sep_cnt = {
            let mut d = decoder(&path);
            let (start, len) = frames[0];
            cnt_of_separetor_in_frame(&mut d, start, len, &finder, SEPARATOR).expect("cannot count")
        };
        // The last frame holds fewer records than the rest, so it is left out of the rotation.
        let frame_count = frames.len() - 1;
        // A stride rather than the next frame along: reading whole frames leaves the decoder on
        // the boundary the following frame starts at, and stepping in order would hand that saving
        // to whichever side reads the most.
        let base = |turn: usize| (turn * 7 % frame_count) * sep_cnt + sep_cnt / 2;

        // Every pair of indices a lookup can be asked for: one on its own, the next one after it,
        // and the one before it, which the window has to decode the frame again for.
        for (name, second) in [("one", None), ("next", Some(1isize)), ("back", Some(-1))] {
            let mut before = BeforeReader::open(&path, &finder);
            let mut turn = 0usize;
            group.bench_function(format!("before/{name}"), |b| {
                b.iter(|| {
                    let first = base(turn);
                    turn += 1;
                    black_box(before.record(black_box(first), &finder));
                    if let Some(step) = second {
                        let next = first.wrapping_add_signed(step);
                        black_box(before.record(black_box(next), &finder));
                    }
                })
            });
            let mut after = RecordReader::open(path.clone(), SEPARATOR).expect("no reader");
            let mut turn = 0usize;
            group.bench_function(format!("after/{name}"), |b| {
                b.iter(|| {
                    let first = base(turn);
                    turn += 1;
                    black_box(after.record(black_box(first)).unwrap());
                    if let Some(step) = second {
                        let next = first.wrapping_add_signed(step);
                        black_box(after.record(black_box(next)).unwrap());
                    }
                })
            });
        }
        group.finish();
    }

    {
        // What `cat` does: a record range out of the middle of the file, to a sink. `before` is
        // the whole-span read that `cat_data` returned a `Vec` from; `after` writes the
        // window's own bytes and never builds that `Vec`.
        let mut group = c.benchmark_group("cat");
        let from = RECORDS / 2;
        for cnt in [3usize, 1000] {
            group.bench_function(format!("before/{cnt}"), |b| {
                b.iter(|| {
                    let out = before_cat(&path, black_box(from), black_box(cnt), &finder).unwrap();
                    std::io::sink().write_all(&out).unwrap();
                })
            });
            group.bench_function(format!("after/{cnt}"), |b| {
                b.iter(|| {
                    let mut reader =
                        RecordReader::open(path.clone(), SEPARATOR).expect("Failed to open reader");
                    reader
                        .records_to(black_box(from), black_box(cnt), &mut std::io::sink())
                        .unwrap()
                })
            });
        }
        group.finish();
    }
}

criterion_group!(benches, read);
criterion_main!(benches);
