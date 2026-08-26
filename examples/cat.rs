//! Read a range of records out of a compressed file by record index.
//!
//! ```sh
//! cargo run --example cat -- events.jsonl.seek.zst 10000 3
//! ```
//!
//! Only the frames covering the requested range are decompressed. The record index is resolved to a
//! frame index arithmetically, so the cost does not grow with how far into the file the range sits.
//!
//! Note: `cnt` currently yields one more record than requested. See the known issues in `README.md`.

use std::path::PathBuf;

use seekzstdsep::cat_data;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: cat <FILE> <FROM> <CNT>";
    let path = PathBuf::from(args.next().expect(usage));
    let from: usize = args.next().expect(usage).parse()?;
    let cnt: usize = args.next().expect(usage).parse()?;

    let records = cat_data(path, from, cnt, b"\n")?;
    print!("{}", String::from_utf8_lossy(&records));

    Ok(())
}
