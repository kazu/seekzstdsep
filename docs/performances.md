# Performance

Costs that are known and still present. Tick an item when it is gone.

Ordered by how much each one costs, worst first.

- [ ] [Frame 0 is decompressed on every call](#frame-0-is-decompressed-on-every-call) — doubles the decompression per lookup
- [ ] [The seek table is read in full on every call](#the-seek-table-is-read-in-full-on-every-call) — grows with the file: 9 kB at a million records, 90 kB at ten million
- [ ] [The frame table is built in full when three entries are needed](#the-frame-table-is-built-in-full-when-three-entries-are-needed) — also grows with the file, but only an allocation and a pass
- [ ] [The frame checksum costs 4 bytes per frame](#the-frame-checksum-costs-4-bytes-per-frame) — 0.06% of the file, paid for corruption detection

Numbers come from `docs/bench/`; the harness is in `bench/`.

## The seek table is read in full on every call

`Decoder::new` in `RecordReader::from_file` (`src/reader.rs:62`), which `cat_data` builds one of per
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

`seek_table_decomp_frames` (`src/seekzstdsep_lib.rs:991`) loops over every frame and returns a
`Vec<(u64, u64)>` holding all of them.

`cat_data` uses three of those entries: `frames[0]`, `frames[frame_idx]` and
`frames[end_frame_idx]`. The rest are allocated, filled and dropped. 16 bytes per frame and a full
pass over the table, on every call.

Does not vary with `--from`. Grows with the file.

## Frame 0 is decompressed on every call

`RecordReader::from_file` derives the records-per-frame invariant by decompressing frame 0 and
counting separators (`src/reader.rs:74-75`), and `cat_data` builds a reader per call.

On the benchmark fixture that is 65,579 bytes of decompression, on top of the 65 kB of the frame
actually wanted. Every lookup decompresses two frames, and one of them is always frame 0.

The count is a property of the file, not of the request, so it is re-derived per call for a value
that never changes.

Does not vary with `--from` or with the file size.

The reader keeps that frame, so `RecordReader::record` reads it from the cache. `RecordReader::records`
does not: it goes through `lines_between_by_separator_in_frame`, which decompresses the range
itself, so frame 0 is still decompressed twice on the `cat_data` path. Holding one reader open is
what removes all three costs above — see below.

## The frame checksum costs 4 bytes per frame

Every frame `compress` writes ends with a 32-bit content checksum. On a 142 MB fixture of 1,000,200
records at `--frame-size 65536` — 2,161 frames — the output is 15,320,474 bytes with it and
15,311,830 with `--no-check`, a difference of 8,644 bytes: exactly 4 per frame, 0.056% of the file.

It is not a defect and is listed here so the cost is on the record. What it buys is stated in
`docs/format.md`: the frame a lookup decompresses is verified as it is read.
