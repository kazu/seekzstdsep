# Todos

Decisions to make and work to build. Neither defects (`docs/bugs.md`) nor costs
(`docs/performances.md`). Ordered by what blocks what.

## Editing an existing file

Designed in `docs/design/2026-08-24-truncate-append-split-concat.md`. `split` and `concat` were
designed there and dropped; the doc records why. Implementation order is undecided.

- [x] `truncate`
- [x] `append`
- [ ] `compress --align`
- [ ] `copy-range`
- [ ] `append --input-seekable`

## Later

- [ ] [Concurrent append, and reading during an append](#concurrent-append-and-reading-during-an-append)
- [ ] [Separate metadata from lookup](#separate-metadata-from-lookup)

## Blocking nothing

- [ ] [`inspect_with_opts` is not re-exported](#inspect_with_opts-is-not-re-exported)
- [ ] [`out_dir` is written out at every call site](#out_dir-is-written-out-at-every-call-site)

### Concurrent append, and reading during an append

`append` is a read-modify-write of the tail: read the seek table, decode the last data frame,
`set_len` it away, write the replacement, write a new table. Two writers doing that at once corrupt
each other, and no choice of parameter type prevents it — the seek table sits at the end and the
last frame is partial, so appending is a rebuild of the tail rather than a write after it.

Exclusive access is therefore required: a mutex within a process, `flock` across processes. `&mut
File` states that requirement even though the borrow checker cannot enforce it across handles.

Writing concurrently scales by giving each writer its own file and merging later, which is what
segmented logs do. Merging without re-compressing needs every file aligned — `compress --align`
is what provides that — and needs each writer's remainder carried into its next batch rather than
left at the end of its own file, or it lands out of order in the merged result.

Undecided: what a reader sees while an append runs. Frames before the last one do not move, so
already-written records stay readable, but a reader that opens between the `set_len` and the table
write finds no seek table, and one reading near the tail sees the frame replaced underneath it.

### Separate metadata from lookup

`cat_data` opens the file, reads the whole seek table, and decompresses frame 0 to count separators
on every call. Two of those are already recorded in `docs/performances.md`.

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
