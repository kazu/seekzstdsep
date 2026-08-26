//! Compress a text file into the separator-aware seekable zstd format.
//!
//! ```sh
//! cargo run --example compress -- events.jsonl events.jsonl.seek.zst
//! ```
//!
//! Uses [`compress_to_seekable_zst_with_opts`], which takes a `Read + Seek` source so it can retry
//! with adjusted framing if the auto-detected records-per-frame turns out not to fit. See
//! `docs/format.md` for what that retry is doing and why.

use std::fs::File;
use std::path::PathBuf;

use seekzstdsep::{CompressOptions, compress_to_seekable_zst_with_opts};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: compress <INPUT> <OUTPUT>";
    let input = PathBuf::from(args.next().expect(usage));
    let output = PathBuf::from(args.next().expect(usage));

    let mut reader = File::open(&input)?;
    let mut writer = File::create(&output)?;

    let opts = CompressOptions {
        // The default derives records-per-frame from the first frame and writes a per-frame
        // checksum.
        out_dir: output.parent().map(|p| p.to_path_buf()),
        out_path: Some(output.clone()),
        ..Default::default()
    };

    compress_to_seekable_zst_with_opts(
        &mut reader,
        &mut writer,
        // Target frame size in bytes. A frame ends at the first separator at or after this point,
        // so frames overshoot and their byte sizes drift. The record count is what stays fixed.
        64 * 1024,
        // Keep the separator count uniform across frames. This is the invariant that makes record
        // lookup a division instead of a scan. Turning it off gives up O(1) `cat`.
        true,
        b"\n",
        // limit_multiplier: how far past the target to search for a separator before giving up.
        None,
        Some(opts),
    )?;

    let original = std::fs::metadata(&input)?.len();
    let compressed = std::fs::metadata(&output)?.len();
    println!(
        "{} -> {} ({} -> {} bytes, {:.1}%)",
        input.display(),
        output.display(),
        original,
        compressed,
        100.0 * compressed as f64 / original as f64,
    );

    Ok(())
}
