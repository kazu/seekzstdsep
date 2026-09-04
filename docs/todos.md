# Todos

Decisions to make and work to build. Neither defects (`docs/bugs.md`) nor costs
(`docs/performances.md`). Ordered by what blocks what.

## Editing an existing file

Designed in `docs/design/2026-08-24-truncate-append-split-concat.md`. `split`, `concat` and
`compress --align` were designed there and dropped; the doc records why.

- [x] `truncate`
- [x] `append`
- [x] `copy-range`
- [x] `append --input-seekable`

## Later

- [ ] [Concurrent append, and reading during an append](#concurrent-append-and-reading-during-an-append)
- [ ] [Separate metadata from lookup](#separate-metadata-from-lookup)

## Blocking nothing

- [ ] [A record read has no way to verify its frame's checksum](#a-record-read-has-no-way-to-verify-its-frames-checksum)
- [ ] [The frame is read with a single `read` call](#the-frame-is-read-with-a-single-read-call)
- [ ] [`RecordReader` cannot be asked to check the uniform count](#recordreader-cannot-be-asked-to-check-the-uniform-count)
- [ ] [`inspect_with_opts` is not re-exported](#inspect_with_opts-is-not-re-exported)
- [ ] [`out_dir` is written out at every call site](#out_dir-is-written-out-at-every-call-site)
- [ ] [The read window and the default frame size are not tuned](#the-read-window-and-the-default-frame-size-are-not-tuned)
- [ ] [`cat --cnt` cannot say "to the end"](#cat---cnt-cannot-say-to-the-end)

## Done

- [x] Indexed access still holds a whole frame

### Concurrent append, and reading during an append

`append` is a read-modify-write of the tail: read the seek table, decode the last data frame,
`set_len` it away, write the replacement, write a new table. Two writers doing that at once corrupt
each other, and no choice of parameter type prevents it — the seek table sits at the end and the
last frame is partial, so appending is a rebuild of the tail rather than a write after it.

Exclusive access is therefore required: a mutex within a process, `flock` across processes. `&mut
File` states that requirement even though the borrow checker cannot enforce it across handles.

Writing concurrently scales by giving each writer its own file and merging later, which is what
segmented logs do. Merging without re-compressing needs every file aligned — `compress` then
`truncate` to the last frame boundary is what provides that — and needs each writer's remainder
carried into its next batch rather than left at the end of its own file, or it lands out of order in
the merged result.

Undecided: what a reader sees while an append runs. Frames before the last one do not move, so
already-written records stay readable, but a reader that opens between the `set_len` and the table
write finds no seek table, and one reading near the tail sees the frame replaced underneath it.

### Separate metadata from lookup

A reader opened per range opens the file, reads the whole seek table, and decompresses frame 0 to
count separators on every call. Two of those are already recorded in `docs/performances.md`.

A reader that acquires the metadata once and answers many lookups fixes both, and makes parallel
reads possible: the metadata is immutable and shareable, so each thread can hold its own file handle
and serve its own record range with no coordination. That is the property the uniform separator
count exists to provide, expressed as an API.

```
struct Reader { /* seek table, records per frame */ }
Reader::open(path)              // once
Reader::records(&self, from, cnt)   // repeatedly, from several threads
```

Undecided: whether `Reader` holds a file handle at all, or hands out per-thread ones. Decoding is
stateful, so it cannot be shared behind `&self`.

### A record read has no way to verify its frame's checksum

A frame ends with a content checksum over its whole decompressed content, so only a decode that
reaches the frame's end verifies it. zeekstd says the same of its decoder: "The frame checksum of
the last decompressed frame will not be verified, if the limit isn't at the end of a frame." A
record read stops at the last record asked for, so a flipped byte elsewhere in that frame comes
back as data and nothing is reported. Corruption inside a block the read does decode is still
caught — zstd reports `Data corruption detected` — so what is missed is the part of the frame the
read skipped.

That is what partial decompression means, not a defect. What reaches a frame's end today: `open` on
frame 0, `total_records` on the last frame, `inspect --no-fast-mode` on every frame, and an edit on
the frames it rewrites. What is missing is the choice — a caller that wants the whole frame checked
has no way to ask for it.

**The remedy is a flag, defaulting to off.** Checking means decoding the frame to its end, which
puts the time back in proportion to the frame size — 16 MiB for three records where the window
decodes 32 KiB — and that proportion is what the window commits exist to remove. `zstd -d` checks by
default because it decompresses everything anyway; that reason does not carry here. Memory is not
the cost: the rest of the frame can be decoded and dropped, on its own thread with its own decoder
and file handle, since the check needs nothing the read produces. Both `--help` texts have to say
which way the flag is set.

`docs/cli.md` and `cat --help` already state the behaviour, and both send the reader to
`docs/bugs.md` for it. Those two references go with this work: the behaviour is not a defect, and a
user-facing text has no business pointing at a bug list — or at this file.

### The frame is read with a single `read` call

`decompressed_range_into` (`src/seekzstdsep_lib.rs`), which `edit::FrameReader` takes a frame's
bytes from:

```rust
let mut data = vec![0u8; len as usize];
let _n = decoder.read(&mut data[..])?;
```

`Read::read` may return fewer bytes than the buffer holds without that being an error, and zeekstd
promises nothing more: `Decoder::decompress` is documented "call this repetetively to fill `buf`",
and its example loops until a call returns 0. A short read would leave the tail of `data` as NUL,
which an edit would copy into the file it writes while counting the frame's separators short — and
the return value is discarded, so nothing would notice.

It has never returned short. 1,212 single reads over fixtures of 600, 50,000 and 200,000 records,
spanning one, three and ten frames at a time, all filled the buffer, and a later read of 24 MB
across 364 frames filled it too. `Decoder::decompress` loops internally until `buf` is full or
`offset_limit` is reached, and this crate sizes every buffer from the seek table, so the limit is
never what stops it. Nothing goes wrong today; what is wrong is that the code rests on how zeekstd
is written rather than on what it promises.

Two halves, and only the first is worth doing now. Erroring on `n != len` removes the part that
cannot be noticed. The loop is wanted only once zeekstd returns short, and a release that does is
what would settle whether it ever will. The rustdoc on `decompressed_range_into` sends the reader
to `docs/bugs.md` for this and has to change with it.

### `RecordReader` cannot be asked to check the uniform count

`RecordReader` locates a record by dividing its index by frame 0's separator count, so everything it
answers rests on every frame holding that count. A file that does not is read at the wrong offsets
and reports nothing. Both sides say so in their rustdoc: `convert_text_to_seekable_zst_reader` cuts
frames by size alone and states that `RecordReader` cannot locate records in its output, and
`RecordReader::records` states the requirement from the other end. `seekzstdsep compress` holds the
count uniform and has no flag to stop it, so writing such a file takes a library caller.

One consequence shows without any wrong data being handed back. `total_records` counts the last
frame directly, while `record` divides by frame 0's count and refuses anything past the last frame,
so a file whose last frame holds more than frame 0 makes the two disagree: with 10 records in the
first frame and 15 in the last, `total_records` is 25 and `record(20)` is `None`, which the nushell
plugin reports as "Row number too large (max: 24)". This crate's compressor cannot write that file —
its last frame holds at most as many as the rest — so it takes another writer or one built by hand.
The `FIXME` on `total_records` (`src/reader.rs`) points at `docs/bugs.md` for this and moves with the
work.

The edit side already has the check. `records_per_frame` (`src/edit.rs`) takes a `SeparatorCheck`:
`FirstFrame` confirms frame 0 ends with the separator, and `TwoFrames` also counts the last frame
that is not allowed to be short, refusing when the two disagree. `append_frames` goes further with
`RangeCheck::EveryFrame`. `RecordReader::open` offers none of it, so `cat`, `record`,
`total_records` and the nushell plugin's row access take frame 0's count on trust. `truncate` sits
between the two: it validates the separator against frame 0, then computes the total from that count
without reading a second frame.

The work is to let `RecordReader::open` take the same `SeparatorCheck`, defaulting to what it does
now. `TwoFrames` is one extra frame decoded per open — `docs/performances.md` already counts frame
0's as a cost of opening — and it catches a file cut by size rather than by record count, which is
what `convert_text_to_seekable_zst_reader` writes. It does not prove uniformity: a file uniform at
both ends and broken in the middle passes. Proving it costs every frame, which is the price the
format exists to avoid, and `inspect --no-fast-mode` is already the way to pay it.

### `inspect_with_opts` is not re-exported

`src/lib.rs` re-exports `inspect` but not `inspect_with_opts`, unlike the other `_with_opts`
functions. Callers reach it through `seekzstdsep::seekzstdsep_lib::inspect_with_opts`, which is the
only place the module path is needed in the public API.

### `out_dir` is written out at every call site

`CompressOptions::out_dir` decides where the staging file goes, and putting it on the same
filesystem as `out_path` is what lets the final move be a reflink instead of a copy. Every caller
in the repository that sets `out_path` also sets `out_dir` to that path's directory — nine call
sites in eight files, with no exception:

```
src/cli.rs:90                                 examples/compress.rs:28
benches/read.rs:131                           bench/src/fixture.rs:144
nu_plugin_zstdsep/src/commands/save.rs:231    tests/common/mod.rs:122, :189
nu_plugin_zstdsep/tests/common/mod.rs:52      tests/seekzstdsep_lib.rs:626
```

A rule with no exceptions belongs in the default: `out_dir: None` with `out_path: Some` should use
that path's parent rather than `env::temp_dir()`. Forgetting it costs a full copy of the output and
no test catches that, since correctness is unchanged.

Undecided: what it does to callers outside the repository. The crate is published, and one that
sets `out_path` without `out_dir` would find its staging file move from the system temporary
directory to the output's own — where the quota may be smaller, where a directory watcher will see
it, and where a network filesystem would be slower than a local `/tmp`. Nobody in this repository
writes that combination, so the change is invisible here and a behaviour change there.

### The read window and the default frame size are not tuned

`READ_BUF_SIZE` — and `READ_FRAME_BUF_SIZE` with it — is 32 KiB
(`src/seekzstdsep_lib.rs`). `rg` reads in 64 KiB, and `--frame-size` defaults to 65536, so the
window is half of both. Neither number was chosen by measurement.

Two questions, and they are not independent: whether the window should be 64 KiB to match, and
whether the default frame size should move once it is. A window the size of a frame reads a whole
frame in one go and never slides; a window half of it slides once per frame. Which is faster is
not obvious — the larger window costs a larger allocation per reader, which every `RecordReader`
pays at open.

Settled by measuring, not by matching a number to another number. `benches/read.rs` has the `cat`
and `into_records` cases to measure with; the window size would have to become a parameter, or the
constant changed and the two builds run alternately.

### `cat --cnt` cannot say "to the end"

`CatArgs::cnt` (`src/main.rs`) is a plain `usize`, while `edit.rs` gives `truncate`,
`append_frames` and `copy_range` a `cnt: Option<u64>` where `None` is the end of the file. `cat`
has no such value, so a caller who wants the rest of a file writes a number large enough to outrun
it, which is what `docs/cli.md` tells them to do.

Nothing is broken by it: the arithmetic that placed a range no longer wraps on such a count, and
the read stops at the last record. What is left is that the same request is written two ways in
one command line. `Option<u64>` on `CatArgs::cnt` makes them one, and is a change to the interface
rather than a defect.
