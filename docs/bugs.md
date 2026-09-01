# Known bugs

Five lists, and an entry moves between them rather than being deleted. Which one it is in says how
sure it is: reproduced by a command, read off the source, observed but undecided, real in code that
nothing calls, or fixed. A fixed entry keeps its line with the box checked and loses the section
under it — the list is what is known, and the investigation is only worth reading while the bug is
open.

Performance costs that are not defects live in `docs/performances.md`.

## Reproduced

Each has a command below that reproduces the stated output. Ordered by damage, worst first.

- [ ] [A path the user typed panics instead of being reported](#a-path-the-user-typed-panics-instead-of-being-reported)
- [ ] [A separator that does not occur in frame 0 divides by zero](#a-separator-that-does-not-occur-in-frame-0-divides-by-zero)
- [ ] [`inspect` panics on a frame that fails to decode](#inspect-panics-on-a-frame-that-fails-to-decode)
- [ ] [The requested count overflows while the range is placed](#the-requested-count-overflows-while-the-range-is-placed)
- [ ] [`cat` returns the wrong number of records when the requested count overflows](#cat-returns-the-wrong-number-of-records-when-the-requested-count-overflows)

## Not reproduced

Read off the source, not observed. Each says what would settle it.

## Not confirmed to be a bug

The behaviour is observed; whether it is wrong is undecided.

## Not worth an entry of their own

Real, in code that nothing calls. Kept so the next reader does not derive them again; each goes
when its code does.

- [ ] `old_cnt_of_separetor_in_frame_via_buf` underflows on short input — superseded, called from
  nowhere, and the only one of the four superseded functions with no `#[deprecated]`. Deleting it
  at 0.5.0 takes the underflow with it.

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

### A path the user typed panics instead of being reported

`compress` and `inspect` take a failed open, create or decode with `.expect()`, so the process dies
with exit 101 where the rest of the CLI reports the error and exits 1.

```
$ seekzstdsep inspect /tmp/does-not-exist.zst          # fail open: Os { code: 2, ... }
$ seekzstdsep inspect plain.zst                        # cannot open decoder: Unknown frame descriptor
$ seekzstdsep compress /tmp/does-not-exist.txt out.zst # failed to open input file: Os { code: 2, ... }
```

`src/cli.rs` (open input, create output, remove input under `--rm`), `src/seekzstdsep_lib.rs` in
`inspect_with_opts` (open, decoder, seek table), `src/main.rs` (the result of `inspect_with_opts`).

Every one of those sits in a function that already returns `anyhow::Result`, so the fix is `?` at
each site. Leaving the one in `src/main.rs` moves the panic rather than removing it. `?` alone drops
the path from the message; `.with_context` keeps it. No test reaches any of them.

### A separator that does not occur in frame 0 divides by zero

`RecordReader::from_file` counts frame 0's separators at open and keeps the result as the record
count of every frame. Nothing refuses a count of 0, and `records_request` (`src/reader.rs`), which
every record range goes through, divides by it.

```sh
seekzstdsep cat clean.seek.zst --from 0 --cnt 1 --separator ZZZZ
# => thread 'main' panicked at src/reader.rs:216:25: attempt to divide by zero
```

Debug and release alike. Reached by a `--separator` the file was not built with, or by a file whose
frame 0 holds no complete record; the default `\n` occurs in any text file, so it takes an explicit
flag or a foreign file. `RecordReader::record` returns `None` on a count of 0, so the nushell
plugin's row access is unaffected, and the plugin refuses such a separator earlier anyway
(`nu_plugin_zstdsep/src/source.rs`).

`edit.rs` refuses the same condition where it reads the count — `bail!("the separator does not occur
in frame 0")` in `records_per_frame`. `RecordReader::from_file` is where the reader would do the
same.

### `inspect` panics on a frame that fails to decode

`src/seekzstdsep_lib.rs`, in `inspect_with_opts` — the fourth `.expect()` in that function, and the
one the entry above does not cover.

`cnt_of_separetor_in_frame` is called inside a `map` closure, which cannot propagate, so its error
is taken with `expect` and a frame that will not decompress aborts the process. Every frame this
crate writes ends with a content checksum, so one flipped byte reaches it.

In nushell:

```nu
1..2000 | each {|i| $'{"i":($i)}' } | str join "\n" | $"($in)\n" | save --raw --force in.jsonl
^seekzstdsep compress in.jsonl out.seek.zst --frame-size 16384
let b = (open --raw out.seek.zst | into binary)
bytes build ($b | bytes at ..<200) (if ($b | bytes at 200..<201) == 0x[ff] { 0x[00] } else { 0x[ff] }) ($b | bytes at 201..) | save --force out.seek.zst
^seekzstdsep inspect out.seek.zst --no-fast-mode
# => thread 'main' panicked at src/seekzstdsep_lib.rs:1119:18:
#    failt to get count: Restored data doesn't match checksum
```

`cat --from 0 --cnt 1` on that file prints `Error: Restored data doesn't match checksum` and exits
1, which is what `inspect` should do.

`?` is not the fix here. The closure has to return `Result<InspectResult>` and the `map` be
collected as `Result<Vec<_>>`.

### The requested count overflows while the range is placed

`frame_range` (`src/edit.rs`) places the end of a range with `(from + c) % n`, which overflows
before the arm below it can refuse the same `c` with `from.saturating_add(c)`. A debug build panics;
a release build wraps and lands on a refusal by accident, not always the one that says what the
caller did wrong.

```sh
seekzstdsep compress in.jsonl clean.seek.zst --frame-size 65536   # 1333 records per frame
seekzstdsep copy-range clean.seek.zst out.seek.zst --from 1333 --cnt 18446744073709551615
# debug   => thread 'main' panicked at src/edit.rs:751:20: attempt to add with overflow
# release => Error: refusing to copy ... which holds 200000 records
```

`--cnt 18446744073709550283` on the same file sums to 0 instead, which the first arm takes for a
frame boundary; a release build then fails with `Error: frame index too large`.

`--from` has to be a frame boundary or an earlier refusal fires first. `copy-range` and
`append --input-seekable` are the two callers, and nothing is written either way, so the damage is
the panic and the misleading refusal.

### `cat` returns the wrong number of records when the requested count overflows

`records_request` (`src/reader.rs`) places the end of a range with `from + cnt + 1`. `from` and
`cnt` are `usize` taken straight from the command line, so the sum wraps. A debug build panics on
the add; a release build carries the wrapped value into the length and answers with it.

```sh
seekzstdsep compress in.jsonl clean.seek.zst --frame-size 65536   # 200000 records, 1333 per frame
seekzstdsep cat clean.seek.zst --from 1333 --cnt 18446744073709550282 | wc -l
# => 0, exit 0, nothing on stderr, where 198667 records exist from index 1333
```

The wrapped sum puts the end frame at 0, so which wrong answer comes back depends on where `from`
lands: `--from 0` returns 1333 of the 200000 that exist, `--from 1333` returns 0 of 198667, and
`--from 2666` returns 197334 of 197334, which is right. The count never exceeds `--cnt` —
`take_records` caps it and `end_frame_idx` is clamped to the last frame — so the fault is always a
short answer reported as a whole one, which a caller cannot tell from a file that holds no more.

`--cnt 1000000` is fine; it takes a `--cnt` within a frame's worth of `u64::MAX`. That is what a
caller writes to mean "to the end", which `cat` has no other way to say: `CatArgs::cnt` is a plain
`usize`, while `edit.rs` gives `truncate`, `append_frames` and `copy_range` a `cnt: Option<u64>`
where `None` is the end. `checked_add` fixes the fault; the type is why anyone reaches it.

The same sum overflows in `edit.rs`, above.

