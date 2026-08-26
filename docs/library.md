# Library

The API in the order you meet it: compress, read back, edit in place. `examples/` holds the
same calls as programs that are compiled by `cargo test`, so they are the copy that cannot rot.

## Compress

Compressing works over any `Read`/`Write` pair:

```rust
use seekzstdsep::convert_to_seekable_zst_reader;

let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
let mut compressed: Vec<u8> = Vec::new();

convert_to_seekable_zst_reader(
    input,
    &mut compressed,
    64 * 1024, // frame size target in bytes
    true,      // hold the separator count uniform across frames
    b"\n",
    None,      // limit_multiplier, defaults to 4
)
.unwrap();

assert!(!compressed.is_empty());
```

That fourth argument is the whole point: with `false`, frames are cut by size alone and `cat` can no
longer resolve a record index by arithmetic. `convert_text_to_seekable_zst_reader` is a shorthand
that passes `false`.

## Read a record range, and the frame layout

Reading a record range back needs a real file, because it seeks:

```rust,no_run
use seekzstdsep::{cat_data, inspect};
use std::path::PathBuf;

let path = PathBuf::from("events.jsonl.seek.zst");

// Records starting at index 10000.
let records: Vec<u8> = cat_data(path.clone(), 10000, 3, b"\n").unwrap();
print!("{}", String::from_utf8_lossy(&records));

// Per-frame layout.
for frame in inspect(path, b"\n").unwrap() {
    println!("{} records in {} compressed bytes", frame.cnt_of_sep, frame.comp_size);
}
```

## Truncate

`truncate` shortens a file in place, and needs it open for both reading and writing:

```rust,no_run
use seekzstdsep::truncate;
use std::fs::File;

let mut f = File::options()
    .read(true)
    .write(true)
    .open("events.jsonl.seek.zst")
    .unwrap();

truncate(&mut f, 10_000, b"\n").unwrap();
```

## Append

`append` adds records to the end of a file, and needs it open the same way:

```rust,no_run
use seekzstdsep::{OnMissingSeparator, append};
use std::fs::File;

let mut f = File::options()
    .read(true)
    .write(true)
    .open("events.jsonl.seek.zst")
    .unwrap();

append(&mut f, File::open("more.jsonl").unwrap(), b"\n", OnMissingSeparator::Refuse).unwrap();
```

The records come from any `Read`. `OnMissingSeparator::Insert` writes a separator at the join
instead of refusing a file that ends in a fragment.

## Compressing with options

`compress_to_seekable_zst_with_opts` is the higher-level entry point: it takes a `Read + Seek`
source and retries with adjusted framing when the auto-detected records-per-frame turns out not to
fit, which the streaming entry points cannot do. Output is staged in a temporary file and cloned to
`CompressOptions::out_path` with reflink, which avoids copying the data a second time; the writer
argument receives it only as a fallback, when reflink is unavailable. So set `out_path` to get
output. `compress_to_seekable_zst` takes no options and therefore has no destination.

