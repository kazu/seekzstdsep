# Record boundaries: the record finder

**Status:** designed, not implemented
**Date:** 2026-09-05

Where a record ends stops being "after the separator" and becomes a function the caller supplies.
A separator is one such function; FlatBuffers size-prefixed records, fixed-length records and
MessagePack values are others. Nothing in the crate outside the finders knows a format.

## The interface

```rust
impl Fn(&[u8]) -> Option<usize>
```

The length of the record that starts at `data[0]`, or `None` when `data` does not hold a whole one.

- `data` always starts on a record boundary.
- `Some(0)` is never returned.
- A length longer than `data.len()` is never returned.
- `None` means "not yet". The caller reads more and asks again. Where nothing more can be read,
  what is left is a fragment, and a fragment is not a record.
- A finder is a pure function of `data`. Nothing is carried between calls.

## The finders

`src/find.rs`, public as `seekzstdsep::find`. A finder that needs configuring is a function
returning the finder; one that needs none is the finder.

| Name | Boundary |
|---|---|
| `find::by_separator(&Finder) -> impl Fn(&[u8]) -> Option<usize> + '_` | the needle, which the record ends with |
| `find::by_le32_prefix(&[u8]) -> Option<usize>` | `u32` LE length not counting itself, then that many bytes (FlatBuffers `FinishSizePrefixed`) |
| `find::by_fixed(usize) -> impl Fn(&[u8]) -> Option<usize>` | a constant |
| `find::by_msgpack(&[u8]) -> Option<usize>` | one MessagePack value, type bytes walked |

```rust
pub fn by_le32_prefix(data: &[u8]) -> Option<usize> {
    let n = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
    (data.len() >= 4 + n).then_some(4 + n)
}
```

## What takes a finder

`src/record.rs` — `first_end` takes the finder, and every operation in the file is derived from it:

| Now | After |
|---|---|
| `ends(data, finder, separator_len)` | `ends(data, find)` |
| `first_end(data, finder, separator_len)` | `first_end(data, find)` |
| `count(data, finder)` | `count(data, find)` |
| `ends_whole(data, finder, separator_len)` | `ends_whole(data, find)` |
| `Stream::from_buffer(source, finder, separator_len, buf)` | `Stream::from_buffer(source, find, buf)` |
| `Stream::with_capacity(source, finder, separator_len, cap)` | `Stream::with_capacity(source, find, cap)` |
| `Reader::records(finder, separator_len)` | `Reader::records(find)` |
| `Window::walk(finder, separator_len, want)` | `Window::walk(find, want)` |

`Stream<'a, R>` becomes `Stream<R, F>` and `Records<'a, R>` becomes `Records<'a, R, F>`. `Run` and
everything written against it — `skip_records`, `take_records`, `write_to`, `count_records`,
`next_owned`, `walk_from` — is unchanged.

`src/edit.rs` — `Cutter<'a, R>` becomes `Cutter<R, F>`. `records_per_frame`, `frame_records`,
`frame_counts`, `check_range_uniform` and `validate_separator` take a finder in place of a
`&Finder`. `records_per_frame` reads the separator length off the needle today; that line goes.

`check_separator` stays as it is and keeps its name. It is a check on a separator, made where a
separator arrives, and the paths that take a finder do not call it.

## The read window

`record::Reader`'s window grows to hold one record: where it is full and no record has ended, it
doubles. It never shrinks, and a reader reuses it across frames.

A record never spans a frame, so the growth is bounded by the frame being read.

This removes `Records`' run of `count: 0` — the piece of a record longer than the window, handed
out with `separator.len() - 1` bytes held back for a needle that could straddle. A finder that
reads a length header cannot resume from the middle of a record, so no such piece is handed out to
one.

## The frame target

`frame_size` is compared against the end of the record, the separator included.

Today the comparison is against the end of the record's content, which is the record end less the
separator length. A record has no content end under a general finder.

For a separator, this cuts one record earlier than 0.4.x where a record ends within
`separator.len()` bytes after `frame_size`. Under `is_same_separator_cnt` that lands in the first
frame, so it changes the records per frame of the whole file. Files already written are unaffected
and read the same.

## Public API

Existing signatures are kept and become thin layers over the new ones.

| Added | Existing, now calling it with `by_separator` |
|---|---|
| `RecordReader::open_with(path, find)` | `RecordReader::open(path, separator)` |
| `RecordReader::from_file_with(path, file, find)` | `RecordReader::from_file(path, file, separator)` |
| `count_records_in_frame(decoder, start, len, find)` | `cnt_of_separetor_in_frame(decoder, start, len, finder, separator)` |
| `read_records_in_frame(decoder, start, len, skip, cnt, find)` | `records_between_by_separator_in_frame(...)` |
| `count_records_in_buf(data, find)` | `cnt_of_separetor_in_frame_via_buf(data, finder, separator)` |
| `convert_records_to_seekable_zst_reader_with_opts(reader, writer, frame_size, is_same_record_cnt, find, limit_multiplier, opts)` | `convert_to_seekable_zst_reader_with_opts(...)` |
| `compress_records_to_seekable_zst_with_opts(reader, writer, frame_size, is_same_record_cnt, find, limit_multiplier, opts)` | `compress_to_seekable_zst_with_opts(...)` |
| `truncate_records(f, record_len, find)` | `truncate(f, record_len, separator)` |
| `append_records_with(f, data, find, on_missing, level)` | `append_records(f, data, separator, on_missing, level)` |
| `append_frames_with(f, input, from, cnt, find, check)` | `append_frames(...)` |
| `copy_range_with(input, output, from, cnt, find, align, check)` | `copy_range(...)` |

`OnMissingSeparator::Insert` writes the separator, so it stays on the separator entry points.
`append_records_with` takes `OnMissingSeparator::Refuse` alone, as `Insert` has nothing to write.

`RecordReader` holds the finder as `Box<dyn Fn(&[u8]) -> Option<usize> + Send + Sync>`, so the
public type gains no parameter and `records`, `Records` and `RecordIter` stay generic and
monomorphised for every caller inside the crate. This costs one indirect call per record on
`RecordReader`'s own path.

`RecordReader::separator()` returns the separator it was opened with, and an empty slice when it
was opened with `open_with`. `check_separator` refuses an empty separator, so the two cannot be
confused.

`RecordReader::from_file`'s refusal of a frame 0 that holds no record names the separator. The
`open_with` path names the file alone.

## CLI

`--format` is added to every subcommand that takes `--separator`: `compress`, `convert`, `cat`,
`inspect`, `truncate`, `append`, `copy-range`.

What a format is configured with comes as a second flag, `--format-param`.

| `--format` | `--format-param` | Finder |
|---|---|---|
| `sep` | the bytes a record ends with | `by_separator` |
| `fixed` | the length | `by_fixed` |
| `flatbuffers` | refused | `by_le32_prefix` |
| `msgpack` | refused | `by_msgpack` |

`--format sep --format-param "\n"` is the default, which is what `--separator` defaults to now.
`--separator <s>` means `--format sep --format-param <s>`, and is refused alongside
`--format-param`.

`--format-param` carries no clap default. A default of `"\n"` would be read as a length by
`--format fixed` given on its own. `sep` alone takes `"\n"`; `fixed` alone is refused.

Its bytes are taken as they are given, with no escape processing, as `--separator`'s are. In
nushell `"aaa\n"` is already a newline; in zsh it is `$'aaa\n'`.

```rust
pub fn from_spec(name: &str, param: Option<&str>)
    -> anyhow::Result<Box<dyn Fn(&[u8]) -> Option<usize> + Send + Sync>>
```

The only place in the crate that names the formats. What a format is configured with is bound into
the finder it returns, so nothing is carried alongside it.

`append --insert-separator` writes a separator rather than finds one, and is refused for any format
but `sep:`.

The file does not record which format it was written with, as it does not record the separator.

## Tests

`tests/record_formats.rs`:

- Round trip per finder: compress, then read back by index, by range and by iteration.
- Frame 0's record count and `records_per_frame` per finder.
- A record longer than the read window, per finder.
- `truncate`, `append` and `copy-range` at frame boundaries under `flatbuffers`.
- A `flatbuffers` length that runs past the end of its frame: the frame's records before it are
  returned and the fragment is dropped.
- `by_fixed` and `by_le32_prefix` over the same bytes disagreeing, which is what reading a file
  with the wrong format does.

`tests/compress_equivalence.rs` keeps comparing `old_` against `new_`. The frame target change
moves where `new_` cuts, so any fixture whose records land within a separator length of
`frame_size` is expected to differ and the test states which.

## Out of scope

- The nushell plugin. It takes `--separator` and keeps taking it.
- Recording the format in the file.
- Renaming `separator` where it now means a record boundary: `RecordReader::separator`,
  `max_of_separator`, `keep_cnt_of_separators_in_frame`, `cnt_of_separetor_in_frame`,
  `is_same_separator_cnt`.
