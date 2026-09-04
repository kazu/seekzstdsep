# Known bugs

Five lists, and an entry moves between them rather than being deleted. Which one it is in says how
sure it is: reproduced by a command, read off the source, observed but undecided, real in code that
nothing calls, or fixed. A fixed entry keeps its line with the box checked and loses the section
under it — the list is what is known, and the investigation is only worth reading while the bug is
open.

Performance costs that are not defects live in `docs/performances.md`.

## Reproduced

Each has a command below that reproduces the stated output. Ordered by damage, worst first.

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
- [x] A path the user typed panics instead of being reported
- [x] `cat`, `truncate` and `append` do not say which file failed to open
- [x] The compressor's retry panics instead of reporting a rewind that fails
- [x] A separator that does not occur in frame 0 divides by zero
- [x] `inspect` panics on a frame that fails to decode
- [x] The requested count overflows while the range is placed
- [x] The requested index overflows while the frame is placed
- [x] `cat` returns the wrong number of records when the requested count overflows
