# Library

The API in the order you meet it: compress, read back, edit in place. `examples/`
contains runnable versions of the main workflows, compiled by `cargo test`.

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

That fourth argument is the whole point. Either way a frame ends at a record boundary; with `false`
the record count is left to fall where the byte target puts it, and `cat` can no longer resolve a
record index by arithmetic. `RecordReader` opens such a file all the same and divides by the count
frame 0 happens to hold, so it answers with the wrong records;
`RecordReader::verifying` turns that into an error for the frames a read walks to the end of. A
read placed by a frame it never reaches is still answered with the wrong records: nothing counted
that frame.

## Read records with `RecordReader`

`RecordReader` seeks within a file. Open it with a separator, then read a record range or inspect
the physical frame layout:

```rust,no_run
use seekzstdsep::{RecordReader, inspect};
use std::path::PathBuf;

let path = PathBuf::from("events.jsonl.seek.zst");

// Records starting at index 10000.
let mut reader = RecordReader::open(path.clone(), b"\n").unwrap();
let records: Vec<u8> = reader.records(10000, 3).unwrap();
print!("{}", String::from_utf8_lossy(&records));

println!("{} frames", reader.frame_count());
println!("{} records per full frame", reader.records_per_frame());
if let Some(bytes) = reader.frame_decompressed_size(0) {
    println!("frame 0 expands to {bytes} bytes");
}

// Per-frame layout.
for frame in inspect(path, b"\n").unwrap() {
    println!("{} records in {} compressed bytes", frame.cnt_of_sep, frame.comp_size);
}
```

`frame_count()` is the total number of physical frames in the original file.
`frame_decompressed_size(frame)` takes an absolute, zero-based frame number and returns `None` if
it is out of range. The size is the byte count after decompression; getting it does not decompress
the frame. `records_per_frame()` is counted from frame 0 when the reader opens.

### Iterate records

Choose an iterator by whether it should borrow bytes from the reader or own each record:

| Method | Items | Starting point |
| --- | --- | --- |
| `records_from(index)` | Borrowed record bytes | Record index from this reader's start |
| `into_records_from(index)` | Owned `Vec<u8>` | Record index from this reader's start |
| `into_records()` | Owned `Vec<u8>` | This reader's first frame |

All three iterators read lazily. Use `take(n)` to stop after `n` records. A borrowed record from
`records_from` must be dropped before requesting the next item; copy it if it must outlive the
iteration. `into_records` also supports `rev()` for reading backwards. For a reader opened with
`start_frame > 0`, record index 0 is the first record in that frame.

```rust,no_run
use seekzstdsep::RecordReader;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from("events.jsonl.seek.zst");
    let mut reader = RecordReader::open(path.clone(), b"\n")?;
    for record in reader.records_from(10_000).take(3) {
        println!("{}", String::from_utf8_lossy(&record?));
    }

    let owned: Vec<Vec<u8>> = RecordReader::open(path, b"\n")?
        .into_records_from(10_000)
        .take(3)
        .collect::<anyhow::Result<_>>()?;
    Ok(())
}
```

### Reuse one open file across readers

On Unix and Windows, `open_with_opts` accepts both a new file and an existing `RecordReader`.
Open the file once, then create readers with independent read positions that share its open file
and frame metadata. `start_frame` is an absolute, zero-based physical frame number. The new reader
starts there and `into_records()` continues through the end of the file.

The example below assumes full frames have a uniform record count, as with `true` in the
compression example above. `records_per_frame()` reports frame 0's count; with varying counts,
`take(per_frame)` does not necessarily stop at the next physical frame boundary.

```rust,no_run
use seekzstdsep::{RecordReader, RecordReaderOpenOptions, find::Boundary};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let opened = RecordReader::open_with_opts(RecordReaderOpenOptions {
        path: Some(PathBuf::from("events.jsonl.seek.zst")),
        boundary: Some(Boundary::Separator(b"\n".to_vec())),
        ..RecordReaderOpenOptions::new()
    })?;

    let start_frame = opened.frame_count() / 2;
    let per_frame = opened.records_per_frame();
    let worker = RecordReader::open_with_opts(RecordReaderOpenOptions {
        path: None,
        boundary: None,
        reuse: Some(&opened),
        start_frame,
    })?;
    assert_eq!(worker.frame_count(), opened.frame_count());
    assert_eq!(
        worker.frame_decompressed_size(start_frame),
        opened.frame_decompressed_size(start_frame),
    );
    assert_eq!(worker.frame_decompressed_size(0), opened.frame_decompressed_size(0));

    // With uniform frame counts, `take` reads this frame's records.
    // Omit it to read through EOF.
    let records: Vec<Vec<u8>> = worker.into_records()
        .take(per_frame)
        .collect::<anyhow::Result<_>>()?;

    let checked = RecordReader::open_with_opts(RecordReaderOpenOptions {
        path: None,
        boundary: None,
        reuse: Some(&opened),
        start_frame,
    })?.verifying();
    let checked_records: Vec<Vec<u8>> = checked.into_records()
        .take(per_frame)
        .collect::<anyhow::Result<_>>()?;
    assert_eq!(records, checked_records);

    // Construct workers before sending them to threads; no reader lock is needed.
    let workers = (0..opened.frame_count())
        .map(|start_frame| RecordReader::open_with_opts(RecordReaderOpenOptions {
            path: None,
            boundary: None,
            reuse: Some(&opened),
            start_frame,
        }))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let handles: Vec<_> = workers.into_iter().map(|worker| {
        std::thread::spawn(move || {
            worker.into_records().take(per_frame).collect::<anyhow::Result<Vec<_>>>()
        })
    }).collect();
    for handle in handles {
        let frame_records = handle.join().expect("worker panicked")?;
        println!("{} records", frame_records.len());
    }
    Ok(())
}
```

Create separate workers before moving them into threads. Each worker has its own decoder and read
position; reusing a reader does not reopen the path or parse the seek table again. A new file can
also set `start_frame` in the same options, or use a finder instead of a separator by setting
`boundary` to `Boundary::Finder(Box::new(find::by_fixed(3)))`. Reused readers keep the original
boundary; leave `path` and `boundary` as `None` when setting `reuse`. An out-of-range
`start_frame` returns an error.

Call `.verifying()` before iteration to check the record count and record ending at each physical
frame boundary that the read reaches. This also works on a reused reader; verification state is
private to each reader while the frame metadata stays shared.

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
        records_per_frame: None,
    },
    b"\n",
)
.unwrap();
```

The records come from any `Read`. `OnMissingSeparator::Insert` writes a separator at the join
instead of refusing a file that ends in a fragment. `level` is the Zstandard compression level of
the frames this writes, 0 the zstd default.

Set `records_per_frame: Some(n)` to append with a known frame count, including to a file with
only one or two frames. `None` retains count inference and its three-frame minimum.
The validation scope and last-frame rules are documented on
[`append_records`](https://docs.rs/seekzstdsep/latest/seekzstdsep/fn.append_records.html).

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
`find::by_separator`. `RecordReader::from_reader` takes either as a `find::Boundary`, on any
`Read + Seek` source.

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
