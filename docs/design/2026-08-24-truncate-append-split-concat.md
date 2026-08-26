# truncate, append, split, concat

**Status:** `truncate` and `append` are implemented in `src/edit.rs`. `split` and `concat` are design
only.
**Date:** 2026-08-24

Four operations that modify an existing file, all working at frame boundaries and never rewriting
the interior. Implement in this order, each reusing the one before: `truncate`, `append`, `split`,
`concat`.

## Rules

1. **A short frame is permitted only as the last frame of a file.** Never in the interior, never at
   the head. Violating this makes record lookup return wrong records with no error.
2. **`zstd -d` reproduces the original bytes** after every operation.
3. **No operation rewrites more than the region it affects.** Nothing before the first affected byte
   is read or written. The seek table is the one thing rebuilt in full, so it alone is linear in
   frame count.

## Format facts

See `docs/format.md`. Two things shape all four operations.

**Seek table entries store sizes, not offsets.** Moving a frame changes no entry; changing one
frame's size invalidates the position of every frame after it.

**Every frame carries data.** `RawEncoder::end_frame` logs an entry whether or not data is pending
and `Encoder::finish` calls it unconditionally, so the compressor ends each frame only when the next
one starts and leaves the last to `finish`. Nothing here may re-emit an empty frame at the end: it
would land in the interior of any file later appended to and shift every subsequent frame index by
one. `cat_data` computes `frame_idx = from / n` and indexes directly, so the result would be
silently wrong records, not an error.

## zeekstd gotchas

Verified against zeekstd 0.6.2. Re-check when the dependency moves; the crate source is the
authority, not this list.

- **No API removes an entry from a `SeekTable`.** Build a fresh `SeekTable::new()` and `log_frame`
  the survivors in order. This is why all four operations rebuild the table rather than patch it.
- `RawEncoder::compress` and `end_frame` **fill a caller-supplied buffer; they do not write to a
  writer.** Loop `compress` until it has consumed the input, and `end_frame` until nothing is left.
- The sizes for `log_frame` come from `RawEncoder::into_seek_table()`, which holds exactly the
  frames just written. `end_frame` returns only the epilogue byte count.
- `log_frame` takes `u32`; every size getter returns `u64`.
- `frame_start_comp(i)` errors for `i >= num_frames()`. Address the end with `frame_end_comp(n-1)`
  or `size_comp()`.
- `upper_frame` is inclusive: one frame is `lower_frame(k).upper_frame(k)`.
- `into_serializer` consumes the table, which is `Clone`. `Serializer` implements `Read`.
- `FrameSizePolicy` defaults to 2 MiB of uncompressed data. Re-encoding needs an explicit larger
  policy, or a frame of `n` records exceeding it is split, putting a short frame in the interior.

Match the frame being replaced: `Encoder::new` defaults, and the checksum flag read from bit 2 of
that frame's header descriptor, so a re-encoded frame is consistent with the rest of the file. New
files get a checksum; files made without one keep none.

## Separator validation

Required before any destructive operation. The file does not record which separator it was built
with, and cutting with the wrong one cuts in the wrong place.

Let `F` be the number of frames. Decompress data frames `0` and `1`
and count separators with the candidate:

- count is 0 → refuse, the separator does not occur
- counts differ → refuse, either the separator is wrong or the file lacks a uniform count
- counts match → that is `n`, records per frame

**This needs `F >= 3`.** With `F == 2`, frame 1 is the legitimately short final frame and comparing
it against frame 0 refuses a valid file. With `F < 3` only the zero case is detectable; that is a
limit to accept, not to work around.

`truncate` and `concat` additionally need the record count of the **last** data frame, which
validation deliberately excludes. That is a second decompression. Destructive operations pay both
unconditionally. Do not add this to the read path.

Note that validation counts separators. Where the final record has no trailing separator, separator
count and record count differ by one; a record count is a separator count, and a record keeps the
separator that follows it.

**A file whose last byte is not the separator therefore ends in a fragment, not a record.**
Compression preserves it as-is. Joining anything after such a file merges the fragment with the
first appended record into a single record, silently, and shifts every later record index by one.
`append` and `concat` refuse this by default:

```
enum OnMissingSeparator { Refuse, Insert }
```

`Refuse` is the default. `Insert` writes one separator at the join. An enum rather than a `bool` so
the choice is readable at the call site. On the CLI this is `--insert-separator`.

Separators are counted as non-overlapping matches, so ending with the separator's bytes is not the
same as ending with a record: with `\n\n` for a separator, `a\n\n\n` holds one record and one byte
over. The test is that the last match ends the data. A separator that overlaps itself is also the
case `Insert` cannot serve — writing one leaves another fragment — and it refuses there.

A frame carrying no data is not the last record's frame. `Encoder::finish` writes one after an
`end_frame`, and the record the file ends with is then in the frame before it. `append` and `concat`
address the last frame that carries data; `set_len` drops the empty ones with it.

## Shared machinery

All four modify `f` in place and are destructive. Non-destructive use is the caller's job: clone the
file first, which `cp --reflink=auto` does in about a millisecond where the filesystem supports it.
No operation stages to a temporary file — the bytes before the affected region are already correct
and copying them would make every operation linear in file size.

The shape is the same each time: read the seek table and keep it, since `set_len` destroys the copy
on disk; validate the separator; optionally decode one frame; optionally encode replacements;
`set_len` to the first affected byte; append the new frames; append a fresh seek table.

`f` is `&mut File` rather than `&File`. `File::set_len` takes `&self` and `Read`, `Write` and `Seek`
are all implemented for `&File`, so the exclusive borrow is not required by the type system. It is
there to state at the call site that the file is being rewritten.

Errors are `anyhow::Result` as elsewhere in the crate. Every refusal below is an error, not a silent
no-op.

### The crash window

Between `set_len` and the last byte of the new seek table the file is not readable, and the two
operations that shorten it fail differently:

- **`truncate` and `split` lose only the seek table.** Every surviving frame is untouched on disk, so
  the file can be repaired by walking the frames and rebuilding the table. What `set_len` removed was
  going to be removed anyway.
- **`append` and `concat` can lose records.** Both `set_len` away the last data frame before writing
  its replacement, and during that window those records exist only in memory.

For `append` and `concat`, encode the replacement frame **before** calling `set_len`, so the window
is a write rather than a compression.

## truncate

```
truncate(f: &mut File, record_len: u64, separator: &[u8]) -> Result<()>
```

`record_len` is the resulting length in records, not the number removed.

1. Validate the separator, obtaining `n`. Decompress the last data frame to get the current total.
2. Refuse if `record_len` exceeds the total. Refuse if `record_len == 0`; a zero-frame file
   makes `seek_table_decomp_frames` return `None` and panics every reader.
3. `k = record_len / n`, `rem = record_len % n`.
4. If `rem != 0`, decode frame `k` and encode its first `rem` records as one frame.
5. `set_len(frame_end_comp(k-1))`, then append the re-encoded frame if there is one.
6. Append a fresh table built from entries `0..k` and the re-encoded frame if there is one.

Nothing before `frame_end_comp(k-1)` is read or written. `k == 0` — keeping fewer records than one
frame holds — cuts at 0, so the re-encoded frame becomes the whole file.

Cost: at most one frame decoded and re-encoded.

A result of fewer than three frames cannot be validated again, so nothing here can be applied to it
a second time. That is the `F >= 3` floor above, not a limit truncate adds.

Truncation cuts immediately after a separator, so the output always ends with one and a trailing
fragment in the input is dropped. Truncating to the current record length is therefore not a no-op
on a file that ends in a fragment.

## append

```
append(f: &mut File, data: impl Read, separator: &[u8],
       on_missing: OnMissingSeparator) -> Result<()>
```

1. Validate the separator, obtaining `n`.
2. Decode the last data frame, which generally holds fewer than `n` records. If its last byte is
   not the separator, refuse, or insert one according to `on_missing`. This costs nothing extra;
   the frame is already decoded.
3. Concatenate its records with `data` and re-cut into frames of exactly `n` records, the remainder
   becoming the new final short frame. Encode at least the first of them before step 4.
4. `set_len(frame_start_comp(last_data_frame))`, then append the new frames.
5. Append a fresh table built from the entries before the last data frame and the new ones.

Cost: one frame re-encoded, plus compression of `data`.

**Cut at the last data frame, not at `size_comp()`.** Appending after a short frame leaves a short
frame in the interior.

**`n` is fixed by the frames already on disk and must not be changed.** The existing retry loop in
`compress_to_seekable_zst_with_opts` responds to `CompressErrorData` by halving records-per-frame and
recompressing from the start. Doing that here would place frames of `n/2` after frames of `n` —
exactly the violation above. It is also unusable for other reasons: it writes a complete file
including a seek table, and it requires `Read + Seek`.

The implementation therefore does not go through the compressor at all. It counts separators itself
and hands each group of `n` records to the same encoder `truncate` re-encodes with, which writes
into the file being appended to rather than into a file of its own. There is then no
`frame_size * limit_multiplier` bound to exceed, and `data` needs to be no more than `Read`.

What is left is zeekstd's own ceiling: `SEEKABLE_MAX_FRAME_SIZE`, 1 GiB of uncompressed data per
frame, which no frame size policy can raise. A group of `n` records larger than that would be split
in two, putting a short frame in the interior, so append refuses when the encoder returns more
frames than it was given groups. The compressor has the same hazard at 2 MiB, which is
`docs/bugs.md`, not this.

An empty `data` is a no-op: return without rewriting.

## split

```
split(f: &mut File, back: impl Write, record_len: u64, separator: &[u8]) -> Result<u64>
```

`f` becomes the front half; the back half is written to `back`. Returns the record count left in
`f`, which is `record_len` rounded down to a frame boundary. Rounding down keeps both halves free of
interior short frames.

1. Validate the separator, obtaining `n`. `k = record_len / n`. Refuse `k == 0` or `k >= F`; either
   would leave one side empty.
2. Write `f`'s compressed bytes `[frame_start_comp(k), size_comp())` to `back`, then a table built
   from entries `k..F`.
3. `set_len(frame_end_comp(k-1))` on `f`, then append a table built from entries `0..k`.

No decompression, and the copied entries need no adjustment. The front half is never written — it is
already correct in place — so the cost is the size of the back half, not of the file.

**Stop reading at `size_comp()`, not at end of file.** The bytes after it are `f`'s seek table.
Copying them puts a stale skippable frame in the middle of `back`, which both `zstd -d` and
`from_seekable` ignore, so it survives every obvious test as silent bloat.

Write `back` before shortening `f`. Doing it the other way round destroys the only copy of those
frames first.

Exact-position splitting is not offered; see Out of scope.

## concat

```
concat(f: &mut File, back: impl Read + Seek, separator: &[u8],
       on_missing: OnMissingSeparator) -> Result<()>
```

`back` is appended to `f`.

**Only when `f`'s last data frame is full.** Refusing is better than silently producing an interior
short frame. Fullness is the record count of `f`'s last data frame, from the separator validation
above.

1. Validate both, obtaining `n_f` and `n_back`. Refuse unless equal.
2. Decompress `f`'s last data frame. Refuse unless it holds exactly `n` records. If its last byte is
   not the separator, refuse, or insert one according to `on_missing` — which requires re-encoding
   that frame, so it is no longer the pure byte copy the rest of the operation is.
3. `set_len(frame_end_comp(last_data_frame))` on `f`. This drops `f`'s seek table.
4. Append `back`'s compressed bytes `[0, size_comp())`.
5. Append a fresh table built from `f`'s entries, then `back`'s.

No decompression beyond validation, and `f`'s existing frames are never rewritten, so the cost is
the size of `back`.

**`cat f back` is not a substitute.** It decompresses correctly under `zstd -d` but is not seekable:
only the trailing table is found, and its offsets are relative to `back`.

Both inputs need `F >= 3` for validation, so `concat` cannot be applied to very small files.

## Testing

- **Invariant after every operation**, via `inspect --no-fast-mode`: all interior frames equal, only
  the last data frame short.
- **`zstd -d` reproduces the expected bytes.**
- **Record lookup still correct** — `cat` at several positions against expected records.
- **Round trips**, which work because every operation is destructive on `f`: `split` then `concat`
  restores `f`. `append` then `truncate` back to the original length restores `f`.
- **Composition**: `append` after `truncate` after `append`. Truncate leaves a short final frame,
  which is the state append must already handle.
- **Refusals**: wrong separator, `truncate` beyond the current count, `truncate` to 0, `split` at 0
  or past the end, `concat` with a short frame at the join, `concat` with mismatched `n`, `append`
  and `concat` onto a file ending in a fragment, any operation on a file with fewer than three data
  frames.
- **Nothing before the affected byte is touched.** Compare the prefix byte for byte against the
  original after every operation.
- **Frame counts** after every operation: every frame carries data, and the count is the one the
  record count implies.

## Out of scope

Not part of this work and not to be investigated: deleting from the front, inserting at an exact
record position, a short frame at the head of a file, and in-place record update. All are handled by
operating on multiple files instead — retention drops whole files, insertion splits around the range
and writes a middle file. No format change is needed.
