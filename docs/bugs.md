# Known bugs

Three lists. An entry only belongs in the first one if a command reproduces it. A fixed entry keeps
its line with the box checked and loses the section under it: the list is what is known, and the
investigation is only worth reading while the bug is open.

## Reproduced

Each has a command below that produces the stated output on the benchmark fixture. Ordered by
damage, worst first.

- [ ] [A file that cannot be opened panics instead of being reported](#a-file-that-cannot-be-opened-panics-instead-of-being-reported)
- [ ] [`inspect` panics on a frame that fails to decode](#inspect-panics-on-a-frame-that-fails-to-decode)

## Not reproduced

Read off the source, not observed. Each says what would settle it.

- [ ] [The frame is read with a single `read` call](#the-frame-is-read-with-a-single-read-call)
- [ ] [Wrong records are returned without an error when the invariant does not hold](#wrong-records-are-returned-without-an-error-when-the-invariant-does-not-hold)
- [ ] [A frame 0 with no separator divides by zero](#a-frame-0-with-no-separator-divides-by-zero)
- [ ] [`old_cnt_of_separetor_in_frame_via_buf` underflows on short input](#old_cnt_of_separetor_in_frame_via_buf-underflows-on-short-input)
- [ ] [The record count and the reachable indices disagree on a foreign file](#the-record-count-and-the-reachable-indices-disagree-on-a-foreign-file)
- [ ] [Adding `from` and `cnt` can overflow and the length then underflows](#adding-from-and-cnt-can-overflow-and-the-length-then-underflows)

## Not confirmed to be a bug

The behaviour is observed; whether it is wrong is undecided.


Performance costs that are not defects live in `docs/performances.md`.

## Fixed

- [x] `cat` returns one record more than asked for
- [x] The unwritten tail of the buffer is returned as data — the NUL half is the single `read` below
- [x] `cat_data` panics when the request runs past the last record
- [x] `compress` with no arguments produces nothing
- [x] Three tests are commented out and their cases are uncovered
- [x] `compress_to_seekable_zst` discards its output and returns `Ok`
- [x] `compress` cuts a frame at 2 MiB whatever the record count is
- [x] The encoder emits a trailing empty frame
- [x] `seekzstdsep compress` runs the compressor twice

### A file that cannot be opened panics instead of being reported

`src/seekzstdsep_lib.rs:735` in `cat_data` and `:815` in `inspect_with_opts`, both

```rust
std::fs::File::open(&input).expect("fail open")
```

A mistyped path is enough:

```
$ seekzstdsep cat /tmp/does-not-exist.zst --from 0 --cnt 1
thread 'main' panicked at src/seekzstdsep_lib.rs:735:52:
fail open: Os { code: 2, kind: NotFound, message: "No such file or directory" }
$ echo $?
101
```

**Why this is wrong rather than a matter of taste: both functions are public API.**
`src/lib.rs:16` and `:22` re-export them, and both are declared to return a `Result`. A caller that
writes `match cat_data(..)` cannot catch this — the process is gone before the `match` is reached.
The signature promises a reported failure and the body does not deliver one. In a binary,
`.expect()` on a path the user typed would be defensible; in a library entry point it takes the
choice away from the caller.

Rust's line is not "did it fail" but "is this a bug in the program or an expected failure". A
violated invariant panics; I/O and user input return `Err`. A missing path is the second kind.

The inconsistency is visible from the shell, since both are the same class of user error:

```
$ seekzstdsep cat missing.zst --from 0 --cnt 1     ; echo $?   # 101, backtrace note
$ seekzstdsep cat records.seek.zst --from 999999 --cnt 1 ; echo $?   # 1, "Error: record 999999 is past the end"
```

Fixed by `?` at both sites. Neither is reached by any current test.

### `inspect` panics on a frame that fails to decode

`src/seekzstdsep_lib.rs:1123`

`cnt_of_separetor_in_frame` is called inside a `map` closure and its error is taken with `expect`,
so a frame that will not decompress aborts the process rather than being reported. Frames carry a
content checksum, so one flipped byte reaches it.

```
> ^seekzstdsep compress in.jsonl out.seek.zst --frame-size 16384
> let b = (open --raw out.seek.zst | into binary)
> let byte = ($b | bytes at 200..<201)
> bytes build ($b | bytes at ..<200) (if $byte == 0x[ff] { 0x[00] } else { 0x[ff] }) ($b | bytes at 201..) | save --force out.seek.zst
> ^seekzstdsep inspect out.seek.zst --no-fast-mode
thread 'main' panicked at src/seekzstdsep_lib.rs:1123:18:
failt to get count: Data corruption detected
```

`cat --from 0 --cnt 1` on the same file prints `Error: Data corruption detected` and exits 1, which
is what `inspect` should do. Fixing it means collecting the closure's results
instead of mapping straight to `InspectResult`.

### Wrong records are returned without an error when the invariant does not hold

Stated in the rustdoc on `cat_data`. The frame is located by dividing `from` by frame 0's separator
count, which is only valid if every frame holds the same count.

Settled by compressing a file with `is_same_separator_cnt` false, reading a known record index from
it, and comparing against the same index in the uncompressed source.

### A frame 0 with no separator divides by zero

`src/reader.rs:143-155`, in `RecordReader::records`, which `cat_data` is now a wrapper over.

`total_sep_cnt = self.sep_cnt * self.frames.len()` (143) is the divisor for `frame_idx` (144) and
`end_frame_idx` (154), and `from % self.sep_cnt` (151) divides by `sep_cnt` directly.

Settled by running `cat` with a `--separator` that does not occur in the file, or on a file whose
first frame holds no complete record. The nushell plugin refuses that separator before it gets
here (`nu_plugin_zstdsep/src/source.rs`); `cat_data` does not.

### The frame is read with a single `read` call

`src/seekzstdsep_lib.rs`, in `decompressed_range`, which `lines_between_by_separator_in_frame` and
`cnt_of_separetor_in_frame` both take the frame's bytes from:

```rust
let mut data = vec![0u8; len as usize];
let _n = decoder.read(&mut data[..])?;
```

`Read::read` may return fewer bytes than the buffer holds without that being an error. There is no
loop and the return value is discarded, so a short read would leave the remainder of `data` as NUL:
the reader would return them as record bytes and the counter would count separators short.

zeekstd does not promise a full buffer. `Decoder::decompress` is documented "call this repetetively
to fill `buf`", and its example loops until a call returns 0.

It has never been observed to return short. 1,212 single reads over fixtures of 600, 50,000 and
200,000 records, spanning one, three and ten frames at a time, all filled the buffer, and a later
read of 24 MB across 364 frames filled it too. `Decoder::decompress` loops internally until `buf`
is full or `offset_limit` is reached, and this crate sizes every buffer from the seek table, so the
limit is never the one that stops it.

So the code depends on how zeekstd is written rather than on what it promises, and nothing today
goes wrong. Settled by a zeekstd release that returns short, which would want the loop rather than
a fallback that copes with NUL.

The two sites this was taken from also differed in what they did with a read error: one called
`expect` and panicked, the other returned it. The shared body returns it, so
`lines_between_by_separator_in_frame` no longer panics there.

### `old_cnt_of_separetor_in_frame_via_buf` underflows on short input

`src/seekzstdsep_lib.rs:643`

```rust
let max_pos = data.len() - sep_len;
```

Underflows when `data` is shorter than the separator. Arithmetically certain from the expression,
but never executed. Carries a `FIXME`. The function is superseded by
`cnt_of_separetor_in_frame_via_buf` and is on no current path, but it is `pub`.

Settled by calling it with a buffer shorter than the separator.

### The record count and the reachable indices disagree on a foreign file

`RecordReader::total_records` (`src/reader.rs:103`) counts the last frame directly, while
`RecordReader::record` (`src/reader.rs:113`) divides the index by frame 0's count and refuses
anything past the last frame. The two agree only while no frame holds more records than frame 0.

A file whose last frame holds more reports records that no index can reach: with two frames, 10
records in the first and 15 in the last, `total_records` is 25 and `record(20)` is `None`. Through
the nushell plugin the error then reads "Row number too large (max: 24)" for index 20.

This crate's compressor cannot write such a file — the last frame holds at most as many as the
rest. Settled by a file from another writer, or by one built by hand.

### Adding `from` and `cnt` can overflow and the length then underflows

`src/reader.rs:154`, in `RecordReader::records`. `from` and `cnt` are `usize` taken straight from
the command line, so `from + cnt + 1` wraps in a release build. Clamping `end_frame_idx` to the
last frame does not help here: a wrapped value can make it smaller than `frame_idx`, and then

```rust
self.frames[end_frame_idx].0 + self.frames[end_frame_idx].1 - start
```

underflows.

Settled by passing a `--cnt` near `usize::MAX` and seeing which of the two faults comes first.
