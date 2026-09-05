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
use seekzstdsep::{RecordReader, inspect};
use std::path::PathBuf;

let path = PathBuf::from("events.jsonl.seek.zst");

// Records starting at index 10000.
let records: Vec<u8> = RecordReader::open(path.clone(), b"\n").unwrap().records(10000, 3).unwrap();
print!("{}", String::from_utf8_lossy(&records));

// Per-frame layout.
for frame in inspect(path, b"\n").unwrap() {
    println!("{} records in {} compressed bytes", frame.cnt_of_sep, frame.comp_size);
}
```

## Truncate

`truncate` shortens a file in place to a whole number of frames — the record count has to be a
multiple of the records per frame — and needs it open for both reading and writing:

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

`append` adds to the end of a file, and needs it open the same way. What is added is an
`AppendInput`:

```rust,no_run
use seekzstdsep::{AppendInput, OnMissingSeparator, append};
use std::fs::File;

let mut f = File::options()
    .read(true)
    .write(true)
    .open("events.jsonl.seek.zst")
    .unwrap();

append(
    &mut f,
    AppendInput::Records {
        data: File::open("more.jsonl").unwrap(),
        on_missing: OnMissingSeparator::Refuse,
        level: 0,
    },
    b"\n",
)
.unwrap();
```

The records come from any `Read`. `OnMissingSeparator::Insert` writes a separator at the join
instead of refusing a file that ends in a fragment. `level` is the Zstandard compression level of
the frames this writes, 0 the zstd default.

`AppendInput::Frames` joins another seekable file instead, copying its frames as compressed bytes:

```rust,no_run
use seekzstdsep::{AppendInput, RangeCheck, append};
use std::fs::File;

let mut f = File::options()
    .read(true)
    .write(true)
    .open("events.jsonl.seek.zst")
    .unwrap();
let more = File::open("more.seek.zst").unwrap();

// This variant carries no `Read`, so name the type parameter the other one would have fixed.
let frames: AppendInput<&[u8]> = AppendInput::Frames {
    input: &more,
    from: 0,
    cnt: None,
    check: RangeCheck::FirstFrame,
};
append(&mut f, frames, b"\n").unwrap();
```

Neither file is decompressed and nothing is re-encoded, so the cost is the size of the range rather
than of either file. That is available only where the frames already fit together: both files have
to hold the same number of records per frame, `f` has to end at a frame boundary with its last frame
full, and `from` and `from + cnt` have to fall on frame boundaries of the input.

`RangeCheck::FirstFrame` reads the input's records per frame off its frame 0 and takes the copied
frames on trust. `RangeCheck::EveryFrame` counts all of them and refuses one holding a count of its
own, at the price of decompressing the range.

Handing `AppendInput::Records` a compressed stream is refused: its bytes are frames, and the
separator bytes it holds by chance would each be counted as a record.

Both arms are public on their own. `append_records` and `append_frames` take the same arguments the
variants carry, so a caller that only ever does one of the two reaches it directly and says which at
the call site rather than in a value.

## Compressing with options

`compress_to_seekable_zst_with_opts` is the higher-level entry point: it takes a `Read + Seek`
source and retries with adjusted framing when the auto-detected records-per-frame turns out not to
fit, which the streaming entry points cannot do. Output is staged in a temporary file and cloned to
`CompressOptions::out_path` with reflink, which avoids copying the data a second time; the writer
argument receives it only as a fallback, when reflink is unavailable. So set `out_path` to get
output. `compress_to_seekable_zst` takes no options and therefore has no destination.


## Copy a record range

`copy_range` reads a file and writes a second one, so it needs the input open for reading only, and
takes any `Write` as the destination:

```rust,no_run
use seekzstdsep::{Alignment, SeparatorCheck, copy_range, inspect};
use std::fs::File;
use std::path::PathBuf;

let path = PathBuf::from("events.jsonl.seek.zst");
// Both ends of the range have to fall on a frame boundary, so they are multiples of the record
// count every frame but the last one holds.
let per_frame = inspect(path.clone(), b"\n").unwrap()[0].cnt_of_sep as u64;

let input = File::open(&path).unwrap();
let mut back = File::create("back.seek.zst").unwrap();

copy_range(
    &input,
    &mut back,
    per_frame * 75,
    None,
    b"\n",
    Alignment::NotRequired,
    SeparatorCheck::FirstFrame,
)
.unwrap();
```

The fourth argument is the record count to copy, `None` meaning to the end of the file. A range that
starts or ends anywhere else than at a frame boundary is refused rather than rounded, since the
frames are copied as compressed bytes.

The `Alignment` is why that call reads `NotRequired`. `Required` refuses a range whose last frame
holds a different number of records than the rest, and the frame a file ends with holds whatever was
left over — so a range running to the end of a file is refused unless it says `NotRequired`. What it
gives up is joining the result onto another file by copying bytes; the result reads back like any
other file. A range ending on a frame boundary, such as `Some(per_frame * 40)` here, needs nothing
given up.

The `SeparatorCheck` decides how much of the file the separator is checked against.
`SeparatorCheck::FirstFrame` decompresses frame 0, takes the record count from it, and refuses a
separator that does not end it — a frame ends immediately after the separator it was cut with.
`SeparatorCheck::TwoFrames` counts a second frame as well and refuses when the two differ, which
catches a count that drifts later in the file at the price of that frame decompressed.

## Records that do not end with a separator

Where a record ends is a function, and `seekzstdsep::find` holds the ones the crate ships:
`by_separator`, `by_le32_prefix` (FlatBuffers `FinishSizePrefixed`), `by_fixed` and `by_msgpack`.
Anything of the shape `Fn(&[u8]) -> Option<usize>` will do — the length of the record that starts at
`data[0]`, or `None` when `data` does not hold a whole one.

Every entry point above has a twin that takes one in place of the separator:
`compress_records_to_seekable_zst_with_opts`, `convert_records_to_seekable_zst_reader_with_opts`,
`RecordReader::open_with` and `from_file_with`, `count_records_in_frame`, `read_records_in_frame`,
`count_records_in_buf`, `inspect_records_with_opts`, `truncate_records`, `append_records_with`,
`append_frames_with` and `copy_range_with`. The separator forms are those, called with
`find::by_separator`.

```rust,no_run
use seekzstdsep::find;
use seekzstdsep::{CompressOptions, RecordReader, compress_records_to_seekable_zst_with_opts};
use std::path::PathBuf;

let out = PathBuf::from("events.bin.seek.zst");
compress_records_to_seekable_zst_with_opts(
    std::fs::File::open("events.bin").unwrap(),
    std::io::sink(),
    64 * 1024,
    true,
    find::by_le32_prefix,
    None,
    Some(CompressOptions {
        out_path: Some(out.clone()),
        ..Default::default()
    }),
)
.unwrap();

let mut reader = RecordReader::open_with(out, Box::new(find::by_le32_prefix)).unwrap();
let first = reader.record(0).unwrap();
```

`RecordReader` holds the finder boxed, so the type carries no parameter and a lookup costs one
indirect call per record; the separator form pays the same. `RecordReader::separator` comes back
empty when the reader was opened with a finder, and `OnMissingSeparator::Insert` is refused by
`append_records_with` — writing a separator at the join needs a separator to write.
