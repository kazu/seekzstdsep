# Route `.seek.zst` paths through the `zstdsep` plugin and leave every other path to the builtin.
#
# `nu/install.nu` links this file into `~/.config/nushell/autoload/`. Autoload files are read for
# the interactive REPL only, never for `nu script.nu`, so a script that wants the same routing has
# to `use <this file> *` itself.
#
# The builtins are captured as aliases before being shadowed: calling `open` from inside the
# shadowing `open` finds the shadow, not the builtin. Keep the aliases and the definitions out of
# the file that calls them — an alias resolved in the same file as the call resolves to the shadow,
# and the body runs twice.

module zstdsep_hook {
    export alias core-open = open
    export alias core-save = save

    # What `seekzstdsep` writes. `source::inner_extension` strips the same two extensions.
    const MARKER = ".seek.zst"

    # The plugin's own defaults, repeated here because a named flag cannot be forwarded unset:
    # `--frame-size=$x` with `$x` null is a type error, and a call's flags cannot be built at
    # runtime. Passing these always is the same call as passing none of them, which `tests/hook.nu`
    # checks byte for byte. The ones with no value standing for "unset" — `--finder-arg`,
    # `--format` and `--records-per-frame` — are branched on instead.
    const FINDER = "sep"
    const SEPARATOR = "\n"
    const FRAME_SIZE = 131072
    const LIMIT_MULTIPLIER = 4

    # What `--finder-arg` is forwarded as, or null for "not at all". `sep` has a default and is
    # always forwarded; `fixed` needs one and is forwarded as given; the rest take none, and the
    # plugin refuses one, so an unset one stays unset.
    def finder-arg [finder: any, arg: any]: nothing -> any {
        if ($finder | default $FINDER) == $FINDER { $arg | default $SEPARATOR } else { $arg }
    }

    # Whether what was named belongs to the plugin, refusing a flag that went to the other side.
    #
    # `owned` is that decision, taken by the caller: `save` takes it from the one name it is given,
    # `open` from every name its globs expanded to.
    #
    # `theirs` and `mine` name the flags only the builtin and only the plugin have, mapped to their
    # values: null for an unset named flag, false for an unset switch.
    def routes [command: string, owned: bool, theirs: record, mine: record]: nothing -> bool {
        let stray = (if $owned { $theirs } else { $mine }
            | transpose name value
            | where {|flag| $flag.value != null and $flag.value != false }
            | get name)
        if ($stray | is-not-empty) {
            let flags = ($stray | each {|name| $"--($name)" } | str join ", ")
            let msg = if $owned {
                $"($flags): the builtin `($command)` takes that, `zstdsep ($command)` does not"
            } else {
                $"($flags): only a *($MARKER) file takes that"
            }
            error make { msg: $msg }
        }
        $owned
    }

    # What a glob names, or the pattern itself when it names nothing.
    #
    # A pattern that matches no file is kept as written so that the builtin and the plugin each
    # report the missing file themselves, which is what they do without the hook. A pattern `glob`
    # refuses for any reason is kept the same way: the commonest is a name that is not a pattern at
    # all (`a[b.json` is a legal file name and the builtin opens it), and whatever else `glob` may
    # refuse, the name still reaches the side that will report it.
    #
    # Sorted, because the expansion order is the order the records come out in and a directory
    # walk has none worth relying on.
    def expand [pattern: string]: nothing -> list<string> {
        let hits = (try { glob $pattern | sort } catch { [] })
        if ($hits | is-empty) { [$pattern] } else { $hits }
    }

    # Open files. `.seek.zst` paths return one `zstdsep open` handle; anything else is the builtin.
    #
    # Globs are expanded here rather than in the plugin: the routing needs the names behind a
    # pattern to decide which side it goes to, and expanding twice would be a second answer to the
    # same question.
    #
    # Quoting is what a quoted pattern carries and a nu script cannot read: `open "*.json"` names
    # the file `*.json` rather than the json files. The builtin still sees it, because it is handed
    # `...$files` as the engine built them; the expansion is only how this command decides where to
    # route. So what is lost is on the plugin's side, which is handed the expansion: a `.seek.zst`
    # file whose name holds a glob character is opened as the pattern in its name, quoted or not.
    export def open [
        ...files: glob          # the file(s) to open, globs expanded
        --raw(-r)               # open the file as raw binary
        --finder: string        # .seek.zst: record format, sep, fixed, flatbuffers or msgpack (default: sep)
        --finder-arg: string    # .seek.zst: the separator for sep (default: a newline), the length for fixed
        --format(-f): string    # .seek.zst: parse records with `from <format>` instead
        --no-partial            # .seek.zst: every record as a list stream instead of a handle
    ] {
        let names = ($files | each {|file| expand ($file | into string) } | flatten)
        let seekable = ($names | where {|name| $name | str ends-with $MARKER })
        # A mixed set has no side to go to: one call cannot both hand `--finder` to the plugin and
        # read the other files without it.
        if ($seekable | is-not-empty) and ($seekable | length) != ($names | length) {
            let others = ($names | where {|name| not ($name | str ends-with $MARKER) })
            error make {
                msg: $"*($MARKER) files and others in one `open`: ($others | str join ', ') are not ours"
            }
        }
        let mine = { finder: $finder, finder-arg: $finder_arg, format: $format, no-partial: $no_partial }
        if (routes "open" ($seekable | is-not-empty) {} $mine) {
            let finder = ($finder | default $FINDER)
            let arg = (finder-arg $finder $finder_arg)
            if $format == null and $arg == null {
                (zstdsep open ...$names --finder=$finder --raw=$raw --no-partial=$no_partial)
            } else if $format == null {
                (zstdsep open ...$names --finder=$finder --finder-arg=$arg --raw=$raw --no-partial=$no_partial)
            } else if $arg == null {
                (zstdsep open ...$names --finder=$finder --format=$format --raw=$raw --no-partial=$no_partial)
            } else {
                (zstdsep open ...$names --finder=$finder --finder-arg=$arg --format=$format --raw=$raw --no-partial=$no_partial)
            }
        } else {
            # The values as the engine built them, not the names they expanded to: that is what
            # carries the quoting, and it is what keeps this call the builtin's own.
            core-open --raw=$raw ...$files
        }
    }

    # Write the input to a file. A `.seek.zst` path compresses it; anything else is the builtin.
    #
    # Nothing runs before the `if`, and nothing before the call inside each branch: a statement
    # ahead of them collects the pipeline input, and `save` has to stream. The parentheses are what
    # make the flags on their own lines arguments rather than statements of their own.
    export def save [
        filename: path            # the file to write
        --stderr(-e): path        # the file to save stderr to, with --raw
        --raw(-r)                 # write the input as it is, serialising nothing
        --append(-a)              # add to the file instead of writing a new one
        --force(-f)               # overwrite an existing file
        --progress(-p)            # show a progress bar
        --finder: string          # .seek.zst: record format, sep, fixed, flatbuffers or msgpack (default: sep)
        --finder-arg: string      # .seek.zst: the separator for sep (default: a newline), the length for fixed
        --format: string          # .seek.zst: serialise with `to <format>` instead
        --insert-separator        # .seek.zst: with --append, close a trailing fragment first
        --frame-size: int         # .seek.zst: target size of a frame in bytes
        --records-per-frame: int  # .seek.zst: records per frame, instead of --frame-size
        --limit-multiplier: int   # .seek.zst: how much of a frame the separator search may buffer
        --no-check                # .seek.zst: leave the content checksum out of every frame
    ] {
        if (routes "save" ($filename | into string | str ends-with $MARKER)
                { stderr: $stderr, progress: $progress }
                {
                    finder: $finder
                    finder-arg: $finder_arg
                    format: $format
                    insert-separator: $insert_separator
                    frame-size: $frame_size
                    records-per-frame: $records_per_frame
                    limit-multiplier: $limit_multiplier
                    no-check: $no_check
                }) {
            if (finder-arg $finder $finder_arg) == null and $format == null and $records_per_frame == null {
                (zstdsep save $filename
                    --finder=($finder | default $FINDER)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else if (finder-arg $finder $finder_arg) == null and $format == null {
                (zstdsep save $filename
                    --finder=($finder | default $FINDER)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --records-per-frame=$records_per_frame
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else if (finder-arg $finder $finder_arg) == null and $records_per_frame == null {
                (zstdsep save $filename
                    --finder=($finder | default $FINDER)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --format=$format
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else if (finder-arg $finder $finder_arg) == null {
                (zstdsep save $filename
                    --finder=($finder | default $FINDER)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --format=$format --records-per-frame=$records_per_frame
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else if $format == null and $records_per_frame == null {
                (zstdsep save $filename
                    --finder=($finder | default $FINDER) --finder-arg=(finder-arg $finder $finder_arg)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else if $format == null {
                (zstdsep save $filename
                    --finder=($finder | default $FINDER) --finder-arg=(finder-arg $finder $finder_arg)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --records-per-frame=$records_per_frame
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else if $records_per_frame == null {
                (zstdsep save $filename
                    --finder=($finder | default $FINDER) --finder-arg=(finder-arg $finder $finder_arg)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --format=$format
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else {
                (zstdsep save $filename
                    --finder=($finder | default $FINDER) --finder-arg=(finder-arg $finder $finder_arg)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --format=$format --records-per-frame=$records_per_frame
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            }
        } else if $stderr == null {

            (core-save $filename --raw=$raw --append=$append --force=$force --progress=$progress)
        } else {
            (core-save $filename --stderr=$stderr --raw=$raw --append=$append --force=$force --progress=$progress)
        }
    }
}

use zstdsep_hook *
export use zstdsep_hook *