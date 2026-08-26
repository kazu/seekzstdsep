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
    # checks byte for byte. The two with no value standing for "unset" — `--format` and
    # `--records-per-frame` — are branched on instead.
    const SEPARATOR = "\n"
    const FRAME_SIZE = 65536
    const LIMIT_MULTIPLIER = 4

    # Whether `path` belongs to the plugin, refusing a flag that went to the other side.
    #
    # `theirs` and `mine` name the flags only the builtin and only the plugin have, mapped to their
    # values: null for an unset named flag, false for an unset switch.
    def routes [command: string, path: string, theirs: record, mine: record]: nothing -> bool {
        let owned = ($path | str ends-with $MARKER)
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

    # Open a file. A `.seek.zst` path returns a `zstdsep open` handle; anything else is the builtin.
    export def open [
        ...files: glob          # the file(s) to open
        --raw(-r)               # open the file as raw binary
        --separator(-s): string # .seek.zst: the separator records end with (default: a newline)
        --format(-f): string    # .seek.zst: parse records with `from <format>` instead
        --no-partial            # .seek.zst: every record as a list stream instead of a handle
    ] {
        let names = ($files | each {|file| $file | into string })
        let seekable = ($names | where {|name| $name | str ends-with $MARKER })
        if ($seekable | is-not-empty) and ($names | length) > 1 {
            error make {
                msg: $"one *($MARKER) file at a time: ($names | length) were named, and a handle is one file's"
            }
        }
        let mine = { separator: $separator, format: $format, no-partial: $no_partial }
        if (routes "open" ($seekable | append "" | first) {} $mine) {
            let path = ($seekable | first)
            let sep = ($separator | default $SEPARATOR)
            if $format == null {
                (zstdsep open $path --separator=$sep --raw=$raw --no-partial=$no_partial)
            } else {
                (zstdsep open $path --separator=$sep --format=$format --raw=$raw --no-partial=$no_partial)
            }
        } else {
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
        --separator(-s): string   # .seek.zst: the separator to end records with
        --format: string          # .seek.zst: serialise with `to <format>` instead
        --insert-separator        # .seek.zst: with --append, close a trailing fragment first
        --frame-size: int         # .seek.zst: target size of a frame in bytes
        --records-per-frame: int  # .seek.zst: records per frame, instead of --frame-size
        --limit-multiplier: int   # .seek.zst: how much of a frame the separator search may buffer
        --no-check                # .seek.zst: leave the content checksum out of every frame
    ] {
        if (routes "save" $filename
                { stderr: $stderr, progress: $progress }
                {
                    separator: $separator
                    format: $format
                    insert-separator: $insert_separator
                    frame-size: $frame_size
                    records-per-frame: $records_per_frame
                    limit-multiplier: $limit_multiplier
                    no-check: $no_check
                }) {
            if $format == null and $records_per_frame == null {
                (zstdsep save $filename
                    --separator=($separator | default $SEPARATOR)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else if $format == null {
                (zstdsep save $filename
                    --separator=($separator | default $SEPARATOR)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --records-per-frame=$records_per_frame
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else if $records_per_frame == null {
                (zstdsep save $filename
                    --separator=($separator | default $SEPARATOR)
                    --frame-size=($frame_size | default $FRAME_SIZE)
                    --limit-multiplier=($limit_multiplier | default $LIMIT_MULTIPLIER)
                    --format=$format
                    --append=$append --force=$force --raw=$raw
                    --insert-separator=$insert_separator --no-check=$no_check)
            } else {
                (zstdsep save $filename
                    --separator=($separator | default $SEPARATOR)
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
