//! Fixture generation and caching.
//!
//! Fixtures live outside the repository, under a cache directory chosen per storage target, and
//! are reused once generated. Nothing here is timed except by the compression benchmark, which
//! builds its own output rather than reusing the cache.
//!
//! `docs/benchmark.md` fixes the fixture only by its sizes (1,000,000 records, 74.2 MB
//! uncompressed, 3.0 MB compressed), never by its content, so the generator below is a
//! reconstruction. It is chosen so that the uncompressed size lands on 74.19 MB. See
//! `docs/bench/README.md` for where that leaves the frame count.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// 16 words, total length 67, so a record averages 70 + 67/16 = 74.1875 bytes.
const WORDS: [&str; 16] = [
    "okay", "get", "put", "warn", "start", "stop", "sync", "idle", "read", "write", "open",
    "close", "flush", "retry", "done", "init",
];

pub fn record(i: u64) -> String {
    format!(
        "{{\"id\":\"{:010}\",\"ts\":\"2026-08-23T00:00:00Z\",\"lvl\":\"info\",\"msg\":\"{}\"}}\n",
        i,
        WORDS[(i % 16) as usize]
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub records: u64,
    pub frame_size: usize,
    pub raw_bytes: u64,
    pub zstd_bytes: u64,
    pub zstd_level: i32,
    pub seek_bytes: u64,
    pub seek_frames: u64,
    pub seek_records_per_frame: u64,
}

#[derive(Clone, Debug)]
pub struct Fixture {
    pub meta: Meta,
    /// Uncompressed JSONL, read by the `uncompressed + shell` baseline.
    pub raw: PathBuf,
    /// Ordinary zstd stream, read by the `zstd + shell` baseline.
    pub zstd: PathBuf,
    /// seekzstdsep: frames cut on record boundaries, uniform record count per frame.
    pub seek: PathBuf,
}

impl Fixture {
    /// Every file a case might touch, for cache eviction.
    pub fn all_files(&self) -> Vec<PathBuf> {
        vec![self.raw.clone(), self.zstd.clone(), self.seek.clone()]
    }
}

fn tag(records: u64) -> String {
    if records % 1_000_000 == 0 {
        format!("{}m", records / 1_000_000)
    } else if records % 1_000 == 0 {
        format!("{}k", records / 1_000)
    } else {
        records.to_string()
    }
}

pub fn ensure(dir: &Path, records: u64, frame_size: usize, zstd_level: i32) -> Result<Fixture> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let t = tag(records);
    let raw = dir.join(format!("f{t}.jsonl"));
    let zstd_path = dir.join(format!("f{t}.jsonl.zst"));
    let seek = dir.join(format!("f{t}.seek.zst"));
    let meta_path = dir.join(format!("f{t}.meta.json"));

    let complete = [&raw, &zstd_path, &seek, &meta_path]
        .iter()
        .all(|p| p.exists());
    if complete {
        let meta: Meta = serde_json::from_str(&std::fs::read_to_string(&meta_path)?)?;
        if meta.records == records && meta.frame_size == frame_size {
            return Ok(Fixture {
                meta,
                raw,
                zstd: zstd_path,
                seek,
            });
        }
    }

    eprintln!("# generating fixture: {records} records in {}", dir.display());
    write_raw(&raw, records)?;
    write_zstd(&raw, &zstd_path, zstd_level)?;
    write_seek(&raw, &seek, frame_size)?;

    let (seek_frames, seek_rpf) = seek_stats(&seek)?;
    let meta = Meta {
        records,
        frame_size,
        raw_bytes: std::fs::metadata(&raw)?.len(),
        zstd_bytes: std::fs::metadata(&zstd_path)?.len(),
        zstd_level,
        seek_bytes: std::fs::metadata(&seek)?.len(),
        seek_frames,
        seek_records_per_frame: seek_rpf,
    };
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
    Ok(Fixture {
        meta,
        raw,
        zstd: zstd_path,
        seek,
    })
}

fn write_raw(path: &Path, records: u64) -> Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
    for i in 0..records {
        w.write_all(record(i).as_bytes())?;
    }
    w.flush()?;
    Ok(())
}

fn write_zstd(src: &Path, dst: &Path, level: i32) -> Result<()> {
    let mut r = File::open(src)?;
    let out = File::create(dst)?;
    let mut enc = zstd::stream::Encoder::new(BufWriter::with_capacity(1 << 20, out), level)?;
    std::io::copy(&mut r, &mut enc)?;
    enc.finish()?.flush()?;
    Ok(())
}

/// Uses the library's own compressor rather than the CLI, so the double `convert` call in
/// `src/main.rs` does not enter the fixture.
fn write_seek(src: &Path, dst: &Path, frame_size: usize) -> Result<()> {
    let mut input = File::open(src)?;
    let mut sink = std::io::sink();
    let opts = seekzstdsep::CompressOptions {
        out_dir: dst.parent().map(|p| p.to_path_buf()),
        out_path: Some(dst.to_path_buf()),
        ..Default::default()
    };
    seekzstdsep::compress_to_seekable_zst_with_opts(
        &mut input,
        &mut sink,
        frame_size,
        true,
        b"\n",
        None,
        Some(opts),
    )?;
    Ok(())
}

/// `(frame count, records in frame 0)`, read straight from the seek table.
fn seek_stats(path: &Path) -> Result<(u64, u64)> {
    let mut decoder = zeekstd::Decoder::new(File::open(path)?)?;
    let frames = decoder.seek_table().num_frames() as u64;
    let first_len = decoder.seek_table().frame_end_decomp(0)?;
    let mut buf = vec![0u8; first_len as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = decoder.decompress(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let rpf = memchr::memchr_iter(b'\n', &buf[..filled]).count() as u64;
    Ok((frames, rpf))
}
