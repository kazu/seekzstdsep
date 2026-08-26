plugin use zstdsep
use std/assert
use ../nu/zstdsep-hook.nu *

# Tests for `nu/zstdsep-hook.nu`. Run them through `tests/run-hook.nu`, which registers the plugin
# `plugin use` above expects and puts a scratch directory in the way of every path written here.

# --- the builtin still answers for everything else ---------------------------

"hello\n" | save plain.txt
assert equal (open plain.txt) "hello\n"
assert equal (open plain.txt --raw | into string) "hello\n"

[[a, b]; [1, 2]] | save plain.json
assert equal (open plain.json) [[a, b]; [1, 2]]

"appended\n" | save --append plain.txt
assert equal (open plain.txt) "hello\nappended\n"

assert error {|| "other\n" | save plain.txt }
"other\n" | save --force plain.txt
assert equal (open plain.txt) "other\n"

# A glob still globs: the parameter has to stay a glob to survive the forward to the builtin.
assert equal (open *.json) [[a, b]; [1, 2]]

# --- .seek.zst goes to the plugin --------------------------------------------

[{a: 1}, {a: 2}, {a: 3}] | save records.jsonl.seek.zst

# The default is `zstdsep open`'s default: a handle, read one frame at a time.
assert equal (open records.jsonl.seek.zst | describe) "zstdsep handle"
assert equal (open records.jsonl.seek.zst).1.a 2

assert equal (open records.jsonl.seek.zst --no-partial | length) 3
assert equal (open records.jsonl.seek.zst --no-partial | get a) [1, 2, 3]
assert equal (open records.jsonl.seek.zst --raw --no-partial | length) 3
assert equal (open records.jsonl.seek.zst --raw --no-partial | first | describe) "string"

# --format overrides what the inner extension names.
assert equal (open records.jsonl.seek.zst --format json --no-partial | get a) [1, 2, 3]

# A separator other than a newline has to be said on both sides.
"a;b;c;" | save --raw --separator ";" split.seek.zst
assert equal (open split.seek.zst --separator ";" --raw --no-partial) [a, b, c]

# --- flags stay on their own side --------------------------------------------

assert error {|| open plain.txt --no-partial }
assert error {|| open plain.txt --separator "," }
assert error {|| open plain.txt --format json }
assert error {|| "x\n" | save --separator "," plain2.txt }
assert error {|| "x\n" | save --no-check plain2.txt }
assert error {|| [{a: 1}] | save --progress progress.jsonl.seek.zst }
assert error {|| [{a: 1}] | save --stderr err.txt stderr.jsonl.seek.zst }

# A handle is one file's, so more than one of ours is refused rather than concatenated.
assert error {|| open records.jsonl.seek.zst records.jsonl.seek.zst }

# --- the plugin's own flags reach it -----------------------------------------

[{a: 1}, {a: 2}, {a: 3}] | save --records-per-frame 1 grow.jsonl.seek.zst
assert equal (zstdsep inspect grow.jsonl.seek.zst | length) 3

[{a: 4}] | save --append --records-per-frame 1 grow.jsonl.seek.zst
assert equal (open grow.jsonl.seek.zst --no-partial | get a) [1, 2, 3, 4]

assert error {|| [{a: 5}] | save grow.jsonl.seek.zst }
[{a: 5}] | save --force --records-per-frame 1 grow.jsonl.seek.zst
assert equal (open grow.jsonl.seek.zst --no-partial | get a) [5]

# --- the defaults the hook repeats are the plugin's own ----------------------

# A named flag cannot be forwarded unset, so the hook passes --separator, --frame-size and
# --limit-multiplier on every call. That has to be the same call as passing none of them.
[{a: 1}, {a: 2}] | zstdsep save by-plugin.jsonl.seek.zst
[{a: 1}, {a: 2}] | save by-hook.jsonl.seek.zst
assert equal (^cmp --silent by-plugin.jsonl.seek.zst by-hook.jsonl.seek.zst | complete | get exit_code) 0

# A flag that does change the output still reaches the plugin.
[{a: 1}, {a: 2}] | save --no-check unchecked.jsonl.seek.zst
assert ((^cmp --silent by-hook.jsonl.seek.zst unchecked.jsonl.seek.zst | complete | get exit_code) != 0)

# --- the builtins stay reachable ---------------------------------------------

# `open --raw` on one of ours gives the records, not the file, so the alias is the way to the bytes.
assert equal (core-open --raw by-hook.jsonl.seek.zst | into binary | first 4) (0x[28 b5 2f fd])

print "hook.nu: ok"
