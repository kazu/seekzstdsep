# Fold a frame-size sweep of szbench results into the chart that picks the default frame size.
#
# `sweep-load DIR` reads every `fs*-l*.json` in DIR into one table with a row per
# (frame size, level): sizes, mean single-record read time over the positions of the matrix,
# compression time, and the same for the plain zstd baselines. `sweep-chart` draws one level of
# it as two panels sharing the frame-size axis: read time on top, compressed size below.
#
# nushell, from the repository root, after one `szbench run --frame-size N --level L --out
# DIR/fsN-lL.json` per point:
#   use bench/sweep.nu *
#   sweep-load DIR | sweep-chart --level 3 | save -f docs/bench/frame-size.svg

use plot.nu [svg-lines svg-stack]

def fmt-mb [bytes: int] {
    $"($bytes / 1000000 | math round --precision 2) MB"
}

export def sweep-load [dir: path] {
    ls $dir | get name | where { |f| ($f | path basename) =~ "^fs[0-9]+-l[0-9]+\\.json$" } | each { |f|
        let d = (open $f)
        let fx = $d.fixture
        let one = ($d.rows | where suite == matrix and cnt == 1)
        let comp = ($d.rows | where suite == compress)
        {
            frame_size: $fx.frame_size
            level: $fx.zstd_level
            records_per_frame: $fx.seek_records_per_frame
            raw_bytes: $fx.raw_bytes
            seek_bytes: $fx.seek_bytes
            zstd_bytes: $fx.zstd_bytes
            seek_read_ms: ($one | where engine == seekzstdsep | get wall_ms_med | math avg)
            zstd_read_ms: ($one | where engine == "zstd+shell" | get wall_ms_med | math avg)
            raw_read_ms: ($one | where engine == "uncompressed+shell" | get wall_ms_med | math avg)
            seek_compress_ms: ($comp | where engine == seekzstdsep | get wall_ms_med | first)
            zstd_compress_ms: ($comp | where engine == zstd | get wall_ms_med | first)
        }
    } | sort-by level frame_size
}

# The points that get a value printed next to them; the rest would overlap at the left edge.
const labelled = [65536 131072 1048576 2097152 4194304 8388608]
# The read panel has 64 KiB and 128 KiB within a pixel of each other, so it labels only the first.
const labelled_read = [65536 1048576 2097152 4194304 8388608]

# One panel of `sweep-chart`: `which` is read (ms per record) or size (MB), each with plain zstd at
# the same level as a flat line labelled at its right end.
export def sweep-panel [which: string, --level: int = 3] {
    let t = ($in | where level == $level)
    let last = ($t | get frame_size | math max)
    match $which {
        "read" => (
            ($t | each { |r| { series: "seekzstdsep", x: $r.frame_size, y: $r.seek_read_ms, label: (if $r.frame_size in $labelled_read { $"($r.seek_read_ms | math round --precision 1) ms" } else { "" }) } })
            | append ($t | each { |r| { series: $"plain zstd -($level), decoding from the start", x: $r.frame_size, y: $r.zstd_read_ms, label: (if $r.frame_size == $last { $"($r.zstd_read_ms | math round --precision 1) ms" } else { "" }), below: true } })
        )
        "size" => (
            ($t | each { |r| { series: "seekzstdsep", x: $r.frame_size, y: ($r.seek_bytes / 1000000), label: (if $r.frame_size in $labelled { fmt-mb $r.seek_bytes } else { "" }) } })
            | append ($t | each { |r| { series: $"plain zstd -($level)", x: $r.frame_size, y: ($r.zstd_bytes / 1000000), label: (if $r.frame_size == $last { fmt-mb $r.zstd_bytes } else { "" }), below: true } })
        )
    }
}

export def sweep-chart [--level: int = 3] {
    let t = $in
    let records = ($t | first | get raw_bytes | $in / 1000000 | math round --precision 1)
    [
        ($t | sweep-panel read --level $level | svg-lines --title $"Level ($level): time to read one record, ms \(mean over 7 positions\)" --x-bytes --y-min 0 --y-max 25 --legend-left --height 300)
        ($t | sweep-panel size --level $level | svg-lines --title $"Level ($level): compressed size, MB \(($records) MB raw\)" --x-label "frame size" --x-bytes --y-min 0 --height 300)
    ] | svg-stack
}
