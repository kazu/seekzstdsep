# On-disk format and the uniform-separator-count invariant

This document describes what a `seekzstdsep` file actually is and, more importantly, **the
invariant that every fast path in this crate depends on**. Read it before changing anything about
how frames are cut.

## Layer 1: the Zstandard Seekable Format

`seekzstdsep` writes a standard [Zstandard Seekable Format][spec] file via the [`zeekstd`] crate. It
adds no bytes of its own — any compliant seekable-zstd reader can read these files, and plain `zstd
-d` decompresses them correctly because the frames are simply concatenated.

[spec]: https://github.com/rorosen/zeekstd/blob/main/seekable_format.md
[`zeekstd`]: https://docs.rs/zeekstd

```
+---------+---------+     +-----------+------------------------+
| frame 0 | frame 1 | ... | frame N-1 | seek table (skippable) |
+---------+---------+     +-----------+------------------------+
```

Each data frame is an independent zstd frame, so decompressing one requires no state from any other.
Every one this crate writes ends with a content checksum — the low 32 bits of the XXH64 of the
frame's decompressed bytes — so only a read that decodes the whole frame can check it, which a
record read does not (`docs/bugs.md`). `compress --no-check` leaves it out, and an operation that replaces a frame writes the
replacement with whatever the frame it replaced carried, so a file keeps whichever setting it was
made with.

The seek table is a zstd *skippable* frame at the end of the file.

Three properties of the seek table matter for anything that modifies a file in place:

1. **Entries store sizes, not absolute offsets.** Each entry is `c_size: u32` and `d_size: u32`
   (8 bytes; 12 with the seek table's own per-entry checksum, which is a different thing from the
   frame checksum above and which zeekstd parses but never writes). A frame's file offset is the
   prefix sum of all
   preceding `c_size` values. Changing one frame's compressed size therefore invalidates the
   position of every frame after it, even though only one table entry changed.
2. **There is no checksum over the seek table body.** The integrity field is 9 bytes — a magic
   number, the frame count, and a size descriptor. Individual entries can be patched in place
   without recomputing anything over the table as a whole.
3. **The integrity field sits at the end** (`Format::Foot`, the default for tables appended to
   compressed data), so a reader locates the table by seeking to the end of the file.

## Layer 2: what seekzstdsep adds

Plain seekable zstd addresses data by **byte** offset. Newline-delimited data is addressed by
**record**. `seekzstdsep` bridges the two with one rule:

> **The invariant:** every frame contains exactly the same number of separators, except the final
> data frame, which may contain fewer.

This holds when `is_same_separator_cnt` is set, which is the default (`--keep-cnt-of-separators-in-frame`).

With that rule, record index to frame index is a division, and no index beyond the seek table is
needed.

### How frames get cut

`convert_records_to_seekable_zst_reader_with_opts` in `src/seekzstdsep_lib.rs` reads into a growing
buffer and asks a record finder (`src/find.rs`) where each record ends; for a separator that finder
is a `memchr::memmem::Finder`. Two different policies decide where a frame ends:

- **First frame:** ends at the first record end at or after `frame_size` bytes, the record boundary
  itself included. So `frame_size` is a *target*, not a bound — frames overshoot to the next record
  boundary. The number of records that landed in this frame becomes `max_of_separator`.
- **Every later frame:** ends after exactly `max_of_separator` separators, regardless of byte size.

The consequence is that **byte sizes drift while record counts stay fixed** — the opposite of what
the `frame_size` parameter name suggests. This is deliberate and is the whole point of the format.

`limit_multiplier` (default 4) bounds the search: if `frame_size * limit_multiplier` bytes accumulate
without a usable frame boundary, compression fails rather than buffering without limit.

### The retry loop

Auto-detected framing can turn out not to fit — a records-per-frame count derived from the first
frame may push a later frame past the buffer limit if record sizes vary. `compress_to_seekable_zst_with_opts`
handles this by catching the internal `CompressErrorData`, halving `max_of_separator` (or doubling
`frame_size` when the count is already below 2), rewinding both reader and writer, and starting over.
This is why that entry point requires `Read + Seek` rather than plain `Read`: it may need to re-read
the input from the beginning.

Output goes to a temporary file and is moved into place with `reflink_copy::reflink_or_copy`, falling
back to a byte copy when the filesystem has no reflink support. A retry therefore never leaves a
half-written file at the destination.

## What depends on the invariant

Three things break if the invariant stops holding. All of them are silent failures — they produce
wrong answers rather than errors.

**1. `RecordReader::records` record lookup.** It reads the separator count of frame 0, then computes:

```text
total_sep_cnt = sep_cnt * frames.len()
frame_idx     = frames.len() * from / total_sep_cnt
idx_in_frame  = from % sep_cnt
```

The `frames.len()` factor cancels, so `frame_idx` is just `from / sep_cnt`. It is correct precisely
because every frame holds `sep_cnt` separators. If frames held varying counts, this would silently
return the wrong records — there is no index to fall back on and no way to detect the drift.

**2. `inspect` fast mode.** It counts separators in frame 0 and the last few frames, and
assumes that count for everything in between. `--no-fast-mode` counts every frame and is the way to
verify the invariant actually holds for a given file.

**3. Shardable parallel scans.** Worker `i` can claim records `[i*N, (i+1)*N)` and seek directly to
its own frames with no shared index and no coordination. This is the property that makes the format
worth using for analytical workloads, and it is the one most easily lost by a well-meaning change to
framing.

## Worked example

50,000 JSONL records, 3,777,780 bytes, compressed with defaults:

```
$ seekzstdsep compress events.jsonl events.jsonl.seek.zst
$ seekzstdsep inspect events.jsonl.seek.zst
```

| Frames | Records each | Decompressed size | Compressed size |
| --- | --- | --- | --- |
| 0..53 (54 frames) | 914 | ~65.6 KB, drifting to ~67.6 KB | ~2.1 KB, drifting to ~3.0 KB |
| 54 | 644 | 48,944 B | 1,622 B |

Total: 155,096 bytes across 55 frames. `54 * 914 + 644 = 50,000`, so the invariant holds exactly.
Note the decompressed sizes drifting upward while the record count stays pinned at 914 — that is the
invariant doing its job.

## Known deviations and limitations

- **The final data frame is partial** (644 records above, not 914). Every consumer must treat the
  last frame as a special case. `inspect` fast mode already does.
- **Records per frame is not persisted.** `max_of_separator` exists only during compression. Every
  reader recovers it by decompressing frame 0 and counting. This works, but it means the file carries
  no record of its own framing parameters — including which separator it was built with. Any feature
  that modifies an existing file (append, update, compaction) needs this value first, so persisting
  it is the prerequisite for all of them.
- **The record format is not persisted either.** A record ends where a finder says it does
  (`src/find.rs`), and which finder wrote the file is not recorded any more than the separator is.
  Reading a file back under a different format addresses different records and generally reports no
  error.
- **`frame_size * limit_multiplier` has a hard floor of 32768**, the size of the internal read
  buffer. The unprocessed-data check runs immediately after each read, so a single read trips it
  when the product is smaller. With the default multiplier of 4 that puts the floor at
  `frame_size >= 8192`; below it, any input larger than the limit fails, including input that is
  entirely separators. Inputs smaller than the limit never reach the check and still succeed. The error text ("No separator was found before reaching the limit size") is
  misleading in that case, and its third field reports `limit_multiplier` under the label "Current
  unprocessed data size".

## Rules for changes

1. **Do not break the uniform separator count.** If a change can produce frames with differing
   counts, `RecordReader::records` returns wrong records with no error. Whatever the change, the
   body of the file must keep a uniform count, or the lookup path must be replaced at the same time.
2. **A partial frame is only ever allowed at the end.** Consumers special-case the tail; they do not
   special-case the middle.
3. **Frame byte sizes are not a contract.** They drift by design. Nothing should assume a frame's
   decompressed size.
4. **Verify with `inspect --no-fast-mode`.** Fast mode assumes the invariant it would be used to
   check, so it cannot detect a violation in the middle of a file.
