# The benchmark harness

What is measured, and how. `docs/benchmark.md` decides what it is measured against.

The harness lives in `bench/`, a separate crate with its own `[workspace]` table so that adding it
changed nothing in the crate under test. It depends on `seekzstdsep` by path, and `cargo package`
skips it automatically.

## What is measured

One measurement is: **read `cnt` records starting at record `from`, from a file.** Three ways of
doing that are compared:

| Engine | Command |
| --- | --- |
| `uncompressed+shell` | `tail -n +N file.jsonl \| head -n C` |
| `seekzstdsep` | `seekzstdsep cat --from N --cnt C file.seek.zst` |
| `zstd+shell` | `zstd -dcq --no-asyncio file.jsonl.zst \| tail -n +N \| head -n C` |

`from` and `cnt` are a **matrix**, not two separate sweeps: every position is measured at every
record count. Sweeping one axis with the other pinned cannot tell a cost that depends only on
position from one that depends on both, and it misses whatever only happens in a corner. The two
corner failures in `findings-2026-08-23.md` were found in cells that no single sweep visits.

Defaults: `from` at 0 / 10 % / 25 % / 50 % / 75 % / 90 % / 99.9 % of the records, `cnt` at
1 / 10 / 100. 21 cells x 3 engines.

Plus a compression benchmark — `seekzstdsep compress` against `zstd -3` — so that a change to the
read path is not paid for on the write side.

Four metrics on every cell:

| Metric | Why |
| --- | --- |
| Elapsed time | the headline |
| Peak RSS | catches oversized buffers that time alone hides |
| CPU time | uncompressed is I/O bound, compressed is CPU bound; elapsed time conflates them |
| Bytes read | storage and transfer cost; the term that dominates cold or over a network |

## Running it

```sh
cd bench && cargo build --release

# Fixtures are generated once per (storage, record count) and reused.
szbench fixture --storage local --records 1000000

# One result file per set of conditions.
szbench run --storage local --cache warm --reps 10 \
    --label local/warm --out docs/bench/raw/local-warm.json

# Result files taken under the same conditions are merged into one section.
szbench report --in docs/bench/raw/local-warm.json,docs/bench/raw/nfs-cold.json \
    --out docs/bench/baseline.md --title "..."
```

`--positions` and `--cnts` change the matrix axes. `--suite matrix` or `--suite compress` runs only
one half. Storage presets: `local` (`$HOME/.cache`), `tmp` (`/tmp`), `nfs` (`$SZBENCH_NFS_DIR`,
which has no default and must be set); `--cache-dir` overrides. The `seekzstdsep` binary is looked
for next to `szbench`, which is where it lands when `CARGO_TARGET_DIR` is shared; `--bin-dir`
overrides.

## No engine gets more of the machine than another

Every process is pinned to a single CPU (`--pin-cpu`, default 3, via `sched_setaffinity` between
`fork` and `exec`), and `zstd` is invoked with `--no-asyncio`.

Without this a two- or three-process pipeline overlaps its stages, and zstd 1.5.4+ reads its input
on a second thread, so those baselines finish in less elapsed time than their own CPU time. Against
a single-threaded engine that is not a comparison of work, it is a comparison of hardware. Pinned,
`cpu / wall` is 0.99–1.00 for all three engines, and elapsed time means the same thing in every
column. Pass a negative `--pin-cpu` to leave scheduling alone.

## Rounds, not repeats

A **round** measures every cell once. `--reps` rounds are recorded, preceded by `--warmups`
discarded rounds (forced to zero when cold), and **the order reverses on every round**.

Measuring one cell N times in a row measures how warm that one cell got, and hands each engine its
own stretch of wall clock, so any drift in the machine lands on whichever engine happened to be
running then. A round keeps the engines adjacent in time and sharing the same conditions;
alternating the direction removes the remaining advantage of always being measured first.

Around the rounds sit two untimed passes:

- **verification** — one run per cell with stdout captured, so the records each engine actually
  returns are recorded rather than assumed. This is how the `records returned` and `bytes returned`
  columns caught `cat --cnt C` returning C+1 records, and caught 74 MB of NUL padding. Each
  engine's output is then compared against `uncompressed+shell`; a mismatch is reported per cell,
  so a timing cannot come from an engine that returned the wrong records.
- **bytes read** — one `strace` run per cell. Separate because `strace` destroys the timing.

Each case is a pipeline of one to three processes, built by the harness with `fork`/`execvp` and
real pipes rather than handed to a shell, so every stage is a direct child and individually
measurable.

## Aggregating a pipeline

`zstd -dc | tail | head` is three processes. One rule, applied to every engine including the
single-process one:

| Metric | Rule | Source |
| --- | --- | --- |
| Elapsed time | first `fork` to last child reaped, all stages on one CPU | `Instant` |
| CPU time | **summed** over stages | `ru_utime + ru_stime` from `wait4` |
| Peak RSS | **maximum** over stages | `ru_maxrss` from `wait4` |
| Bytes read, block layer | **summed** over stages | `ru_inblock × 512` |
| Bytes read, from the file | **summed** over stages | `strace`, separate pass |

Peak RSS is a maximum and not a sum because no kernel interface reports a true simultaneous sum for
a process group without cgroup delegation. The same rule everywhere is what the comparison needs;
it is not a claim about total memory in flight.

A stage that dies of `SIGPIPE` is not an error — that is how `zstd -dc | tail | head` stops early.
Only the last stage has to exit cleanly.

### Bytes read from the file

`strace -f -y` on each stage, summing successful `read`/`pread64`/`readv`/`preadv` returns on file
descriptors whose path annotation names the fixture. Reads from pipes are excluded, which is what
makes the number comparable between a single process and a pipeline.

`-f` is not optional. zstd 1.5.4 and later read their input on a separate asyncio thread, so
without it `zstd -dc` reports zero bytes read — a plausible-looking wrong answer. Following threads
makes the log interleave, so the parser rejoins `<unfinished ...>` with `<... read resumed>`.

Anything read through `mmap` stays invisible. Nothing under test maps its input.

### Cold cache

`--cache cold` evicts the fixtures with `posix_fadvise(POSIX_FADV_DONTNEED)` before every run. This
is per-file eviction, not a global `drop_caches`, so it does not disturb the rest of the machine.
`--cold-method drop-caches` does the global version, and needs root.

That the eviction worked is not assumed: block-layer bytes read are recorded, and they match the
bytes read from the file on cold runs and are zero on warm ones. This holds on NFS as well as XFS.

## The fixture

`docs/benchmark.md` pins the fixture by its sizes and never by its content, so the generator is a
reconstruction:

```
{"id":"0000000000","ts":"2026-08-23T00:00:00Z","lvl":"info","msg":"okay"}
```

`msg` cycles through 16 words of total length 67, so a record averages 74.1875 bytes and
1,000,000 of them come to 74.19 MB — the specified 74.2 MB. `id` is zero padded and ascending.

Three files per record count, cached outside the repository:

| File | Read by |
| --- | --- |
| `f1m.jsonl` | `uncompressed+shell` |
| `f1m.jsonl.zst` | `zstd+shell` |
| `f1m.seek.zst` | `seekzstdsep` |

The seekzstdsep fixture is built through the library, not the CLI, so that the duplicated
`convert_to_seekable_zst_reader` call in `src/main.rs` cannot enter the fixture.

## Known limits

- Peak RSS is the maximum over a pipeline's stages, not their simultaneous sum.
- Peak RSS reports pages touched, not pages allocated. A large lazily-zeroed allocation that is
  never written does not appear in it; `strace -e trace=mmap` is what distinguishes the two, and
  the distinction turned out to matter — see `findings-2026-08-23.md`.
- Bytes read misses `mmap`ed input.
- `--cache cold` evicts the fixtures only. Everything else on the machine stays cached.
- Elapsed time includes `fork` and `exec`, which is about 0.5 ms of every measurement. That is the
  floor for anything measured through a binary.
