# zstdsep: a nushell plugin

**Status:** implemented in `nu_plugin_zstdsep/`, except the range and count subcommands
(`slice`, `first`, `last`, `len`). Two claims below turned out to be wrong and are corrected in
place — see "Formats".
**Date:** 2026-08-24, with `save` added 2026-08-25

Lazy access to `.seek.zst` files from nushell: `zstdsep open f | get 10` decompresses one frame,
not the file. Built on the facts below about what nushell delegates to plugins; each was verified
against the protocol reference and nushell source, re-check when nushell moves.

## Why not integrating with `open`

`open f.seek.zst | from zst` cannot work lazily, structurally:

- `open` opens the file itself and pipes a **sequential byte stream** into `from zst`. The plugin
  never learns the path: plugin stream headers carry only id/span/type, no metadata
  ([nushell#8030](https://github.com/nushell/nushell/issues/8030), open since 2023).
- The seek table sits in a skippable frame at the end, unreachable without consuming the stream.
- Without the path the plugin cannot reopen the file, so it cannot even return a handle.

An own `zstdsep open <path>` command is therefore the only route to seekability, not a preference.

## What the engine delegates to a custom value

Exactly seven ops ([`CustomValueOp`](https://docs.rs/nu-plugin-protocol/latest/nu_plugin_protocol/enum.CustomValueOp.html)):
ToBaseValue, FollowPathInt, FollowPathString, PartialCmp, Operation, Save, Dropped. These are the
operations that are language semantics rather than commands — everything a plugin could not provide
as a command of its own.

- **Cell paths are delegated.** `get 10` / `$h.10` → `FollowPathInt`, one call per index, no
  batching (`get 10 11 12` is three calls, each carrying the serialized custom value back).
  Ranges cannot appear in cell paths; `get ...(seq 100 110)` spreads to N calls.
- **A delegated op cannot call back into the engine.** `nu-plugin-engine` sends every custom value
  op with `context: None` (`custom_value_op_expecting_value`), and every engine call requires one:
  `FindDecl`, `CallDecl`, `get_current_dir`, `get_config` all fail with "attempted to call FindDecl
  outside of a command invocation". So a cell path has the plugin's own state and nothing else.
- **List commands are not.** `last`, `first`, `skip`, `take`, `slice`, `length`, `where` run inside
  the engine and fail on a custom value with `OnlySupportsThisInputType` — no ToBaseValue fallback.
  The SQLite pushdown inside `last`/`first` is an in-process downcast to `QueryPlan`; plugin custom
  values are opaque `PluginCustomValue` bytes and cannot take that path.
- Adding ops upstream (e.g. `Slice(range)`) was declared trivial by the protocol rework
  ([PR #11911](https://github.com/nushell/nushell/pull/11911)); a proposal is future work, not this
  design.

## Commands

- `zstdsep inspect <path>` → table, one row per frame: the `InspectResult` fields (already
  `Serialize`) plus records per frame, sizes as filesize values. No custom value machinery —
  implement this first, as the walking skeleton of the plugin.
- `zstdsep open <path> [--separator <sep>]` → handle custom value holding `{handle id, path,
  separator}`. The file does not record its separator, so the flag is required knowledge here,
  default `\n`.
- `zstdsep open --no-partial` → a plain list stream of records instead of a handle. Every builtin
  works on it at full-read cost; `first n` still stops early because the engine drops the stream.
  ~~`zstdsep cat` (handle in, stream out) is the same conversion chosen at pipe time.~~ *Dropped: it
  adds no capability over the flag — it reopens the file rather than using the handle's reader — and
  `cat` already names a record range in the CLI. `zstdsep slice` is the lazy version worth having.*
- `zstdsep save <path>` → the inverse of `open`, `seekzstdsep compress` with `--append` for
  `seekzstdsep append`. Text is written as it came; anything structured is serialised by the
  format the inner extension names. An existing file is kept unless `--force` says otherwise, and
  a record the input leaves unterminated is terminated, so the file never ends in the fragment
  that `--append` refuses. The compressor's knobs (`--frame-size`, `--records-per-frame`,
  `--limit-multiplier`, `--no-check`) are passed through.
  `--append` on a format whose `to` command writes a header row writes a second header into the
  middle of the file. nushell's own `save --append` does the same — `convert_to_extension` builds
  a bare call and is not even told that it is appending — and nothing can ask a `to` command
  whether it writes a header, so refusing would mean guessing at a set of format names. This
  matches `save`; the way around it is `to csv --noheaders | zstdsep save --append --raw`. It is
  the same header that makes a cell path into a csv file point one record past where it reads:
  csv's first record is not data, and nothing else in the design has that shape.
- `FollowPathInt(i)`: locate the frame as `cat_data` does (divide by the per-frame separator count —
  inherits the same-count-per-frame invariant and its silent-wrong-answer failure mode), decompress
  that frame only, return record `i`.
- Range and count shapes need own subcommands, because the builtins error: `zstdsep slice <range>`,
  `zstdsep last <n>`, `zstdsep first <n>`, `zstdsep len`.
- `ToBaseValue` returns a summary record (path, frame count, records per frame, total records), not
  the data. Displaying `$h` triggers this op; materializing the whole file on display is a footgun.
  Full output stays explicit via `--no-partial` or the CLI.
- `FollowPathString` returns a field of that same summary, so `$h.records` reaches it without
  displaying the handle. Indices address the file, names address the handle.
- `notify_plugin_on_drop = true`; on `Dropped`, remove the state entry.

No pipeline dead-ends: a builtin list command on the handle fails engine-side, and the remedy is
always one flag (`--no-partial`). The engine's error text
cannot be customized, but it prints the custom value's `type_name()` — name it `zstdsep handle` so
the error identifies itself, and document the remedy in `zstdsep open --help` and the README.

## Formats

The payload format is inferred from the inner extension (`events.jsonl.seek.zst` → `jsonl`),
overridden by `--format <fmt>`, disabled by `--raw`.

**The plugin cannot be ignorant of formats everywhere.** The plan was to resolve `from <fmt>` in
the user's scope through `FindDecl`/`CallDecl` and never know a format — but a delegated op has no
engine to call (above), and `FollowPathInt` is a delegated op. A cell path can only parse in
process or hand back a string. So there are two routes, and which one applies is a property of the
caller, not of the format:

- **json, jsonl, ndjson: parsed in the plugin** (`src/json.rs`), in both routes. Records are
  separated before parsing, so the three names mean one thing here: one JSON value per record.
  This is what makes `$h.10.user.name` work — the engine follows the rest of the path itself, and
  it can only do that into a value it understands.
- **Everything else: `from <fmt>` in the user's scope**, so `use std formats *` covers what std
  covers and a logfmt plugin covers logfmt. Commands only: `--no-partial` is a command invocation
  and can call `CallDecl` with the whole stream as input. A cell path on such a
  file returns the record as a string.
- csv is a poor fit: the header row occupies record 0 (shifting indices), and newlines inside
  quoted fields break the separator invariant outright. Support it only for records known to be
  newline-free; jsonl and logfmt guarantee one-record-per-line structurally.

**Writing splits the same way, for a second reason.** `save` is a command invocation, so the
argument above does not reach it — but `EngineInterface::call_decl` can only run a *builtin*
declaration. Handed one defined in nushell it fails with "can't run custom command with 'run', use
block_id", and `to jsonl` and `to ndjson` are defined in nushell (`std formats`). The very case the
command exists for, `zstdsep save events.jsonl.seek.zst`, is therefore unreachable through the
user's scope.

So json, jsonl and ndjson are serialised in the plugin as well, through `nu-json`'s own `Value`
conversion — the one `to json` uses, so a record written here and one written by `to json --raw`
differ in nothing. `to csv`, `to tsv`, `to yaml`, `to nuon` and the rest are builtins and are
called as planned. Reading and writing then agree on what a `.json` file is: one JSON value per
record, not one array.

## Process and state model

- The plugin process is spawned once and persists; the engine stops it after 10 s idle by default
  (timer resets on every call) and restarts it on demand. Custom values live engine-side and
  survive both.
- State table: handle id → `{seek table, sep_cnt, one decompressed-frame cache}`. The frame cache
  turns `get ...(seq a b)` (N calls, same frame) into one decompress + N lookups.
- A `FollowPathInt` that misses the table (post-GC restart) rebuilds it from the path and separator
  embedded in the custom value. `set_gc_disabled` is an optional optimization on top, never a
  correctness requirement.

## Library prerequisites

- Extract a handle type from `cat_data`. Today it reopens the file, re-reads the seek table, and
  decompresses frame 0 for `sep_cnt` on **every call** (`src/seekzstdsep_lib.rs`); per-point calls
  through it would defeat the seek. The handle holds decoder + frame list + `sep_cnt`;
  `cat_data` becomes a thin wrapper over it.
  *Done: `RecordReader` in `src/reader.rs`. It also carries the one-frame cache, and `into_records`
  / `into_bytes` are what `--no-partial` streams through.*
- A multi-start batch API (sort starts by frame, decompress each frame once) serves the own
  subcommands and the CLI. It does not help `FollowPathInt`, which arrives one index per call; the
  frame cache covers that side. *Not done; it belongs with the subcommands that need it.*

## References

- [Plugin protocol reference](https://www.nushell.sh/contributor-book/plugin_protocol_reference.html)
- [`CustomValue` trait](https://docs.rs/nu-protocol/latest/nu_protocol/trait.CustomValue.html) —
  `follow_path_int` takes a single index
- [PR #11911](https://github.com/nushell/nushell/pull/11911) — persistent plugins, op extensibility
- [nushell#8030](https://github.com/nushell/nushell/issues/8030) — pipeline metadata for plugins,
  unresolved
- `nu_plugin_polars` — precedent for the handle-id + process-cache + own-subcommands shape
