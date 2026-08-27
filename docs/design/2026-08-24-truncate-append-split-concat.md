# truncate, append, copy-range

**Status:** `truncate`, `append` and `copy-range` are implemented in `src/edit.rs`. `compress
--align` and the `--input-seekable` path of `append` are design only. `split` and `concat` were
designed here and dropped; see [Why split and concat were dropped](#why-split-and-concat-were-dropped).
**Date:** 2026-08-24, revised 2026-08-27

Operations that modify or derive from an existing file, all working at frame boundaries and never
rewriting the interior. `truncate` and `append` came first. The two that were to follow, `split` and
`concat`, are replaced by three that compose: `compress --align`, `copy-range`, and `append
--input-seekable`.

## Rules

1. **A short frame is permitted only as the last frame of a file.** Never in the interior, never at
   the head. Violating this makes record lookup return wrong records with no error.
2. **`zstd -d` reproduces the original bytes** after every operation.
3. **No operation rewrites more than the region it affects.** Nothing before the first affected byte
   is read or written. The seek table is the one thing rebuilt in full, so it alone is linear in
   frame count.

## The invariant

A file is **aligned** when every frame holds the same number of records — when the last data frame
is full rather than short. `compress --align` produces such a file, `copy-range` produces one
unless `--no-align` releases it from that, and `append --input-seekable` requires it of its target
and produces it again when its input is aligned too.

This is what makes joining two files a byte copy. If the target's last frame holds `r < n` records,
no amount of re-encoding at the seam fixes it: absorbing the seam shifts every frame of the input by
`n - r` records, and the misalignment propagates to the end. The whole input would have to be
re-encoded, which is what `cat b | append a` already does. So the requirement is not caution, it is
what the format leaves available.

The state is called *aligned* here after the flag that produces it.

## Format facts

See `docs/format.md`. Two things shape all of these operations.

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
  the survivors in order. This is why every operation rebuilds the table rather than patch it.
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

`truncate` and `append --input-seekable` additionally need the record count of the **last** data
frame, which validation deliberately excludes. That is a second decompression. Destructive
operations pay both unconditionally. Do not add this to the read path.

Note that validation counts separators. Where the final record has no trailing separator, separator
count and record count differ by one; a record count is a separator count, and a record keeps the
separator that follows it.

**A file whose last byte is not the separator therefore ends in a fragment, not a record.**
Compression preserves it as-is. Joining anything after such a file merges the fragment with the
first appended record into a single record, silently, and shifts every later record index by one.
`append` refuses this by default:

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
`end_frame`, and the record the file ends with is then in the frame before it. `append` addresses
the last frame that carries data; `set_len` drops the empty ones with it.

### Frame 0 ends with the separator

A cheaper check, which `copy-range` uses instead of the comparison above. The compressor ends a
frame immediately after a separator — the first at or after `frame_size` bytes, then every
`max_of_separator`-th one — so **every frame but the last ends with the separator it was cut with**.
A candidate that does not end frame 0 is therefore not that separator, and the count has to be read
off frame 0 anyway. One decompression answers both.

- It needs `F >= 2` rather than `F >= 3`: only frame 0 has to be a frame other than the last.
- It refuses a wrong separator directly rather than by a count that happens to differ, which is what
  makes it strictly better at what it catches: a candidate present in both compared frames in equal
  numbers passes the comparison and fails this.
- What it cannot see is a count that drifts in a frame it does not read. No compressor here writes
  that, but another writer might, so `--check-uniform` asks for the comparison as well.
- Neither catches a candidate that is a prefix of the real separator: with `-=-` for a separator,
  `-` ends frame 0 too and yields three times the count.

Destructive operations keep the comparison unconditionally. What they risk on a wrong count is the
file itself; `copy-range` risks an output that can be deleted and made again.

## Shared machinery

`truncate` and `append` modify `f` in place and are destructive. `copy-range` is not: it reads its
input and writes a second file. Non-destructive use of the first two is the caller's job: clone the
file first, which `cp --reflink=auto` does in about a millisecond where the filesystem supports it.
No operation stages to a temporary file — the bytes before the affected region are already correct
and copying them would make every operation linear in file size.

The shape of a destructive operation is the same each time: read the seek table and keep it, since
`set_len` destroys the copy on disk; validate the separator; optionally decode one frame; optionally
encode replacements; `set_len` to the first affected byte; append the new frames; append a fresh
seek table.

`f` is `&mut File` rather than `&File`. `File::set_len` takes `&self` and `Read`, `Write` and `Seek`
are all implemented for `&File`, so the exclusive borrow is not required by the type system. It is
there to state at the call site that the file is being rewritten.

Errors are `anyhow::Result` as elsewhere in the crate. Every refusal below is an error, not a silent
no-op.

### The crash window

Between `set_len` and the last byte of the new seek table the file is not readable, and the
operations differ in what that costs:

- **`truncate` loses only the seek table.** Every surviving frame is untouched on disk, so the file
  can be repaired by walking the frames and rebuilding the table. What `set_len` removed was going
  to be removed anyway.
- **`append` can lose records.** It `set_len`s away the last data frame before writing its
  replacement, and during that window those records exist only in memory. Encode the replacement
  frame **before** calling `set_len`, so the window is a write rather than a compression.
- **`append --input-seekable` cannot lose records.** Its target's last data frame is full, so
  nothing is re-encoded and the cut is at the end of that frame rather than at its start. Only the
  seek table is lost.
- **`copy-range` has no window.** It does not modify its input.

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

## compress --align

```
compress [-c N] --align --rest <PATH> INPUT OUTPUT
```

`--align` writes no final frame whose record count differs from the rest. The records that would
have formed it are written to `--rest` as plain bytes.

- **`--align` requires `--rest`.** `--rest` without `--align` is an error.
- **`-c` is unchanged**: used when given, auto-detected when not. `--align` does not make it
  required. Whether two files can be joined is decided by the operation that joins them, and it
  refuses on a mismatch; `compress` cannot know whether its output will ever be joined.
- An input holding fewer records than one frame leaves no frames at all. That follows from the
  definition. What the command does about it is settled when it is implemented, not here.

## copy-range

```
copy-range <INPUT> <OUTPUT> --from N [--cnt N] [--no-align] [--check-uniform]
```

Copies a record range out of `INPUT` into `OUTPUT`. **Non-destructive**: `INPUT` is only read.

- `--from` and `--cnt` are record counts, the same unit as `cat` and `truncate`.
- `--cnt` defaults to the end of the file.
- **`--from` must be the first record of a frame, and `--from + --cnt` must be the first record of a
  frame or the end of the file.** Anything else is an error. Nothing is rounded.
- **The result is aligned unless `--no-align` is given.** The frame a file ends with holds whatever
  was left over, so a range reaching it ends in a frame with a record count of its own. That is
  refused by default rather than shortened to the last full frame, which would drop records
  silently. `--no-align` copies the frame as it is, and the result is then a file that cannot be
  joined onto another by copying bytes.
- `OUTPUT` is positional. `-` writes to stdout.
- The frames are copied as bytes. Only the seek table is built fresh.

Cost: the size of the copied range, plus **one** frame decompressed — frame 0, for the check
[above](#frame-0-ends-with-the-separator). One more where the range reaches the last frame, whose
record count is the only one the seek table cannot supply by arithmetic, and one more again under
`--check-uniform`.

In the library this is

```
copy_range(input: &File, output: impl Write, from: u64, cnt: Option<u64>,
           separator: &[u8], align: Alignment, check: SeparatorCheck) -> Result<()>
```

where `Alignment` is `Required` or `NotRequired` and `SeparatorCheck` is `FirstFrame` or
`TwoFrames` — enums rather than `bool`s so the choice is readable at the call site, as
`OnMissingSeparator` is for `append`. `output` is a `Write` rather than a `File` because `-` has to
reach stdout, and the seek table is written last, so nothing seeks back.

Splitting a file is `copy-range` followed by `truncate`, and the boundary is one number in one unit:

```
copy-range a.seek.zst back.seek.zst --from 128000 --no-align
truncate   a.seek.zst --records 128000
```

Record units rather than frame units are what make that pair read as one boundary. Frame units would
state the same cut as two different numbers.

## append --input-seekable

`append` gains an input that is itself a seekable zst, and a range within that input.

| `INPUT` | `--input-seekable` | what happens | `--input-from` / `--input-cnt` |
|---|---|---|---|
| plain | not allowed | appended as records, as now | a record range within the plain input |
| seek.zst | absent | **decompressed**, then appended as records | any range |
| seek.zst | present | frames **copied as bytes** | must fall on frame boundaries |

- **`--input-seekable` declares the byte-copy path.** It changes what the operation refuses, so it
  is explicit rather than inferred. `INPUT` must then be a path — the seek table is at the end, so
  `Seek` is required and stdin cannot serve. If the file is not a seekable zst, refuse.
- **The decompressing path is detected, not declared.** Appending a compressed file's bytes as
  records is never meaningful, so there is nothing to disambiguate. Unlike `cat b | append a`, it
  streams a frame at a time rather than materialising the whole range.
- **`--input-from` and `--input-cnt` are record counts.** Under `--input-seekable` they carry the
  same boundary rule as `copy-range`: `--input-from` is the first record of a frame, and
  `--input-from + --input-cnt` is the first record of a frame or the end of the input.
- **`--insert-separator` applies to the decompressing path only**, and is an error together with
  `--input-seekable`: a byte copy writes nothing at the seam.

The byte-copy path refuses unless all of these hold:

- `n` is equal in both files
- the target's last data frame is full
- the target ends in a whole record

Its input may end in a short frame. That frame becomes the last frame of the result, which is legal
— but the result is then no longer aligned, and a second `--input-seekable` append onto it refuses.

Cost: the size of the copied range, plus one frame of the target decompressed to establish that it
is full.

## Uses

**Delivery.** A stream lands as a compressed file and a plain tail: `base.seek.zst` holding whole
frames, `tail.jsonl` holding what has not reached a frame's worth yet. Writes go to `tail.jsonl`,
which costs a plain append and rebuilds no seek table. When enough has accumulated it is folded in.
`compress --align` is what keeps `base.seek.zst` aligned; `--rest` is what `tail.jsonl` becomes
again.

**Update.** Divide around the range holding the records to change, rewrite that range, and join the
three pieces back: `copy-range` and `truncate` to divide, `-c n` to re-compress the middle, `append
--input-seekable` to join. The middle is a whole number of frames, so no remainder appears and the
result stays aligned. Cost is the frames touched.

This works only when the record count does not change. If it does, the range is no longer a whole
number of frames and everything after it must be re-compressed. There is no way around that; how
much "everything after" is depends on how the data is divided into files, which is an operational
choice.

**Retention.** Dropping the front is `copy-range --no-align` from the boundary into a new file.
Dividing files finely enough also bounds the cost of a count-changing update above.

## Why split and concat were dropped

`split` wrote the back half itself and shortened `f` in place. `copy-range` plus `truncate` does the
same with one non-destructive operation and one existing one, so there was nothing left for it to do.

`concat` appended a whole file to another. Its precondition — the target's last data frame full — is
not something a caller can arrange: `compress` and `append` both leave the remainder in a short final
frame, so it held only when the record count happened to be a multiple of `n`. `compress --align`
turns that coincidence into something a producer can ask for, and `append --input-seekable` is
`concat` with that precondition made reachable.

Neither can be replaced by a pipe. `cat b | append a` produces a correct file, but it re-compresses
`b` whole — which is the cost the byte copy exists to avoid.

## Testing

- **Invariant after every operation**, via `inspect --no-fast-mode`: all interior frames equal, only
  the last data frame short.
- **`zstd -d` reproduces the expected bytes.**
- **Record lookup still correct** — `cat` at several positions against expected records.
- **Round trips**: `copy-range` plus `truncate` then `append --input-seekable` restores the original.
  `append` then `truncate` back to the original length restores `f`.
- **Alignment**: `compress --align` output has no short final frame, and its `--rest` plus the file
  reproduces the input bytes.
- **Composition**: `append` after `truncate` after `append`. Truncate leaves a short final frame,
  which is the state append must already handle.
- **Refusals**: wrong separator; `truncate` beyond the current count; `truncate` to 0; `copy-range`
  from or to a position that is not a frame boundary, reaching a short final frame without
  `--no-align`, and a separator that does not end frame 0; `append --input-seekable` with mismatched `n`, onto a target whose last frame is
  short, or onto a target ending in a fragment; `--align` without
  `--rest` and `--rest` without `--align`; `--insert-separator` with `--input-seekable`; any
  operation on a file with fewer than three data frames.
- **Nothing before the affected byte is touched.** Compare the prefix byte for byte against the
  original after every operation.
- **Frame counts** after every operation: every frame carries data, and the count is the one the
  record count implies.

## Out of scope

Not part of this work and not to be investigated: deleting from the front, inserting at an exact
record position, a short frame at the head of a file, and in-place record update. All are handled by
operating on multiple files instead — retention drops whole files, insertion divides around the
range and writes a middle file. No format change is needed.

## Undecided

- Whether reading across `base.seek.zst` and its plain tail belongs in the crate, or stays an
  operational convention with the caller opening both. All three operations here are write-side.
- Whether the existing `-c, --cnt-of-separator-per-frame` should move to record vocabulary, and what
  to do about `-f` meaning `--from` in `cat` but `--format` in `inspect`, and `-c` meaning a
  separator count in `compress` but a record count in `cat`.
