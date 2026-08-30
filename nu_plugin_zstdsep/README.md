# nu_plugin_zstdsep

A [nushell](https://www.nushell.sh/) plugin over `.seek.zst` files written by
[seekzstdsep](https://github.com/kazu/seekzstdsep).

```text
> let h = zstdsep open events.jsonl.seek.zst
> $h.1999999.msg
```

That reads one frame out of the file, not the file. See [Cost](#cost).

## Why a command of its own

`open events.jsonl.seek.zst | from zst` cannot be lazy. `open` hands the plugin a sequential byte
stream and never tells it the path ([nushell#8030](https://github.com/nushell/nushell/issues/8030)),
and the seek table sits at the end of the file, unreachable without consuming the stream. Taking
the path as an argument is the only route to seeking, not a preference.

## Install

```sh
cargo build --release -p nu_plugin_zstdsep
plugin add target/release/nu_plugin_zstdsep    # in nushell
plugin use zstdsep
```

`cargo install nu_plugin_zstdsep` works too. The plugin's protocol version has to match the
nushell it runs under; build it against the same release.

## Commands

### `zstdsep inspect <path>`

One row per frame: where it starts, how big it is compressed and decompressed, and how many records
it holds.

```text
> zstdsep inspect events.jsonl.seek.zst
╭───┬────────────┬──────────┬───────────┬──────────────┬────────────┬─────────────┬─────────╮
│ # │ comp_start │ comp_end │ comp_size │ decomp_start │ decomp_end │ decomp_size │ records │
├───┼────────────┼──────────┼───────────┼──────────────┼────────────┼─────────────┼─────────┤
│ 0 │          0 │     7180 │    7.1 kB │            0 │      65634 │     65.6 kB │     463 │
│ 1 │       7180 │     9570 │    2.3 kB │        65634 │      85133 │     19.4 kB │     137 │
╰───┴────────────┴──────────┴───────────┴──────────────┴────────────┴─────────────┴─────────╯
```

`records` is extrapolated from frame 0 for the interior frames. `--no-fast-mode` counts every one,
which is the only way to find a frame that breaks the uniform count that indexing rests on.

### `zstdsep open <path>`

Returns a **handle**, not the data.

```text
> let h = zstdsep open events.jsonl.seek.zst
> $h.10              # its frame decoded up to the record, one record parsed
> $h.10.user.name    # the engine follows the rest of the path itself
> $h | get 10 11 12  # three calls, one frame, decoded once
> $h                 # the summary: path, separator, format, frames, records_per_frame, records
> $h.records         # a field of that summary
```

Flags: `--separator` (default a newline — the file does not record its own), `--format`, `--raw`,
`--no-partial`.

### `zstdsep save <path>`

Writes the input as records and compresses it. `--append` adds to a file that already holds some.

```text
> ls | zstdsep save listing.jsonl.seek.zst        # `to jsonl`, from the extension
> open --raw access.log | zstdsep save access.log.seek.zst   # text, as it came
> [a b c] | zstdsep save lines.seek.zst           # one record per item
> $new | zstdsep save --append listing.jsonl.seek.zst
```

Text is written unchanged; anything structured is serialised by the format the inner extension
names, which `--format` overrides and `--raw` refuses. A record the input does not end with a
separator gets one, so the file never ends mid-record.

Flags: `--append`/`-a`, `--force`/`-f` (an existing file is kept otherwise), `--separator`/`-s`,
`--format`, `--raw`/`-r`, `--insert-separator`, and the compressor's own `--frame-size`,
`--records-per-frame`, `--limit-multiplier`, `--no-check`.

`--append` is the library's `seekzstdsep append`, and inherits its two refusals: a file of fewer
than three frames, which is too short to validate the separator against, and a file that ends
mid-record, which `--insert-separator` closes first.

Appending a format whose `to` command writes a header row puts a second header in the middle of
the file — nushell's own `save --append` does the same, and there is no way to ask a `to` command
whether it writes one. Serialise those yourself and append the text:

```text
> $rows | to csv --noheaders | zstdsep save --append --raw rows.csv.seek.zst
```

### Handles and builtins

`first`, `last`, `skip`, `take`, `slice`, `length` and `where` run inside the engine and refuse a
handle:

```text
> $h | length
Error: nu::shell::only_supports_this_input_type
  x Input type not supported.
   ,-[entry #1:1:1]
 1 | $h | length
   : ^|   ^^^|^^
   :  |      `-- only list, table, binary, and nothing input data is supported
   :  `-- input type: zstdsep handle
```

The remedy is one flag:

```text
> zstdsep open events.jsonl.seek.zst --no-partial | where lvl == error
```

That reads the whole file. `first n` still stops early — the engine drops the stream, and the
plugin decompresses one frame at a time.

## Hooking `open` and `save`

`nu/zstdsep-hook.nu` shadows the builtin `open` and `save`: a `.seek.zst` path goes to the plugin,
everything else to the builtin.

```sh
nu nu_plugin_zstdsep/nu/install.nu              # link it into ~/.config/nushell/autoload/
nu nu_plugin_zstdsep/nu/install.nu --uninstall
```

```text
> ls | save listing.jsonl.seek.zst                       # zstdsep save
> open listing.jsonl.seek.zst                            # zstdsep open: a handle
> open listing.jsonl.seek.zst --no-partial | where type == file
> open Cargo.toml                                        # the builtin, untouched
```

Each command carries the union of the two signatures, and a flag that belongs to the other side is
refused rather than dropped: `open notes.txt --no-partial` and `ls | save --progress
listing.jsonl.seek.zst` are both errors. `core-open` and `core-save` are the builtins under names
that survive the shadowing, and are the only way to read a `.seek.zst` file as bytes.

Two limits. Autoload files are read when the REPL starts and never for `nu script.nu`, so a script
needs a `use .../nu/zstdsep-hook.nu *` of its own. And a handle belongs to one file, so `open
a.seek.zst b.seek.zst` is refused where the builtin would have concatenated them.

`nu nu_plugin_zstdsep/tests/run-hook.nu` runs the tests, against a plugin registered in a directory
of its own.

## Formats

The format comes from the extension inside `.seek.zst`: `events.jsonl.seek.zst` is jsonl.
`--format <name>` overrides it and `--raw` turns it off.

- **json, jsonl, ndjson** are parsed by the plugin. Records are separated before parsing, so all
  three mean the same thing here: one JSON value per record.
- **anything else** is resolved as `from <name>` in *your* scope, so `use std formats *` covers
  what std covers, a logfmt plugin covers logfmt, and a format written later needs no change here.

The second kind works for `--no-partial` and **not** for a cell path: nushell services custom value
operations with no execution context, so a plugin cannot call `FindDecl` from one. `$h.10` on a
logfmt file returns the line as a string; `--no-partial | get 10` parses it.

`zstdsep save` splits the same way and for a second reason. A plugin can only run nushell's *own*
`to` commands: `call_decl` answers one defined in nushell with "can't run custom command with
'run'", and `to jsonl` and `to ndjson` are exactly that (they live in `std formats`). So json,
jsonl and ndjson are written by the plugin — one JSON value per record, through nushell's own
conversion — and `to csv`, `to tsv`, `to yaml`, `to nuon` and the rest work because they are
builtins.

csv is a poor fit whichever way it is parsed: the header row occupies record 0 and shifts every
index, and a newline inside a quoted field breaks the separator invariant outright.

## Cost

2,000,000 JSONL records, 215.5 MB in, 10.1 MB compressed, 3125 frames of 640 records. nushell
0.114.1, release build, warm page cache, the median of repeated `timeit` runs.

| | |
| --- | ---: |
| `$h.1999999.seq` — a frame not in the cache | 380 µs |
| `$h.1999998.seq` — the same frame again | 31 µs |
| `$h \| get 1999997` — the same, through a pipeline | 31 µs |
| `--no-partial \| first 3` | 417 µs |
| `--no-partial \| length` — every record | 4.9 s |

To rebuild it:

```text
> use std formats *
> 1..2_000_000 | each {|i| {
      seq: ($i - 1)
      lvl: ([info warn error] | get ($i mod 3))
      msg: ("" | fill --width (($i mod 101) + 20) --character "x")
  } } | to jsonl | save big.jsonl
> ^seekzstdsep compress big.jsonl big.jsonl.seek.zst
```

```text
> let h = zstdsep open big.jsonl.seek.zst
> timeit { $h.1999999.seq }
```

## What is not here yet

`zstdsep slice`, `zstdsep first`, `zstdsep last` and `zstdsep len` — the range and count shapes that
need commands of their own because the builtins refuse a handle. `--no-partial` covers them at the
cost of reading the whole file.

## License

MIT ([LICENSE](../LICENSE)).
