//! seekzstdsep benchmark harness.
//!
//! One measurement: read `cnt` records starting at record `from`, from a file, recording elapsed
//! time, memory, CPU time and bytes read. Three ways of doing it are compared:
//!
//! - `uncompressed+shell` — `tail -n +N | head -n C` on the plain JSONL
//! - `seekzstdsep` — `seekzstdsep cat --from N --cnt C`
//! - `zstd+shell` — `zstd -dc | tail -n +N | head -n C`
//!
//! `from` and `cnt` form a matrix, not two separate one-dimensional sweeps: every position is
//! measured at every record count. Plus a compression benchmark, so that a change to the read path
//! is not paid for on the write side.
//!
//! Rules from `docs/benchmark.md` that this file implements:
//!
//! - input is always a file, never a pipe into the tool under test
//! - storage location is a parameter and is recorded with every result
//! - cache state is a parameter and is recorded with every result
//! - shell baselines exit early; `tail -n +N | head` is the representative
//! - a pipeline's stages are aggregated by one rule applied to every baseline (see `measure`)

mod fixture;
mod measure;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use measure::{Metrics, Run, Sink};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "szbench", about = "seekzstdsep benchmark harness")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate (or verify) the fixture for a record count and print its sizes.
    Fixture(FixtureArgs),
    /// Measure and write a result file.
    Run(RunArgs),
    /// Render result files as markdown.
    Report(ReportArgs),
}

#[derive(clap::Args)]
struct FixtureArgs {
    #[arg(long, default_value = "local")]
    storage: String,
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 1_000_000)]
    records: u64,
    #[arg(long, default_value_t = 65536)]
    frame_size: usize,
    #[arg(long, default_value_t = 3)]
    level: i32,
}

#[derive(clap::Args)]
struct RunArgs {
    /// matrix | compress | all
    #[arg(long, value_delimiter = ',', default_value = "all")]
    suite: Vec<String>,
    #[arg(long, default_value = "local")]
    storage: String,
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// warm | cold
    #[arg(long, default_value = "warm")]
    cache: String,
    /// fadvise (per-file, default) | drop-caches (global, needs root)
    #[arg(long, default_value = "fadvise")]
    cold_method: String,
    #[arg(long, default_value_t = 1_000_000)]
    records: u64,
    /// Record positions, as fractions of the record count.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "0,0.1,0.25,0.5,0.75,0.9,0.999"
    )]
    positions: Vec<f64>,
    /// Record counts to read at each position.
    #[arg(long, value_delimiter = ',', default_value = "1,10,100")]
    cnts: Vec<u64>,
    #[arg(long, default_value_t = 65536)]
    frame_size: usize,
    #[arg(long, default_value_t = 3)]
    level: i32,
    /// CPU to pin every process to, so no engine gets more of the machine than another. Set to a
    /// negative value to leave scheduling alone.
    #[arg(long, default_value_t = 3)]
    pin_cpu: i32,
    /// Rounds. One round measures every cell once; see `run_rounds`.
    #[arg(long, default_value_t = 10)]
    reps: u32,
    #[arg(long, default_value_t = 1)]
    warmups: u32,
    /// Skip the untimed strace pass that measures logical bytes read.
    #[arg(long, default_value_t = false)]
    no_io_pass: bool,
    /// Directory holding the seekzstdsep binary.
    #[arg(long)]
    bin_dir: Option<PathBuf>,
    /// Free-form tag recorded with every row.
    #[arg(long, default_value = "")]
    label: String,
    #[arg(long)]
    out: PathBuf,
}

#[derive(clap::Args)]
struct ReportArgs {
    #[arg(long = "in", value_delimiter = ',')]
    inputs: Vec<PathBuf>,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value = "")]
    title: String,
}

// ---------------------------------------------------------------- result model

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Env {
    date: String,
    host: String,
    kernel: String,
    cpu: String,
    cores: String,
    mem_total: String,
    zstd_cli: String,
    git_commit: String,
    git_dirty: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Conditions {
    label: String,
    storage: String,
    dir: String,
    fs: String,
    cache: String,
    cold_method: String,
    reps: u32,
    warmups: u32,
    frame_size: usize,
    zstd_level: i32,
    pin_cpu: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Row {
    suite: String,
    engine: String,
    records: u64,
    raw_bytes: u64,
    from: Option<u64>,
    cnt: Option<u64>,
    wall_ms_min: f64,
    wall_ms_med: f64,
    wall_ms_max: f64,
    cpu_ms_med: f64,
    max_rss_kb: u64,
    blk_read_bytes: u64,
    logical_read_bytes: Option<u64>,
    out_bytes: u64,
    out_records: u64,
    /// Whether the first `cnt` records match what `uncompressed+shell` returned for the same cell.
    content_ok: bool,
    ratio: Option<f64>,
    ok: bool,
    cmd: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Results {
    env: Env,
    conditions: Conditions,
    fixture: fixture::Meta,
    rows: Vec<Row>,
}

// ---------------------------------------------------------------- entry point

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Fixture(a) => cmd_fixture(a),
        Cmd::Run(a) => cmd_run(a),
        Cmd::Report(a) => cmd_report(a),
    }
}

fn storage_dir(storage: &str, override_dir: &Option<PathBuf>) -> Result<PathBuf> {
    if let Some(d) = override_dir {
        return Ok(d.clone());
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    Ok(match storage {
        // Both local targets named by docs/benchmark.md are XFS on the same nvme device.
        "local" | "home" => PathBuf::from(home).join(".cache/seekzstdsep-bench"),
        "tmp" => PathBuf::from("/tmp/seekzstdsep-bench"),
        // No default: the NFS mount is site-specific, so the caller names it.
        "nfs" => PathBuf::from(
            std::env::var("SZBENCH_NFS_DIR")
                .map_err(|_| anyhow!("storage nfs needs SZBENCH_NFS_DIR; or pass --cache-dir"))?,
        )
        .join("seekzstdsep-bench"),
        other => bail!("unknown storage {other}; pass --cache-dir"),
    })
}

fn fs_type(p: &Path) -> String {
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let Ok(c) = std::ffi::CString::new(p.to_string_lossy().as_bytes()) else {
        return "?".into();
    };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return "?".into();
    }
    match st.f_type as i64 {
        0x5846_5342 => "xfs",
        0x6969 => "nfs",
        0x0102_1994 => "tmpfs",
        0xEF53 => "ext4",
        0x9123_683E => "btrfs",
        0xCA45_1A4E => "bcachefs",
        _ => "other",
    }
    .into()
}

fn cmd_fixture(a: FixtureArgs) -> Result<()> {
    let dir = storage_dir(&a.storage, &a.cache_dir)?;
    let fx = fixture::ensure(&dir, a.records, a.frame_size, a.level)?;
    println!("{}", serde_json::to_string_pretty(&fx.meta)?);
    println!("dir: {} ({})", dir.display(), fs_type(&dir));
    Ok(())
}

// ---------------------------------------------------------------- measuring

struct Ctx {
    reps: u32,
    warmups: u32,
    cold: bool,
    cold_method: String,
    io_pass: bool,
    scratch: PathBuf,
}

impl Ctx {
    fn prep(&self, files: &[PathBuf]) -> Result<()> {
        if !self.cold {
            return Ok(());
        }
        match self.cold_method.as_str() {
            "drop-caches" => measure::drop_caches(),
            _ => measure::evict(files),
        }
    }
}

/// One cell of the matrix — one engine at one `from` and `cnt` — with its measurements.
struct Slot {
    suite: &'static str,
    engine: &'static str,
    from: Option<u64>,
    cnt: Option<u64>,
    run: Run,
    io_targets: Vec<PathBuf>,
    /// Compression only: the file the run produces, sized once at the end for the ratio.
    out_path: Option<PathBuf>,
    wall_ms: Vec<f64>,
    cpu_ms: Vec<f64>,
    max_rss_kb: u64,
    blk_read_bytes: u64,
    logical_read_bytes: Option<u64>,
    out_bytes: u64,
    out_records: u64,
    /// Digest of the first `cnt` records this engine returned.
    digest: u64,
    content_ok: bool,
    ok: bool,
}

struct Sample {
    wall_ms: Vec<f64>,
    cpu_ms: Vec<f64>,
    max_rss_kb: u64,
    blk_read_bytes: u64,
    logical_read_bytes: Option<u64>,
    out_bytes: u64,
    out_records: u64,
    content_ok: bool,
    ok: bool,
}

impl Slot {
    fn new(
        suite: &'static str,
        engine: &'static str,
        from: Option<u64>,
        cnt: Option<u64>,
        run: Run,
        io_targets: Vec<PathBuf>,
    ) -> Self {
        Slot {
            suite,
            engine,
            from,
            cnt,
            run,
            io_targets,
            out_path: None,
            wall_ms: Vec::new(),
            cpu_ms: Vec::new(),
            max_rss_kb: 0,
            blk_read_bytes: 0,
            logical_read_bytes: None,
            out_bytes: 0,
            out_records: 0,
            digest: 0,
            content_ok: true,
            ok: true,
        }
    }

    fn sample(self) -> Sample {
        Sample {
            wall_ms: self.wall_ms,
            cpu_ms: self.cpu_ms,
            max_rss_kb: self.max_rss_kb,
            blk_read_bytes: self.blk_read_bytes,
            logical_read_bytes: self.logical_read_bytes,
            out_bytes: self.out_bytes,
            out_records: self.out_records,
            content_ok: self.content_ok,
            ok: self.ok,
        }
    }
}

/// Measures every slot once per round, reversing the order on every other round.
///
/// Running one cell N times back to back measures how warm that one cell got, and hands each
/// engine its own slice of wall clock, so any drift in the machine lands on whichever engine
/// happened to be running then. A round holds the whole set instead, so the engines sit next to
/// each other in time and share whatever the machine was doing. Alternating the direction removes
/// the remaining advantage of always being measured first.
fn run_rounds(ctx: &Ctx, slots: &mut [Slot], evict_files: &[PathBuf]) -> Result<()> {
    let total = ctx.warmups + ctx.reps;
    for round in 0..total {
        let recorded = round >= ctx.warmups;
        let forward = round % 2 == 0;
        let order: Vec<usize> = if forward {
            (0..slots.len()).collect()
        } else {
            (0..slots.len()).rev().collect()
        };
        eprintln!(
            "# round {}/{}{}, {}",
            round + 1,
            total,
            if recorded { "" } else { " (warmup, discarded)" },
            if forward { "forward" } else { "reverse" }
        );
        for i in order {
            ctx.prep(evict_files)?;
            let m: Metrics = measure::run_once(&slots[i].run)?;
            if !recorded {
                continue;
            }
            let s = &mut slots[i];
            s.wall_ms.push(m.wall_ns as f64 / 1e6);
            s.cpu_ms.push(m.cpu_ns() as f64 / 1e6);
            s.max_rss_kb = s.max_rss_kb.max(m.max_rss_kb);
            s.blk_read_bytes = s.blk_read_bytes.max(m.blk_read_bytes);
            s.ok &= m.ok();
        }
    }
    Ok(())
}

/// One untimed run per slot with the output captured, so what each engine returned is recorded
/// rather than assumed.
fn verify_pass(ctx: &Ctx, slots: &mut [Slot], evict_files: &[PathBuf]) -> Result<()> {
    let verify = ctx.scratch.join("verify.out");
    for s in slots.iter_mut() {
        let mut vrun = s.run.clone();
        let capture = s.out_path.is_none();
        if capture {
            vrun.sink = Sink::File(verify.clone());
        }
        ctx.prep(evict_files)?;
        let m = measure::run_once(&vrun)?;
        s.ok &= m.ok();
        if capture {
            s.out_bytes = std::fs::metadata(&verify).map(|m| m.len()).unwrap_or(0);
            s.out_records = count_lines(&verify)?;
            // `cat --cnt C` returns C+1 records, so compare the first C.
            s.digest = digest_of_first(&verify, s.cnt.unwrap_or(0))?;
        }
    }
    let _ = std::fs::remove_file(&verify);

    // The three engines have to return the same records, or the timings are comparing different
    // work. `uncompressed+shell` is the reference because it is the one doing no decoding.
    let refs: Vec<(Option<u64>, Option<u64>, u64)> = slots
        .iter()
        .filter(|s| s.suite == "matrix" && s.engine == "uncompressed+shell")
        .map(|s| (s.from, s.cnt, s.digest))
        .collect();
    for s in slots.iter_mut().filter(|s| s.suite == "matrix") {
        if let Some((_, _, d)) = refs.iter().find(|(f, c, _)| *f == s.from && *c == s.cnt) {
            s.content_ok = s.digest == *d;
        }
    }
    let bad = slots.iter().filter(|s| !s.content_ok).count();
    if bad > 0 {
        eprintln!("# WARNING: {bad} cells returned different records than uncompressed+shell");
    }
    Ok(())
}

/// Digest of the first `n` records of a file, so engines can be compared without holding the
/// output in memory.
fn digest_of_first(p: &Path, n: u64) -> Result<u64> {
    use std::hash::{Hash, Hasher};
    let Ok(data) = std::fs::read(p) else {
        return Ok(0);
    };
    let mut end = 0usize;
    for _ in 0..n {
        match memchr::memchr(b'\n', &data[end..]) {
            Some(i) => end += i + 1,
            None => {
                end = data.len();
                break;
            }
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data[..end].hash(&mut h);
    Ok(h.finish())
}

/// One untimed `strace` run per slot. Separate because `strace` destroys the timing.
fn io_pass(ctx: &Ctx, slots: &mut [Slot], evict_files: &[PathBuf]) -> Result<()> {
    for s in slots.iter_mut() {
        ctx.prep(evict_files)?;
        s.logical_read_bytes = Some(measure::io_bytes(&s.run, &s.io_targets)?);
    }
    Ok(())
}

fn count_lines(p: &Path) -> Result<u64> {
    let Ok(data) = std::fs::read(p) else {
        return Ok(0);
    };
    Ok(memchr::memchr_iter(b'\n', &data).count() as u64)
}

fn med(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if s.is_empty() {
        return f64::NAN;
    }
    s[s.len() / 2]
}
fn mn(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::INFINITY, f64::min)
}
fn mx(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
}

// ---------------------------------------------------------------- the three engines

/// Engine names avoid `|` so they can be markdown headers unescaped. The exact command each one
/// ran is recorded in its row's `cmd`.
const ENGINES: [&str; 3] = ["uncompressed+shell", "seekzstdsep", "zstd+shell"];

struct Case {
    engine: &'static str,
    run: Run,
    io_targets: Vec<PathBuf>,
}

fn cases(szs: &str, fx: &fixture::Fixture, from: u64, cnt: u64, pin: Option<usize>) -> Vec<Case> {
    let pinned = |mut r: Run| {
        r.pin_cpu = pin;
        r
    };
    let s = |x: &str| x.to_string();
    // `tail -n +N` is 1-based on lines; `from` is a 0-based record index.
    let line = format!("+{}", from + 1);
    vec![
        Case {
            engine: "uncompressed+shell",
            run: pinned(Run::pipeline(vec![
                vec![
                    s("tail"),
                    s("-n"),
                    line.clone(),
                    fx.raw.to_string_lossy().into_owned(),
                ],
                vec![s("head"), s("-n"), cnt.to_string()],
            ])),
            io_targets: vec![fx.raw.clone()],
        },
        Case {
            engine: "seekzstdsep",
            run: pinned(Run::single(vec![
                szs.to_string(),
                s("cat"),
                s("--from"),
                from.to_string(),
                s("--cnt"),
                cnt.to_string(),
                fx.seek.to_string_lossy().into_owned(),
            ])),
            io_targets: vec![fx.seek.clone()],
        },
        Case {
            engine: "zstd+shell",
            run: pinned(Run::pipeline(vec![
                // Without --no-asyncio, zstd 1.5.4+ reads its input on a second thread, so this
                // stage alone would use more than one CPU worth of work per unit of elapsed time.
                vec![
                    s("zstd"),
                    s("-dcq"),
                    s("--no-asyncio"),
                    fx.zstd.to_string_lossy().into_owned(),
                ],
                vec![s("tail"), s("-n"), line],
                vec![s("head"), s("-n"), cnt.to_string()],
            ])),
            io_targets: vec![fx.zstd.clone()],
        },
    ]
}

fn compress_cases(
    szs: &str,
    fx: &fixture::Fixture,
    dir: &Path,
    level: i32,
    pin: Option<usize>,
) -> Vec<(&'static str, Run, PathBuf)> {
    let out_seek = dir.join("bench-out.seek.zst");
    let out_zstd = dir.join("bench-out.zst");
    let mut list: Vec<(&'static str, Run, PathBuf)> = vec![
        (
            "seekzstdsep",
            Run::single(vec![
                szs.to_string(),
                "compress".into(),
                fx.raw.to_string_lossy().into_owned(),
                out_seek.to_string_lossy().into_owned(),
            ]),
            out_seek,
        ),
        (
            "zstd",
            Run::single(vec![
                "zstd".into(),
                "-q".into(),
                "-f".into(),
                "--no-asyncio".into(),
                format!("-{level}"),
                "-o".into(),
                out_zstd.to_string_lossy().into_owned(),
                fx.raw.to_string_lossy().into_owned(),
            ]),
            out_zstd,
        ),
    ];
    for (_, run, _) in list.iter_mut() {
        run.pin_cpu = pin;
    }
    list
}

fn pos(records: u64, f: f64) -> u64 {
    ((records as f64) * f) as u64
}

// ---------------------------------------------------------------- run

fn cmd_run(a: RunArgs) -> Result<()> {
    let dir = storage_dir(&a.storage, &a.cache_dir)?;
    let szs = seekzstdsep_bin(&a.bin_dir)?;
    let scratch = std::env::temp_dir().join(format!("szbench-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let ctx = Ctx {
        reps: a.reps,
        warmups: if a.cache == "cold" { 0 } else { a.warmups },
        cold: a.cache == "cold",
        cold_method: a.cold_method.clone(),
        io_pass: !a.no_io_pass,
        scratch: scratch.clone(),
    };

    for s in &a.suite {
        if !["all", "matrix", "compress"].contains(&s.as_str()) {
            bail!("unknown suite {s}");
        }
    }
    let all = a.suite.iter().any(|s| s == "all");
    let do_matrix = all || a.suite.iter().any(|s| s == "matrix");
    let do_compress = all || a.suite.iter().any(|s| s == "compress");

    let pin: Option<usize> = (a.pin_cpu >= 0).then_some(a.pin_cpu as usize);
    let fx = fixture::ensure(&dir, a.records, a.frame_size, a.level)?;

    // Build every slot first, then measure all of them together, round by round.
    let mut slots: Vec<Slot> = Vec::new();
    if do_matrix {
        for f in a.positions.iter().copied() {
            let from = pos(fx.meta.records, f);
            for cnt in a.cnts.iter().copied() {
                for c in cases(&szs, &fx, from, cnt, pin) {
                    slots.push(Slot::new(
                        "matrix",
                        c.engine,
                        Some(from),
                        Some(cnt),
                        c.run,
                        c.io_targets,
                    ));
                }
            }
        }
    }
    if do_compress {
        for (engine, run, outp) in compress_cases(&szs, &fx, &dir, a.level, pin) {
            let mut slot = Slot::new("compress", engine, None, None, run, vec![fx.raw.clone()]);
            slot.out_path = Some(outp);
            slots.push(slot);
        }
    }
    eprintln!(
        "# {} cells, {} recorded rounds (+{} warmup)",
        slots.len(),
        ctx.reps,
        ctx.warmups
    );

    let evict_files = fx.all_files();
    verify_pass(&ctx, &mut slots, &evict_files)?;
    run_rounds(&ctx, &mut slots, &evict_files)?;
    if ctx.io_pass {
        io_pass(&ctx, &mut slots, &evict_files)?;
    }

    let mut rows: Vec<Row> = Vec::new();
    for mut s in slots {
        let (suite, engine, from, cnt) = (s.suite, s.engine, s.from, s.cnt);
        let run = s.run.clone();
        let mut ratio = None;
        if let Some(outp) = s.out_path.clone() {
            let osz = std::fs::metadata(&outp).map(|m| m.len()).unwrap_or(0);
            s.out_bytes = osz;
            s.out_records = 0;
            if osz > 0 {
                ratio = Some(fx.meta.raw_bytes as f64 / osz as f64);
            }
            let _ = std::fs::remove_file(&outp);
        }
        rows.push(row(suite, engine, &fx, from, cnt, ratio, &run, s.sample()));
    }

    let results = Results {
        env: env_info()?,
        conditions: Conditions {
            label: a.label.clone(),
            storage: a.storage.clone(),
            dir: dir.to_string_lossy().into_owned(),
            fs: fs_type(&dir),
            cache: a.cache.clone(),
            cold_method: if a.cache == "cold" {
                a.cold_method.clone()
            } else {
                "n/a".into()
            },
            reps: a.reps,
            warmups: ctx.warmups,
            frame_size: a.frame_size,
            zstd_level: a.level,
            pin_cpu: pin,
        },
        fixture: fx.meta.clone(),
        rows,
    };
    if let Some(p) = a.out.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&a.out, serde_json::to_string_pretty(&results)?)?;
    let _ = std::fs::remove_dir_all(&scratch);
    eprintln!("# wrote {}", a.out.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn row(
    suite: &str,
    engine: &str,
    fx: &fixture::Fixture,
    from: Option<u64>,
    cnt: Option<u64>,
    ratio: Option<f64>,
    run: &Run,
    s: Sample,
) -> Row {
    Row {
        suite: suite.into(),
        engine: engine.into(),
        records: fx.meta.records,
        raw_bytes: fx.meta.raw_bytes,
        from,
        cnt,
        wall_ms_min: mn(&s.wall_ms),
        wall_ms_med: med(&s.wall_ms),
        wall_ms_max: mx(&s.wall_ms),
        cpu_ms_med: med(&s.cpu_ms),
        max_rss_kb: s.max_rss_kb,
        blk_read_bytes: s.blk_read_bytes,
        logical_read_bytes: s.logical_read_bytes,
        out_bytes: s.out_bytes,
        out_records: s.out_records,
        content_ok: s.content_ok,
        ratio,
        ok: s.ok,
        cmd: run.to_shell(),
    }
}

fn seekzstdsep_bin(dir: &Option<PathBuf>) -> Result<String> {
    let d = match dir {
        Some(d) => d.clone(),
        None => std::env::current_exe()?
            .parent()
            .context("no parent of current exe")?
            .to_path_buf(),
    };
    let p = d.join("seekzstdsep");
    Ok(if p.exists() {
        p.to_string_lossy().into_owned()
    } else {
        "seekzstdsep".into()
    })
}

fn sh(cmd: &str) -> String {
    std::process::Command::new("sh")
        .args(["-c", cmd])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn env_info() -> Result<Env> {
    Ok(Env {
        date: sh("date -Is"),
        host: sh("hostname"),
        kernel: sh("uname -sr"),
        cpu: sh("awk -F': ' '/model name/{print $2; exit}' /proc/cpuinfo"),
        cores: sh("nproc"),
        mem_total: sh("awk '/MemTotal/{printf \"%.1f GiB\", $2/1048576}' /proc/meminfo"),
        zstd_cli: sh("zstd --version"),
        git_commit: sh("git rev-parse --short HEAD"),
        git_dirty: !sh("git status --porcelain -- src Cargo.toml").is_empty(),
    })
}

// ---------------------------------------------------------------- report

fn cmd_report(a: ReportArgs) -> Result<()> {
    let mut out = String::new();
    if !a.title.is_empty() {
        out.push_str(&format!("# {}\n\n", a.title));
    }
    // Result files taken under the same conditions merge into one section.
    let mut groups: Vec<Results> = Vec::new();
    for path in &a.inputs {
        let r: Results = serde_json::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("parse {}", path.display()))?;
        match groups.iter_mut().find(|g| {
            g.conditions.dir == r.conditions.dir
                && g.conditions.cache == r.conditions.cache
                && g.conditions.label == r.conditions.label
        }) {
            Some(g) => g.rows.extend(r.rows),
            None => groups.push(r),
        }
    }
    for (i, r) in groups.iter().enumerate() {
        if i == 0 {
            out.push_str(&env_block(&r.env));
        }
        out.push_str(&conditions_block(r));
        let matrix: Vec<&Row> = r.rows.iter().filter(|x| x.suite == "matrix").collect();
        if !matrix.is_empty() {
            out.push_str(&matrix_blocks(&matrix));
        }
        let comp: Vec<&Row> = r.rows.iter().filter(|x| x.suite == "compress").collect();
        if !comp.is_empty() {
            out.push_str(&compress_block(&comp));
        }
    }
    std::fs::write(&a.out, out)?;
    eprintln!("# wrote {}", a.out.display());
    Ok(())
}

fn env_block(e: &Env) -> String {
    format!(
        "## Machine\n\n\
         | | |\n| --- | --- |\n\
         | date | {} |\n| host | {} |\n| kernel | {} |\n| cpu | {} ({} cores) |\n\
         | memory | {} |\n| zstd cli | {} |\n| commit | {}{} |\n\n",
        e.date,
        e.host,
        e.kernel,
        e.cpu,
        e.cores,
        e.mem_total,
        e.zstd_cli,
        e.git_commit,
        if e.git_dirty { " (dirty)" } else { "" }
    )
}

fn conditions_block(r: &Results) -> String {
    let f = &r.fixture;
    let mut s = format!(
        "## Conditions{}\n\n\
         | | |\n| --- | --- |\n\
         | storage | `{}` ({}, preset `{}`) |\n\
         | cache | {}{} |\n\
         | repetitions | {} timed (+{} warmup), tables show the best |\n\
         | fixture | {} records, {} uncompressed |\n\
         | seekzstdsep file | {} in {} frames of {} records |\n\
         | plain zstd file | {} |\n\
         | cpu | {} |\n\
         | frame size | {} |\n| zstd level | {} |\n\n",
        if r.conditions.label.is_empty() {
            String::new()
        } else {
            format!(" — {}", r.conditions.label)
        },
        r.conditions.dir,
        r.conditions.fs,
        r.conditions.storage,
        r.conditions.cache,
        if r.conditions.cache == "cold" {
            format!(" (evicted per run with {})", r.conditions.cold_method)
        } else {
            String::new()
        },
        r.conditions.reps,
        r.conditions.warmups,
        f.records,
        mb(f.raw_bytes),
        mb(f.seek_bytes),
        f.seek_frames,
        f.seek_records_per_frame,
        mb(f.zstd_bytes),
        match r.conditions.pin_cpu {
            Some(c) => format!("every process pinned to cpu {c}"),
            None => "not pinned".into(),
        },
        r.conditions.frame_size,
        r.conditions.zstd_level,
    );
    // What each engine name stands for, quoting a command actually executed. `--from` and `--cnt`
    // vary per row; everything else is fixed.
    s.push_str("### What each engine is\n\n| engine | command, as run |\n| --- | --- |\n");
    for e in ENGINES {
        if let Some(row) = r.rows.iter().find(|x| x.suite == "matrix" && x.engine == e) {
            s.push_str(&format!("| {} | `{}` |\n", e, row.cmd.replace('|', "\\|")));
        }
    }
    s.push('\n');
    s
}

fn axes(rows: &[&Row]) -> (Vec<u64>, Vec<u64>) {
    let mut froms: Vec<u64> = rows.iter().filter_map(|r| r.from).collect();
    froms.sort_unstable();
    froms.dedup();
    let mut cnts: Vec<u64> = rows.iter().filter_map(|r| r.cnt).collect();
    cnts.sort_unstable();
    cnts.dedup();
    (froms, cnts)
}

fn find<'a>(rows: &[&'a Row], engine: &str, from: u64, cnt: u64) -> Option<&'a Row> {
    rows.iter()
        .find(|r| r.engine == engine && r.from == Some(from) && r.cnt == Some(cnt))
        .copied()
}


/// Every table names the engine and the unit in its own header.
///
/// One row per `from` x `cnt` cell, one column per engine, so the three ways of doing the same job
/// are read straight across. Nothing here depends on a legend placed outside the table.
fn matrix_blocks(rows: &[&Row]) -> String {
    let (froms, cnts) = axes(rows);
    let mut s = String::new();
    let cells = || -> Vec<(u64, u64)> {
        froms
            .iter()
            .flat_map(|f| cnts.iter().map(move |c| (*f, *c)))
            .collect()
    };
    let get = |from: u64, cnt: u64, e: &str, f: &dyn Fn(&Row) -> String| -> String {
        match find(rows, e, from, cnt) {
            Some(r) => f(r),
            None => "-".into(),
        }
    };

    // Elapsed time, with the spread, since a single number hides a 5x outlier.
    s.push_str("### Elapsed time\n\n");
    // Grouped by statistic, not by engine: best sits next to best, so the three engines are
    // compared on the same footing. Grouping by engine puts the numbers being compared three
    // columns apart.
    s.push_str("| record position `--from` | records read `--cnt` | best ms, uncompressed+shell | best ms, seekzstdsep | best ms, zstd+shell | median ms, uncompressed+shell | median ms, seekzstdsep | median ms, zstd+shell | worst ms, uncompressed+shell | worst ms, seekzstdsep | worst ms, zstd+shell |\n");
    s.push_str("| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    let wall: [&dyn Fn(&Row) -> String; 3] = [
        &|r: &Row| format!("{:.2}", r.wall_ms_min),
        &|r: &Row| format!("{:.2}", r.wall_ms_med),
        &|r: &Row| format!("{:.2}", r.wall_ms_max),
    ];
    for (from, cnt) in cells() {
        s.push_str(&format!("| {from} | {cnt} |"));
        for f in wall {
            for e in ENGINES {
                s.push_str(&format!(" {} |", get(from, cnt, e, f)));
            }
        }
        s.push('\n');
    }
    s.push('\n');

    // Memory.
    s.push_str("### Memory\n\n");
    s.push_str("| record position `--from` | records read `--cnt` | uncompressed+shell, peak RSS MiB | seekzstdsep, peak RSS MiB | zstd+shell, peak RSS MiB |\n");
    s.push_str("| ---: | ---: | ---: | ---: | ---: |\n");
    for (from, cnt) in cells() {
        s.push_str(&format!("| {from} | {cnt} |"));
        for e in ENGINES {
            s.push_str(&format!(
                " {} |",
                get(from, cnt, e, &|r| format!("{:.1}", r.max_rss_kb as f64 / 1024.0))
            ));
        }
        s.push('\n');
    }
    s.push('\n');

    // CPU time.
    s.push_str("### CPU time\n\n");
    s.push_str("| record position `--from` | records read `--cnt` | uncompressed+shell, user+sys ms | seekzstdsep, user+sys ms | zstd+shell, user+sys ms |\n");
    s.push_str("| ---: | ---: | ---: | ---: | ---: |\n");
    for (from, cnt) in cells() {
        s.push_str(&format!("| {from} | {cnt} |"));
        for e in ENGINES {
            s.push_str(&format!(
                " {} |",
                get(from, cnt, e, &|r| format!("{:.2}", r.cpu_ms_med))
            ));
        }
        s.push('\n');
    }
    s.push('\n');

    // Bytes read: from the file, and from the block layer (non-zero only on a cold cache).
    s.push_str("### Bytes read\n\n");
    s.push_str("| record position `--from` | records read `--cnt` | uncompressed+shell, from file MB | seekzstdsep, from file MB | zstd+shell, from file MB | uncompressed+shell, from block layer MB | seekzstdsep, from block layer MB | zstd+shell, from block layer MB |\n");
    s.push_str("| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for (from, cnt) in cells() {
        s.push_str(&format!("| {from} | {cnt} |"));
        for e in ENGINES {
            s.push_str(&format!(
                " {} |",
                get(from, cnt, e, &|r| match r.logical_read_bytes {
                    Some(v) => format!("{:.2}", v as f64 / 1e6),
                    None => "-".into(),
                })
            ));
        }
        for e in ENGINES {
            s.push_str(&format!(
                " {} |",
                get(from, cnt, e, &|r| format!(
                    "{:.2}",
                    r.blk_read_bytes as f64 / 1e6
                ))
            ));
        }
        s.push('\n');
    }
    s.push('\n');

    // What each engine actually returned, measured rather than assumed.
    s.push_str("### Output returned\n\n");
    s.push_str("| record position `--from` | records read `--cnt` | uncompressed+shell, records | seekzstdsep, records | zstd+shell, records | uncompressed+shell, bytes | seekzstdsep, bytes | zstd+shell, bytes | uncompressed+shell, exit ok | seekzstdsep, exit ok | zstd+shell, exit ok | same records as uncompressed+shell: seekzstdsep | same records as uncompressed+shell: zstd+shell |\n");
    s.push_str("| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- | --- | --- |\n");
    for (from, cnt) in cells() {
        s.push_str(&format!("| {from} | {cnt} |"));
        for e in ENGINES {
            s.push_str(&format!(
                " {} |",
                get(from, cnt, e, &|r| r.out_records.to_string())
            ));
        }
        for e in ENGINES {
            s.push_str(&format!(
                " {} |",
                get(from, cnt, e, &|r| r.out_bytes.to_string())
            ));
        }
        for e in ENGINES {
            s.push_str(&format!(
                " {} |",
                get(from, cnt, e, &|r| if r.ok {
                    "yes".into()
                } else {
                    "NO".to_string()
                })
            ));
        }
        for e in ["seekzstdsep", "zstd+shell"] {
            s.push_str(&format!(
                " {} |",
                get(from, cnt, e, &|r| if r.content_ok {
                    "yes".into()
                } else {
                    "NO".to_string()
                })
            ));
        }
        s.push('\n');
    }
    s.push('\n');

    // The comparison the crate lives or dies by.
    s.push_str("### seekzstdsep against uncompressed+shell\n\n");
    s.push_str("| record position `--from` | records read `--cnt` | seekzstdsep elapsed / uncompressed+shell elapsed |\n");
    s.push_str("| ---: | ---: | ---: |\n");
    for (from, cnt) in cells() {
        let v = match (
            find(rows, "seekzstdsep", from, cnt),
            find(rows, "uncompressed+shell", from, cnt),
        ) {
            (Some(a), Some(b)) => format!("{:.2}x", a.wall_ms_min / b.wall_ms_min),
            _ => "-".into(),
        };
        s.push_str(&format!("| {from} | {cnt} | {v} |\n"));
    }
    s.push('\n');
    s
}

fn compress_block(rows: &[&Row]) -> String {
    let mut s = String::from("### Compression\n\n");
    s.push_str("| engine | wall ms | cpu ms | peak RSS | bytes read | output | ratio | MB/s |\n");
    s.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for r in rows {
        s.push_str(&format!(
            "| {} | {:.1} | {:.1} | {} | {} | {} | {:.1}x | {:.0} |\n",
            r.engine,
            r.wall_ms_min,
            r.cpu_ms_med,
            kb(r.max_rss_kb),
            bytes(r.logical_read_bytes),
            mb(r.out_bytes),
            r.ratio.unwrap_or(0.0),
            r.raw_bytes as f64 / 1e6 / (r.wall_ms_min / 1e3),
        ));
    }
    s.push('\n');
    s
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1e6)
}

/// `ru_maxrss` is in kibibytes, so its rendering says MiB and means it.
fn kb(k: u64) -> String {
    if k >= 1024 {
        format!("{:.1} MiB", k as f64 / 1024.0)
    } else {
        format!("{k} KiB")
    }
}

fn bytes(b: Option<u64>) -> String {
    match b {
        None => "-".into(),
        Some(v) if v >= 1_000_000 => format!("{:.1} MB", v as f64 / 1e6),
        Some(v) if v >= 1000 => format!("{:.1} kB", v as f64 / 1e3),
        Some(v) => format!("{v} B"),
    }
}
