# Draw a line chart as SVG from a flat table of points.
#
# Input: a table with columns `series`, `x`, `y`, and optionally `label` (text drawn above the
# point, or below it when `below` is true). One line per distinct `series`, in first-seen
# order. Output: the SVG document as a string. Colors and fonts follow docs/bench/read-latency.svg.
#
# nushell:
#   [[series x y]; [a 1 1.0] [a 2 1.5] [b 1 0.8] [b 2 0.9]]
#   | svg-lines --title "demo" --x-label "x" --y-label "y" | save -f demo.svg

const palette = ["#2a78d6" "#eb6834" "#1baf7a" "#8b5cf6" "#d9a400" "#0e9aa7" "#c0392b" "#6b7280"]
const lp = "("
const rp = ")"

# Pretty tick label: 4096 -> "4 KiB", 1048576 -> "1 MiB", 1.25 -> "1.25".
def fmt-tick [v: float, bytes: bool] {
    if $bytes {
        if $v >= 1048576 { $"($v / 1048576 | into int) MiB" } else if $v >= 1024 { $"($v / 1024 | into int) KiB" } else { $"($v | into int) B" }
    } else {
        let s = ($v | math round --precision 3 | into string)
        $s
    }
}

# Ticks for a linear axis: about `n` "nice" steps covering [lo, hi].
def nice-ticks [lo: float, hi: float, n: int] {
    let span = ($hi - $lo)
    if $span <= 0 { return [$lo] }
    let raw = ($span / $n)
    let mag = (10.0 ** ($raw | math log 10 | math floor | into float))
    let norm = ($raw / $mag)
    let step = (if $norm < 1.5 { 1 } else if $norm < 3 { 2 } else if $norm < 7 { 5 } else { 10 }) * $mag
    let start = (($lo / $step | math floor) * $step)
    let end = (($hi / $step | math ceil) * $step)
    seq $start $step $end | each { |t| $t | into float }
}

# Ticks for a log10 axis: 1, 2, 5 in every decade that [lo, hi] touches, the ends included.
def log-ticks [lo: float, hi: float] {
    let d0 = ($lo | math log 10 | math floor | into int) - 1; let d1 = ($hi | math log 10 | math ceil | into int)
    let cand = (seq $d0 $d1 | each { |d| [1 2 5] | each { |m| $m * (10.0 ** ($d | into float)) } } | flatten)
    let below = ($cand | where { |t| $t <= $lo } | last)
    let above = ($cand | where { |t| $t >= $hi } | first)
    $cand | where { |t| $t >= $below and $t <= $above }
}

# Ticks for a linear byte axis: 0 and multiples of the power of two that gives at most `n` steps.
def pow2-ticks [hi: float, n: int] {
    let step = (2.0 ** (($hi / $n) | math log 2 | math ceil))
    seq 0.0 $step ($hi + $step * 0.001) | each { |t| $t | into float }
}

export def svg-lines [
    --title: string = ""
    --x-label: string = ""
    --y-label: string = ""
    --log-x            # x axis on log2, ticks at each distinct x
    --log-y            # y axis on log10, ticks at 1, 2, 5 per decade
    --legend-left      # legend in the top-left corner instead of the top-right
    --x-bytes          # x tick labels as B / KiB / MiB
    --y-min: float     # force the lower end of y (e.g. 0 or 1)
    --y-max: float     # force the upper end of y (room for the legend, say)
    --y-ref: float     # draw a dashed horizontal reference line (e.g. 1.0 = parity)
    --width: int = 760
    --height: int = 400
] {
    let pts = ($in | each { |r| { series: ($r.series | into string), x: ($r.x | into float), y: ($r.y | into float), label: ($r.label? | default "" | into string), below: ($r.below? | default false) } })
    let names = ($pts | get series | uniq)
    let ml = 64; let mr = 20; let mt = (if $title == "" { 16 } else { 40 }); let mb = 56
    let pw = ($width - $ml - $mr); let ph = ($height - $mt - $mb)

    let tx = { |x: float| if $log_x { $x | math log 2 } else { $x } }
    let xs = ($pts | get x | uniq | sort)
    let xlo = (do $tx ($xs | first)); let xhi = (do $tx ($xs | last))
    let ty = { |y: float| if $log_y { $y | math log 10 } else { $y } }
    let ylo0 = ($pts | get y | math min); let yhi0 = ($pts | get y | math max)
    let ylo1 = (if $y_min == null { $ylo0 } else { [$y_min $ylo0] | math min })
    let ylo1 = (if $y_ref == null { $ylo1 } else { [$ylo1 $y_ref] | math min })
    let yhi1 = (if $y_ref == null { $yhi0 } else { [$yhi0 $y_ref] | math max })
    let yhi1 = (if $y_max == null { $yhi1 } else { [$y_max $yhi1] | math max })
    let yticks = (if $log_y { log-ticks $ylo1 $yhi1 } else { nice-ticks $ylo1 $yhi1 5 })
    let ylo = (do $ty ($yticks | first)); let yhi = (do $ty ($yticks | last))
    let sx = { |x: float| $ml + ((do $tx $x) - $xlo) / ($xhi - $xlo) * $pw }
    let sy = { |y: float| $mt + $ph - ((do $ty $y) - $ylo) / ($yhi - $ylo) * $ph }

    let xticks = (if $log_x { $xs } else if $x_bytes { pow2-ticks ($xs | last) 8 } else { nice-ticks ($xs | first) ($xs | last) 6 })
    let grid = ($yticks | each { |t|
        let y = (do $sy $t)
        $'<line class="gr" x1="($ml)" y1="($y)" x2="($ml + $pw)" y2="($y)"/><text class="t2" x="($ml - 8)" y="($y + 4)" font-size="12" text-anchor="end">(fmt-tick $t false)</text>'
    } | str join "\n")
    let xaxis = ($xticks | each { |t|
        let x = (do $sx $t)
        $'<line class="gr" x1="($x)" y1="($mt + $ph)" x2="($x)" y2="($mt + $ph + 5)"/><text class="t2" x="($x)" y="($mt + $ph + 20)" font-size="11" text-anchor="middle">(fmt-tick $t $x_bytes)</text>'
    } | str join "\n")
    let ref = (if $y_ref == null { "" } else {
        let y = (do $sy $y_ref)
        $'<line class="t2" stroke="#52514e" stroke-dasharray="4 4" x1="($ml)" y1="($y)" x2="($ml + $pw)" y2="($y)"/>'
    })
    let lines = ($names | enumerate | each { |e|
        let k = ($e.index mod ($palette | length))
        let sp = ($pts | where series == $e.item | sort-by x)
        let d = ($sp | each { |p| $"(do $sx $p.x | math round --precision 1),(do $sy $p.y | math round --precision 1)" } | str join " ")
        let dots = ($sp | each { |p| $'<circle class="e($k)" cx="(do $sx $p.x | math round --precision 1)" cy="(do $sy $p.y | math round --precision 1)" r="3"/>' } | str join "")
        let labels = ($sp | where label != "" | each { |p| let px = (do $sx $p.x); let py = (do $sy $p.y | math round --precision 1 | if $p.below { $in + 14 } else { $in - 6 }); if $px > $ml + $pw - 60 { $'<text class="t2" x="($px - 5 | math round --precision 1)" y="($py)" font-size="10" text-anchor="end">($p.label)</text>' } else { $'<text class="t2" x="($px + 5 | math round --precision 1)" y="($py)" font-size="10">($p.label)</text>' } } | str join "")
        $'<polyline class="k($k)" fill="none" stroke-width="2" points="($d)"/>($dots)($labels)'
    } | str join "\n")
    let legend = ($names | enumerate | each { |e|
        let k = ($e.index mod ($palette | length))
        let lx = (if $legend_left { $ml + 12 } else { $ml + $pw - 150 })
        let y = ($mt + 8 + $e.index * 18)
        $'<line class="k($k)" stroke-width="2" x1="($lx)" y1="($y)" x2="($lx + 20)" y2="($y)"/><text class="t1" x="($lx + 26)" y="($y + 4)" font-size="12">($e.item)</text>'
    } | str join "\n")
    let styles = ($palette | enumerate | each { |c| $".k($c.index){stroke:($c.item)}.e($c.index){fill:($c.item);stroke:#fcfcfb}" } | str join "\n")
    let title_el = (if $title == "" { "" } else { $'<text class="t1" x="($ml)" y="24" font-size="15" font-weight="600">($title)</text>' })
    let xl = (if $x_label == "" { "" } else { $'<text class="t2" x="($ml + $pw / 2)" y="($height - 12)" font-size="12" text-anchor="middle">($x_label)</text>' })
    let yl = (if $y_label == "" { "" } else { $'<text class="t2" transform="translate($lp)14,($mt + $ph / 2)($rp) rotate($lp)-90($rp)" font-size="12" text-anchor="middle">($y_label)</text>' })

    $'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ($width) ($height)" width="($width)" height="($height)" role="img" aria-label="($title)">
<style>
text{font-family:-apple-system,BlinkMacSystemFont,Helvetica,Arial,sans-serif}
.sf{fill:#fcfcfb}.gr{stroke:#e6e5e1}.t1{fill:#0b0b0b}.t2{fill:#52514e}
($styles)
@media ($lp)prefers-color-scheme:dark($rp){.sf{fill:#1a1a19}.gr{stroke:#33332f}.t1{fill:#f0efea}.t2{fill:#a8a79f}}
</style>
<rect class="sf" width="($width)" height="($height)"/>
($title_el)
($grid)
($xaxis)
($ref)
($lines)
($legend)
($xl)
($yl)
</svg>
'
}

# Stack SVG documents from `svg-lines` vertically into one. Each keeps its own width; the result
# is as wide as the widest.
export def svg-stack [] {
    let parts = ($in | each { |s|
        let w = ($s | parse --regex 'width="(\d+)" height="(\d+)" role' | first)
        { svg: $s, w: ($w.capture0 | into int), h: ($w.capture1 | into int) }
    })
    let width = ($parts | get w | math max)
    let height = ($parts | get h | math sum)
    let body = ($parts | reduce -f { y: 0, out: "" } { |p, acc|
        let inner = ($p.svg | str replace --regex '<svg [^>]*>' $'<g transform="translate($lp)0,($acc.y)($rp)">' | str replace --regex '</svg>\s*$' "</g>")
        { y: ($acc.y + $p.h), out: ($acc.out + $inner + "\n") }
    } | get out)
    $'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ($width) ($height)" width="($width)" height="($height)" role="img">
($body)</svg>
'
}
