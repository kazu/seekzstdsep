# CLI reference

Every subcommand and what its flags mean. `seekzstdsep -h` lists the subcommands and
`seekzstdsep <subcommand> -h` lists one subcommand's options; this file is the long form.

## Compress

```sh
seekzstdsep compress events.jsonl events.jsonl.seek.zst
```

With no `OUTPUT`, the output path is `INPUT` + `.seek.zst`. With no `INPUT`, input is read from
stdin and the result goes to stdout. Useful options:

| Option | Meaning |
| --- | --- |
| `-s, --separator <S>` | Record separator (default `"\n"`) |
| `--frame-size <N>` | Target frame size in bytes (default 65536) |
| `-c, --cnt-of-separator-per-frame <N>` | Pin records per frame instead of auto-detecting |
| `-l, --limit-multiplier <N>` | How far past `--frame-size` to search for a separator (default 4) |
| `--rm` | Delete the input file after a successful conversion |
| `--no-check` | Leave the per-frame content checksum out (it is written by default) |

`--frame-size` is a target, not a hard bound — a frame ends at the next separator past it, so byte
sizes vary while the record count per frame stays fixed. Leaving the defaults alone is fine for most
input; `docs/format.md` explains when it is not.

Each frame ends with a content checksum, so the one frame a lookup decompresses is verified as it is
read. It costs 4 bytes per frame, which `docs/performances.md` measures against a real file, and
`--no-check` drops it.

## Read a record range

```sh
seekzstdsep cat events.jsonl.seek.zst --from 10000 --cnt 3
```

`--from` is a 0-based record index. `docs/bugs.md` records the current `--cnt` semantics.

## Inspect the frame layout

```sh
seekzstdsep inspect events.jsonl.seek.zst
seekzstdsep inspect events.jsonl.seek.zst --format json
```

Prints per-frame compressed and decompressed extents plus the separator count, which is the quickest
way to confirm the uniform-count invariant actually holds for a given file. By default the separator
count is measured on the first and last few frames and assumed for the rest; pass `-n,
--no-fast-mode` to count every frame.

## Truncate

Shortens the file in place to its first `--records` records, cutting on a record boundary. Picking
that number means knowing how many records the file holds: `inspect` reports the record count of
every frame, and their sum is the record count of the file.

How you add them up depends on the shell. In bash or zsh, with `jq`:

```sh
seekzstdsep inspect events.jsonl.seek.zst --format json | jq '[.[].cnt_of_sep] | add'
# => 50000
```

In nushell, with no external command:

```nu
seekzstdsep inspect events.jsonl.seek.zst --format json | from json | get cnt_of_sep | math sum
# => 50000
```

Then cut to a number you picked from that:

```sh
seekzstdsep truncate events.jsonl.seek.zst --records 10000
```

Only the frame the cut falls inside is re-encoded, and nothing before it is read or written. The
seek table is rebuilt in full, so that part is linear in the number of frames.

Destructive — clone the file first if the original matters, which `cp --reflink=auto` does in about a
millisecond where the filesystem supports it. The separator is validated against the file before
anything is written, which needs at least three frames, so very small files are refused.

## Append

```sh
seekzstdsep append events.jsonl.seek.zst more.jsonl
cat more.jsonl | seekzstdsep append events.jsonl.seek.zst
```

Adds the records to the end of the file in place. The frame a file ends with generally holds fewer
records than the rest, so appending after it would leave a short frame in the interior, where record
lookup divides by a count that no longer holds. That frame is decoded and cut again together with
the new records instead, so every frame but the last comes back holding the count the file was built
with. Nothing before it is read or written.

A file whose last byte is not the separator ends in a fragment rather than in a record, and joining
would merge that fragment with the first appended record. `append` refuses; pass
`--insert-separator` to write a separator at the join and make the fragment a record of its own.
Where one separator does not complete the record — which a separator that overlaps itself, such as
`\n\n`, can leave — that is refused too.

Destructive, and validated before anything is written, on the same terms as `truncate` above.

A compressed file cannot be handed to this, as a path or on stdin. Its bytes are frames, not
records, and appending them as records is not an error but silent corruption — a compressed stream
holds the separator's bytes by chance, and each one is counted as a record. `append` refuses a zstd
stream it was given as records; joining two compressed files is the flag below.

### Joining another seekable file

```sh
seekzstdsep append events.jsonl.seek.zst more.seek.zst --input-seekable
seekzstdsep append events.jsonl.seek.zst more.seek.zst --input-seekable --input-from 4096 --input-cnt 2048
```

`--input-seekable` copies the input's frames as compressed bytes rather than appending records.
Neither file is decompressed and nothing is re-encoded, so the cost is the size of the range rather
than of either file — which is what `cat more.seek.zst | seekzstdsep append …` would pay instead.
`--input-from` and `--input-cnt` bound the part of the input to take, in records; they serve
`--input-seekable` alone. `--insert-separator` is rejected alongside it rather than ignored, since a
byte copy writes nothing at the seam.

The frames have to fit together as they stand, so this refuses unless

- both files hold the same number of records per frame,
- the file being appended to ends at a frame boundary rather than partway through one — its last
  frame full rather than short, which `copy-range` produces and which `truncate` to a multiple of
  the records per frame leaves behind, and
- `--input-from` is the first record of a frame, with `--input-from` plus `--input-cnt` the first
  record of a frame or the end of the input.

The input's records per frame is read off its frame 0, and the frames actually copied are taken on
trust. A file whose interior holds a count of its own — which this compressor never writes, but
another might — is copied in as it is, and record lookup then divides by a count that no longer
holds. `--check-input-frames` counts every frame being copied instead and refuses that. It is not
the default because it decompresses the range, which is the cost the byte copy exists to avoid.

Frames are copied with whatever they carry, so joining a file written with per-frame checksums onto
one written without leaves a result holding both kinds. Each zstd frame records its own, and both
`zstd -d` and the seek table read such a file back correctly.

The input may end in a short frame; that frame becomes the last frame of the result. The result is
then no longer joinable in turn, and a second `--input-seekable` onto it refuses. To avoid that,
`cat` the records past the input's last frame boundary out to a plain file, `truncate` the input
there, and append that plain file after the join.

## Copy a record range

Writes the records from `--from` on into a second file, leaving the input untouched. `--cnt` bounds
the range; without it the range runs to the end of the file. `-` as the output writes to stdout.

The frames are copied as compressed bytes and only the seek table is built fresh, so the cost is the
size of the range rather than of the file. That is what the boundary rule pays for: `--from` has to
be the first record of a frame, and `--from` plus `--cnt` the first record of a frame or the end of
the file. Nothing is rounded — a position inside a frame is refused, since honouring it would mean
decoding and re-encoding, which `compress` already does.

So picking a boundary means knowing how many records a frame holds, which `inspect` reports and
every frame but the last one shares. In bash or zsh, with `jq`:

```sh
seekzstdsep inspect events.jsonl.seek.zst --format json | jq '.[0].cnt_of_sep'
# => 1709
```

In nushell, with no external command:

```nu
seekzstdsep inspect events.jsonl.seek.zst --format json | from json | first | get cnt_of_sep
# => 1709
```

Any multiple of that number is a boundary. 75 frames in:

```sh
seekzstdsep copy-range events.jsonl.seek.zst back.seek.zst --from 128175 --no-align
```

Splitting a file is this followed by `truncate`, with the boundary written once in one unit:

```sh
seekzstdsep copy-range events.jsonl.seek.zst back.seek.zst --from 128175 --no-align
seekzstdsep truncate   events.jsonl.seek.zst --records 128175
```

`--no-align` is what that first line needs: the frame a file ends with holds whatever was left over,
so a range reaching the end of the file ends in a frame with a record count of its own. The result
is a normal file — `cat` and `truncate` read it like any other — but it can no longer be joined onto
another file by copying bytes, which is what *aligned* means and what `--no-align` gives up. Without
the flag such a range is refused rather than silently shortened.

The separator is checked against the file before anything is written, but more cheaply than
`truncate` does it. A frame ends immediately after the separator it was cut with, so a candidate
that does not end frame 0 is not that separator — one frame decompressed answers both that and how
many records a frame holds. `truncate` instead compares two frames, which costs a second one and
needs the file to have at least three; `copy-range` needs two.

What one frame cannot see is a record count that drifts somewhere later in the file, which no
compressor here produces but another writer might. `--check-uniform` counts the last frame that is
not allowed to be short as well and refuses when the two differ, at the price of that frame
decompressed.
