# Performance

Costs that are known and still present. Tick an item when it is gone.

Ordered by how much each one costs, worst first.

- [ ] [Frame 0 is read on every call](#frame-0-is-read-on-every-call) — doubles the decoding per lookup
- [ ] [The seek table is read in full on every call](#the-seek-table-is-read-in-full-on-every-call) — grows with the file: 9 kB at a million records, 90 kB at ten million
- [ ] [The frame table is built in full when three entries are needed](#the-frame-table-is-built-in-full-when-three-entries-are-needed) — also grows with the file, but only an allocation and a pass
- [ ] [The frame checksum costs 4 bytes per frame](#the-frame-checksum-costs-4-bytes-per-frame) — 0.06% of the file, paid for corruption detection
- [x] [Reading several frames rebuilt the decoder for each](#reading-several-frames-rebuilt-the-decoder-for-each) — 4 to 18% of the time spent reading two frames or more

Numbers come from `docs/bench/`; the harness is in `bench/`.

## Reading several frames rebuilt the decoder for each

Building a `Decoder` clones the whole seek table, and `src/edit.rs` used to build one per frame it
read. Every operation there reads at least two — separator validation compares frame 0 against frame
`F-2` — so the table was rebuilt once per frame, and a fresh buffer allocated with it. `FrameReader`
holds the decoder open across the frames of one file and holds the buffer with it, so both happen
once however many frames go through it.

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

## The frame checksum costs 4 bytes per frame

Every frame `compress` writes ends with a 32-bit content checksum. On a 142 MB fixture of 1,000,200
records at `--frame-size 65536` — 2,161 frames — the output is 15,320,474 bytes with it and
15,311,830 with `--no-check`, a difference of 8,644 bytes: exactly 4 per frame, 0.056% of the file.

It is not a defect and is listed here so the cost is on the record. What it buys is stated in
`docs/format.md`: the frame a lookup decompresses is verified as it is read.
