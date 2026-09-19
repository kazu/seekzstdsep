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
- [ ] [`cat` and the nushell plugin cannot ask for the frame check](#cat-and-the-nushell-plugin-cannot-ask-for-the-frame-check)
- [ ] [`inspect_with_opts` is not re-exported](#inspect_with_opts-is-not-re-exported)
- [ ] [`out_dir` is written out at every call site](#out_dir-is-written-out-at-every-call-site)
- [ ] [The read window and the default frame size are not tuned](#the-read-window-and-the-default-frame-size-are-not-tuned)
- [ ] [`cat --cnt` cannot say "to the end"](#cat---cnt-cannot-say-to-the-end)

## Done

- [x] Indexed access still holds a whole frame
- [x] Counting a frame's records holds the whole frame
- [x] The nushell plugin cannot open a file that has no separator
- [x] `RecordReader` cannot be asked to check the uniform count
- [x] Library copy-and-replace append

### Concurrent append, and reading during an append

- CLI append support (git_task 002).
- Cooperating truncate and copy-range, including CLI support (git_task 003/004).
- Plugin support.

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

`decompressed_range_into` (`src/seekzstdsep_lib.rs`), which `edit::FrameReader::read_frame` takes a
frame's bytes from:

```rust
let mut data = vec![0u8; len as usize];
let _n = decoder.read(&mut data[..])?;
```

`Read::read` may return fewer bytes than the buffer holds without that being an error, and zeekstd
promises nothing more: `Decoder::decompress` is documented "call this repetetively to fill `buf`",
and its example loops until a call returns 0. A short read would leave the tail of `data` as NUL,
which `append` would copy into the file it writes as the records it re-cuts — and the return value
is discarded, so nothing would notice.

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

### `cat` and the nushell plugin cannot ask for the frame check

`RecordReader::verifying` is a library call, and nothing in `src/cli.rs`,
`src/main.rs` or `nu_plugin_zstdsep` reaches it, so `cat` and the plugin read on trust with no way
to say otherwise. What it costs is in `docs/performances.md`; what has to be decided is whether it
is a flag or the default for those two.

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
(`src/seekzstdsep_lib.rs`). `rg` reads in 64 KiB, and `--frame-size` defaults to 131072, so the
window is half of one and a quarter of the other. The frame size was measured (`docs/bench/frame-size.svg`); the window was not.

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
