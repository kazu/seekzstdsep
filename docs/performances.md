# Performance

Costs that are known and still present. Tick an item when it is gone.

Unresolved: [the trusted-append option may affect existing append performance](#existing-append-performance-after-adding-the-trusted-count-option).

Ordered by how much each one costs, worst first.

- [ ] [Frame 0 is read on every call](#frame-0-is-read-on-every-call) — doubles the decoding per lookup
- [ ] [The seek table is read in full on every call](#the-seek-table-is-read-in-full-on-every-call) — grows with the file: 9 kB at a million records, 90 kB at ten million
- [ ] [The frame table is built in full when three entries are needed](#the-frame-table-is-built-in-full-when-three-entries-are-needed) — also grows with the file, but only an allocation and a pass
- [ ] [Checking the frames a read walks costs 3.4% of a range that crosses them](#checking-the-frames-a-read-walks-costs-34-of-a-range-that-crosses-them) — only where the caller asks for it
- [ ] [The frame checksum costs 4 bytes per frame](#the-frame-checksum-costs-4-bytes-per-frame) — 0.06% of the file, collected only where a whole frame is decoded
- [x] [Reading several frames rebuilt the decoder for each](#reading-several-frames-rebuilt-the-decoder-for-each) — 4 to 18% of the time spent reading two frames or more
- [x] [A lookup by index held a whole frame](#a-lookup-by-index-held-a-whole-frame) — peak RSS followed the frame size: 36.8 MiB at 32 MiB frames
- [x] [The record boundary was reached through a box](#the-record-boundary-was-reached-through-a-box) — one indirect call per record: 1.1% of a range read

Numbers come from `docs/bench/`; the harness is in `bench/`. Instruction counts come from
callgrind instead, over a fixture the section that quotes them describes.

## Existing append performance after adding the trusted-count option

Status on 2026-09-22: **a wall-clock regression remains possible; it has not been
established or ruled out.** The `#[inline(always)]` attributes on
`append_records_with_opts` and `append_records_with_finder_opts` are accepted as
the current mitigation, not proof that elapsed-time performance is unchanged.

Commit `9da297b` added the option to trust a supplied record count. Even callers
that keep counting frame 0 now pass through the option-taking wrappers. Inlining
lets those calls specialize on the constant `false` argument.

Callgrind measured the existing counting path in `append/records` and the four
`append_copy/{direct,replace}/{15125,981023}` cases. Compared with `4ab4cfc`, the
option without forced inlining added 116–118 instructions per operation. With
both inline attributes, the totals were 200 instructions lower for records and
1,187–1,189 lower for the other cases. Three repetitions agreed exactly. These
are user-space instruction counts, not elapsed time; the custom-finder path was
not covered by these five cases.

Repeated Criterion runs of both versions had large run-to-run variation and
multimodal sample distributions. The latest run used 30 samples, a 1 s warmup,
a minimum 2 s measurement, and three runs per version. It did not establish a
consistent performance difference. There was no reliable elapsed-time comparison
isolating the inline attributes themselves. Binaries, fixtures and measurement
output were on local XFS, with CPU affinity set to CPU 2 and
`CARGO_TARGET_DIR=/home/xtakei/.cargo/target`.

Short CPU profiles also do not settle the question. For records, restricting the
denominator to `append_records_inner` gave similar leading shares before and
after: `validate_separator` about 40%, followed by `encode_frame` about 19–23%.
Whole-benchmark profiles include fixture copying and file destruction, so those
shares must not be mistaken for append-body costs. Other builds were running
during profiling; the profiles were used to inspect function shares, not speed.

Keep this issue open until repeatable measurements can distinguish a code
regression from variation in the benchmark and its environment. Measure the
trusted path separately; its speed cannot establish that the existing path has
not regressed.

## Reading several frames rebuilt the decoder for each

Building a `Decoder` clones the whole seek table, and `src/edit.rs` used to build one per frame it
read. Every operation there reads at least two — separator validation compares frame 0 against frame
`F-2` — so the table was rebuilt once per frame, and a fresh buffer allocated with it. `FrameReader`
holds the decoder open across the frames of one file and holds the record window they are walked
through with it, so both happen once however many frames go through it.

`benches/edit.rs` measures it as `count_frames`: read `K` frames of a 400-frame file and count the
records in each. Same machine, same build directory, before and after taken against one saved
baseline (2026-08-28, release):

| K | before | after | change |
| ---: | ---: | ---: | --- |
| 1 | 44.3 µs | 45.1 to 49.0 µs | +2 to +11% |
| 2 | 92.8 µs | 83.4 to 88.2 µs | −4 to −10% |
| 16 | 753 µs | 618 to 668 µs | −12 to −18% |
| 256 | 11.49 ms | 9.72 to 10.53 ms | −8 to −15% |

The after column is the spread of seven runs against the one saved baseline, and it is wide because
the machine is: one run in seven came out about 8% slower in **every** case at once. So no single
figure here is worth quoting, and a difference of a few percent between two runs says nothing. What
repeats is the direction and the shape — `K = 16` and `K = 256` land near −17% and −14% whenever the
machine is quiet, and `K = 1` lands above zero every time.

**Reading one frame gains nothing, and whether it costs anything is not settled.** `K = 1` comes out
above zero in every run, but every one of those runs is compared against a *saved* baseline rather
than against a run of the other code — which is the comparison the 8% swing above makes unsafe.

Run properly, the cost does not appear. `copy_range` is the operation that reads one frame and
little else, and running master's bench binary alternately with this one, three rounds each, gives:

| | master | here |
| --- | ---: | ---: |
| `copy_range/range` | 59.1 to 60.3 µs | 59.0 to 59.9 µs |
| `copy_range/whole` | 212.3 to 220.3 µs | 212.0 to 213.2 µs |

No difference on `range`, and if anything less spread on `whole`. So the `K = 1` figure is more
likely an artifact of comparing across runs than a cost the code carries. Settling it means
instruction counts under callgrind rather than wall clock; `docs/benchmark.md` says why.

**Interleave the two binaries when the answer matters.** A saved criterion baseline is convenient
and it is what the numbers above were first taken against, which is how they came out wrong.

What it buys is every read of two frames or more: the separator validation in `truncate`, `append`
and `append --input-seekable`, and `--check-input-frames`, which reads the whole copied range.

## A lookup by index held a whole frame

`RecordReader::record` decompressed the frame the index falls in into a `Vec` and kept it for the
next lookup, so what a lookup held followed the frame size. Reading it through the record window
instead holds 32 KiB whatever the frame is, and the walk is left where the record ended so the next
index in the same frame goes on from there.

Peak RSS of three lookups (`record(10)`, `record(11)`, `record(12)`) on 124 MB of input compressed
at three frame sizes, `/usr/bin/time -v`:

| frame size | before | after |
| ---: | ---: | ---: |
| 1 MiB | 4.5 MiB | 3.5 MiB |
| 16 MiB | 20.6 MiB | 4.8 MiB |
| 32 MiB | 36.8 MiB | 4.7 MiB |

What is left is the decoder's own buffers and the seek table, neither of which follows the frame
size.

`benches/read.rs` measures the time as `record`, three cases through one reader held open: an index
on its own, the next one after it, and the one before it. **The frames have to be visited out of
order.** Reading whole frames leaves the decoder exactly at the next frame's start, so stepping
through them in order hands the version that reads the most a free seek, and the measurement comes
out backwards — the whole-frame reader looked 15% faster until the case was fixed.

| | before | after |
| --- | ---: | ---: |
| `record/one` | 25.16 µs | 24.57 µs |
| `record/next` | 26.93 µs | 24.37 µs |
| `record/back` | 27.04 µs | 24.82 µs |

`record/one` is 2.4%, which is too small to read as a gain from a wall clock — it says the lookup
did not get slower, and nothing more. `next` and `back` are 9.5% and 8.2%, which the clock can
carry.

## The seek table is read in full on every call

`Decoder::new` in `RecordReader::from_file` (`src/reader.rs`), which a fresh reader builds one of per
call, reads the entire seek table before anything else happens. Each entry is 8 bytes — 4 for the compressed size, 4 for the decompressed
size, no checksum — plus 17 bytes of header and footer.

| records | frames | seek table |
| ---: | ---: | ---: |
| 1,000,000 | 1,133 | 9,081 B |
| 10,000,000 | 11,314 | 90,529 B |

Verified on the fixture: `f1m.seek.zst` is 2,732,126 bytes and its last frame ends at 2,723,045, a
difference of exactly 9,081.

The size grows with the number of frames, which grows with the file. A lookup that reads one 65 kB
frame reads 9 kB of table alongside it at a million records, and 90 kB at ten million.

## The frame table is built in full when three entries are needed

`seek_table_decomp_frames` (`src/seekzstdsep_lib.rs`) loops over every frame and returns a
`Vec<(u64, u64)>` holding all of them.

`RecordReader::records_request` uses three of those entries: `frames[0]`, `frames[frame_idx]` and
`frames[end_frame_idx]`. The rest are allocated, filled and dropped. 16 bytes per frame and a full
pass over the table, on every call.

Does not vary with `--from`. Grows with the file.

## Frame 0 is read on every call

`RecordReader::from_file` derives the records-per-frame invariant by reading frame 0 and counting
separators (`src/reader.rs`), and a caller that opens a reader per call pays it each time.

On the benchmark fixture that is 65,579 bytes decoded, on top of the 65 kB of the frame actually
wanted. Every lookup reads two frames, and one of them is always frame 0.

The count is a property of the file, not of the request, so it is re-derived per call for a value
that never changes.

Does not vary with `--from` or with the file size. The frame is streamed through the record
reader's window rather than held, so the cost is the decode, not the memory.

Holding one reader open is what removes all three costs above — see below.

## Checking the frames a read walks costs 3.4% of a range that crosses them

`RecordReader::verifying` walks the same span the unchecked read walks and judges each frame end it
passes. What the walk hands out is a run of records at a time, so a run straddles the frame end:
the records before it are counted by scanning that run's first bytes a second time. That second
scan is where the cost is, and it falls on the frames a read crosses rather than on the records it
returns.

Instruction counts under callgrind, the same binary either way, over 200,000 records of about 55
bytes at `frame_size` 65536 — 1,258 records to a frame across 159 frames. Not the fixture
`benches/read.rs` builds: its records are JSON lines around 118 bytes, so a frame holds 555 of
them and a range read of the same record count crosses fewer frame ends.

| walk | unchecked | checked | |
|---|---|---|---|
| 40 reads of 4,000 records, each crossing three or four frames | 87,096,933 | 90,046,782 | +3.4% |
| every record of the file | 144,556,713 | 155,172,502 | +7.3% |
| 4,000 lookups by index | 1,819,926,808 | 1,820,448,995 | +0.03% |

A lookup by index pays almost nothing because it stops inside a frame: there is no frame end to
report, and what it is judged on is where it stopped.

Capping the walk at the frame end instead would remove the second scan, at the price of a
comparison per record inside the walk. That is the shape of cost the finder box had, 1.1% of a
range read, and it would fall on the unchecked path too.

**The read that checks nothing is not what it was — it is faster.** Against `fd2e6a8`, the commit
before the check existed, the same 40 reads cost 22 instructions a call less and walking every
record costs 74 a record less; a lookup by index is unchanged, to the instruction. The reads are
written once and compiled per verifier, so they are generic where the earlier ones were not, and a
generic read is compiled in the crate that calls it: `records_request` inlines into `records_to`
there, and `Records::next` into `next_owned`, neither of which happened when they were compiled
here and called.

Which verifier a read has is a type rather than a value, so the frame ends a checked walk reports
are not something the unchecked one tests and skips: its watcher is a type of no size, and the
reporting is not compiled into it at all. `Judge` takes what it judges as a closure for the same
reason — the judge that judges nothing never asks the window what it walked.

## The frame checksum costs 4 bytes per frame

Every frame `compress` writes ends with a 32-bit content checksum. On a 142 MB fixture of 1,000,200
records at `--frame-size 65536` — 2,161 frames — the output is 15,320,474 bytes with it and
15,311,830 with `--no-check`, a difference of 8,644 bytes: exactly 4 per frame, 0.056% of the file.

It is not a defect and is listed here so the cost is on the record. What it buys is stated in
`docs/format.md`, and only a whole-frame decode collects it (`docs/bugs.md`).

## The record boundary was reached through a box

Where a record ends became a finder, and `RecordReader` holds one so that the public type gains no
parameter. Held only as a `Box<dyn Fn(&[u8]) -> Option<usize>>` it cost an indirect call per record,
and the walk could hoist neither the choice nor the needle's length out of its loop. Holding the
separator's own search beside the box instead, and resolving between the two before the walk starts
(`with_find!` in `src/reader.rs`), leaves the separator calling `memchr` with the length in a
register, as it did before a record had a finder at all.

Instruction counts under callgrind, not the clock. The difference is around 1%, and on this machine
the unchanged `compress/old` case swings 7% between criterion runs — the clock cannot carry it.
`Ir` for a fixed number of iterations of each operation, 0.4.1 against the branch, the fixture's
compressed bytes checked identical first so both sides do the same work.

| | boxed | resolved before the walk |
| --- | ---: | ---: |
| `RecordReader::record` | +0.65% | −0.05% |
| `records_to`, which is what `cat` runs | +1.12% | +0.015% |
| `into_records` | −9.4% | −9.2% |

`into_records` is faster for a different reason, and both columns have it: the read window grows to
hold one record, so a record never spans two runs and `next_owned` takes it in one piece instead of
building it up across them.

`callgrind_annotate` is what located this. Every other symbol matched to the instruction —
`memchr`'s `find_raw_avx2` at 39,261,269 on both sides, `__memcpy_avx_unaligned_erms` within 65 —
and only `Records::next` moved, 11,706,554 against 16,078,824. That is 4,000 lookups over about
1.4M records, so the cost was around three instructions per record: a discriminant loaded, a branch,
and the needle's length read again.
