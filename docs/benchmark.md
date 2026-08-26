# Benchmarks

What to measure, what to measure it against, and the rules that keep the numbers valid.

## Why

seekzstdsep exists for performance. Its optimisations are invisible in the source, so a measurement
is the only thing that detects when one has been undone. `docs/format.md` records why they are
there.

## What to measure

1. **Record range extraction** — `cnt` records starting at record index `from`. The core claim.
2. **Key search over sorted data** — the target use case, and where probe cost multiplies. `rg` over
   the uncompressed file is the competitor here, not the tools in the extraction comparison.
3. **Compression** — throughput and ratio, so that a gain on the read side is not paid for on the
   write side.

### Metrics

Record all four for every case. Time alone hides the worst failures.

| Metric | Why |
| --- | --- |
| Wall time | The headline |
| Peak RSS | Catches oversized buffers |
| CPU time (user + sys) | Uncompressed is I/O bound, compressed is CPU bound |
| Bytes read | Dominates on a cold cache and over a network |

### Axes

Report curves, not single numbers.

| Axis | |
| --- | --- |
| **Record position** | The central claim. seekzstdsep must be flat; scanning baselines rise |
| File size | Cost must not grow with the file |
| `cnt` | One record versus a range spanning several frames |
| Storage | Local nvme versus NFS |

## Compare against

| Baseline | |
| --- | --- |
| Uncompressed, in-source scan | The algorithmic floor: same language, no process or pipe overhead |
| Uncompressed, `tail -n +N \| head` | What a person actually types |
| Plain zstd, `zstd -dc` into the same | The obvious thing to do with a compressed file |
| Plain seekable zstd | Needs a small `zeekstd` program; the stock `zstd` binary has no seekable mode |
| seekzstdsep, before and after each fix | Regression protection, and proof a fix worked |

## Rules

- **Input is a file.** Never stdin, never a pipe into the tool under test. A stream cannot seek, so
  it exercises a different path and answers a different question.
- **Use `tail -n +N | head` as the uncompressed baseline.** It is the fastest shell approach. Never
  `sed -n 'N,Mp'` without a quit, which reads to end of file. Do not benchmark shell variants
  against each other — which shell command is fastest is not the question being asked.
- **Record the storage location with every result,** and never compare results taken on different
  storage. A mount's name does not tell you its filesystem; `szbench` reads the type with
  `statfs` and records it alongside the result.
- **Label every result cold or warm.** Both are real. Do not build fixtures larger than RAM to force
  a cold read — at 78 GiB that is not a workable fixture size.
- **Sum peak RSS and CPU time across pipelines,** or avoid pipes entirely. A pipeline is two
  processes. Apply the same choice to every baseline or the comparison is meaningless.
- **Fixture size is a parameter.** Default to the smallest size that separates O(position) from
  O(1); one million records already shows a tenfold difference. Generate outside the repository and
  reuse.
- **Measure anything under a millisecond in process.** Invoking the CLI costs about that before any
  work happens.

## The result that matters

Scanning is O(position) with a very small constant; seekzstdsep is O(1) with a larger one, so
scanning wins near the start of a file and should. **Report where the two curves cross.** That is
what answers whether the approach is worth using; a ratio at one arbitrary offset is not.

## Recorded baseline, before any fix

Release binary, 2026-08-23. Fixture: 1,000,000 JSONL records, newline separator, default frame size
— 74.2 MB uncompressed, 3.0 MB compressed, 1096 frames. Warm cache, local XFS on nvme, best of
three, `cat --cnt 3` at each position.

| `--from` | Time |
| --- | --- |
| 0 | 0.9 ms |
| 250,000 | 14.6 ms |
| **500,000** | **28.8 ms** |
| 750,000 | 15.3 ms |
| 999,000 | 1.1 ms |

Fast at both ends, slowest in the middle: the shape of `min(absolute end offset, bytes remaining to
EOF)`. `cat_data` passes `frames[end].0 + frames[end].1`, an absolute end offset, into the
`frame_len` parameter, which sizes both the buffer and the read. Cost scales with the file instead
of being constant.

`tail -n +500000 | head -n 3` on the uncompressed fixture takes 9.7 ms under the same conditions.
**At mid-file the crate is currently about three times slower than scanning the uncompressed file**,
while returning 312 bytes.

A new harness should reproduce this shape. If it does not, suspect the harness first.

The allocation size is inferred from the code, not observed: at mid-file the buffer should be tens
of megabytes. Peak RSS is on the metric list partly to confirm or refute that.

## Excluded

- **bgzip + tabix, Parquet, DuckDB, SQLite.** Each needs a second file or a different container.
  Different products, not different implementations of the same thing.
- **A record-count index in a skippable frame.** An extension of the current design rather than an
  alternative to it; belongs in a design note.
