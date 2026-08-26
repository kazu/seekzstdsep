# seekzstdsep

**Seekable Zstandard compression for separator-delimited record formats.**

*日本語版: [README.ja.md](README.ja.md)*

JSONL, CSV, TSV, logfmt — formats where records are delimited by a fixed string, usually a newline.
`seekzstdsep` compresses them so that **any record range can be read without decompressing the data
in front of it**.

It ships both ways: a `seekzstdsep` command line tool, and a Rust library crate.

The output is **not a proprietary container**. It is an ordinary [Zstandard Seekable Format][spec]
file, built by making deliberate choices about where frames are cut, so plain `zstd -d` decompresses
it back to the original bytes:

```sh
zstd -d -c events.jsonl.seek.zst   # byte-identical to the original
```

[spec]: https://github.com/rorosen/zeekstd/blob/main/seekable_format.md

## Why

Seekable zstd already lets you jump to a **byte** offset without decompressing everything before it.
But record-delimited data is addressed by **record**, not by byte. You know you want records
1,000,000..1,000,100; you have no way to know which byte that is without decompressing the whole
prefix.

`seekzstdsep` closes that gap with one rule: **every frame holds the same number of separators**.
Record index to frame index becomes a single division, and the decoder touches only the frames it
needs. No index structure beyond the seek table that the format already carries.

The same rule makes **parallel scans trivial to shard**: worker `i` takes records
`[i*N, (i+1)*N)` and seeks straight to its own frame range, with no shared index and no coordination
between workers.

A 50,000-record JSONL file (3.7 MB) compresses to 155 KB across 55 frames. Reading three records out
of the middle decompresses one frame, not 3.7 MB.

`docs/format.md` covers the on-disk layout and the invariant in full.

## Install

### Prerequisites

| | |
| --- | --- |
| Rust | 1.85 or later (this crate uses edition 2024) |
| C compiler | required — `zstd-sys` compiles a bundled libzstd 1.5.7 from source |

No system libzstd and no `pkg-config` are needed; the C library is vendored and linked statically.
The `zstd` command line tool is not required either, and is only mentioned above to show that the
output is a standard format.

### Build

```sh
cargo install --path .
```

### What it depends on

| Crate | Role |
| --- | --- |
| [`zeekstd`](https://docs.rs/zeekstd) | Zstandard Seekable Format encoder, decoder, and seek table |
| [`zstd`](https://docs.rs/zstd) | Zstandard bindings |
| [`memchr`](https://docs.rs/memchr) | SIMD separator search |
| [`clap`](https://docs.rs/clap) | Command line parsing |
| [`reflink-copy`](https://docs.rs/reflink-copy) | Moves staged output into place without copying, where the filesystem supports it |
| [`tempfile`](https://docs.rs/tempfile) | Staging for the compression retry loop |

`anyhow`, `serde`, `serde_json`, and `tracing` round out the list. See `Cargo.toml` for exact
versions.

## CLI

### Compress

```sh
seekzstdsep compress events.jsonl events.jsonl.seek.zst
```

With no `OUTPUT`, the output path is `INPUT` + `.seek.zst`. With no `INPUT`, input is read from
stdin and the result goes to stdout. Useful options:

| Option | Meaning |
| --- | --- |
| `-s, --separator <S>` | Record separator (default `"\n"`) |
| `--frame-size <N>` | Target frame size in bytes (default 65536) |
| `-c, --cnt-of-separator-per-frame <N>` | Pin records per frame instead of auto-detecting |
| `-l, --limit-multiplier <N>` | How far past `--frame-size` to search for a separator (default 4) |
| `--rm` | Delete the input file after a successful conversion |
| `--no-check` | Leave the per-frame content checksum out (it is written by default) |

`--frame-size` is a target, not a hard bound — a frame ends at the next separator past it, so byte
sizes vary while the record count per frame stays fixed. Leaving the defaults alone is fine for most
input; `docs/format.md` explains when it is not.

Each frame ends with a content checksum, so the one frame a lookup decompresses is verified as it is
read. It costs 4 bytes per frame, which `docs/performances.md` measures against a real file, and
`--no-check` drops it.

### Read a record range

```sh
seekzstdsep cat events.jsonl.seek.zst --from 10000 --cnt 3
```

`--from` is a 0-based record index. See [Known issues](#known-issues) for the current `--cnt`
semantics.

### Inspect the frame layout

```sh
seekzstdsep inspect events.jsonl.seek.zst
seekzstdsep inspect events.jsonl.seek.zst --format json
```

Prints per-frame compressed and decompressed extents plus the separator count, which is the quickest
way to confirm the uniform-count invariant actually holds for a given file. By default the separator
count is measured on the first and last few frames and assumed for the rest; pass `-n,
--no-fast-mode` to count every frame.

### Truncate

```sh
seekzstdsep truncate events.jsonl.seek.zst --records 10000
```

Shortens the file in place to its first `--records` records, cutting on a record boundary. Only the
frame the cut falls inside is re-encoded, and nothing before it is read or written. The seek table is
rebuilt in full, so that part is linear in the number of frames.

Destructive — clone the file first if the original matters, which `cp --reflink=auto` does in about a
millisecond where the filesystem supports it. The separator is validated against the file before
anything is written, which needs at least three frames, so very small files are refused.

### Append

```sh
seekzstdsep append events.jsonl.seek.zst more.jsonl
cat more.jsonl | seekzstdsep append events.jsonl.seek.zst
```

Adds the records to the end of the file in place. The frame a file ends with generally holds fewer
records than the rest, so appending after it would leave a short frame in the interior, where record
lookup divides by a count that no longer holds. That frame is decoded and cut again together with
the new records instead, so every frame but the last comes back holding the count the file was built
with. Nothing before it is read or written.

A file whose last byte is not the separator ends in a fragment rather than in a record, and joining
would merge that fragment with the first appended record. `append` refuses; pass
`--insert-separator` to write a separator at the join and make the fragment a record of its own.
Where one separator does not complete the record — which a separator that overlaps itself, such as
`\n\n`, can leave — that is refused too.

Destructive, and validated before anything is written, on the same terms as `truncate` above.

## Library

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

`compress_to_seekable_zst_with_opts` is the higher-level entry point: it takes a `Read + Seek`
source and retries with adjusted framing when the auto-detected records-per-frame turns out not to
fit, which the streaming entry points cannot do. Output is staged in a temporary file and cloned to
`CompressOptions::out_path` with reflink, which avoids copying the data a second time; the writer
argument receives it only as a fallback, when reflink is unavailable. So set `out_path` to get
output. `compress_to_seekable_zst` takes no options and therefore has no destination.

## nushell

`nu_plugin_zstdsep/` is a nushell plugin over the same files: `zstdsep open f | get 10`
decompresses one frame, not the file.

```text
> let h = zstdsep open events.jsonl.seek.zst
> $h.1999999.msg
```

380 µs on a 2,000,000-record file, against 4.9 s to read all of it.

`nu_plugin_zstdsep/nu/install.nu` links a hook into nushell's autoload directory that shadows
`open` and `save`, so a `.seek.zst` path reaches the plugin and every other path reaches the
builtin. See [nu_plugin_zstdsep/README.md](./nu_plugin_zstdsep/README.md).

## Documentation

- `docs/format.md` — on-disk layout, the uniform-separator-count invariant, and what depends on it.
  **Read this before changing anything about framing.**
- `docs/benchmark.md` — what the benchmarks measure, what they compare against, and the traps
  that produce wrong numbers.
- `docs/design/` — design notes for changes under consideration.

## Known issues

- `frame_size * limit_multiplier` should be at least 32768, the size of the internal read buffer.
  Below that, any input larger than the limit fails with "No separator was found before reaching the
  limit size", even when the input is entirely separators. Smaller inputs never reach the check. That message also reports `limit_multiplier` under the label
  "Current unprocessed data size".

## License

MIT ([LICENSE](./LICENSE)).
