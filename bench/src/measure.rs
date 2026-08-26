//! Process-level measurement.
//!
//! Every case under test is run as one or more child processes of the harness, so that a
//! single-process command and a two- or three-process pipeline are accounted for the same way.
//! The harness builds the pipeline itself rather than handing it to a shell, which keeps every
//! stage a direct child and therefore individually measurable.
//!
//! Aggregation rule, applied identically to every baseline:
//!
//! - wall time: from just before the first `fork` to after the last child is reaped
//! - CPU time: `ru_utime + ru_stime`, **summed** over the stages
//! - peak RSS: `ru_maxrss`, **maximum** over the stages (not summed; no kernel interface reports a
//!   true simultaneous sum for a process group without cgroup delegation)
//! - block-layer bytes: `ru_inblock * 512`, **summed** over the stages. Non-zero only on a cold
//!   cache, which is exactly what it is for
//! - logical bytes read: collected in a separate, untimed `strace` pass; see [`io_bytes`]

use anyhow::{Result, bail};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// One stage of a pipeline.
#[derive(Clone, Debug)]
pub struct Proc {
    pub argv: Vec<String>,
}

impl Proc {
    pub fn new<S: Into<String>, I: IntoIterator<Item = S>>(argv: I) -> Self {
        Proc {
            argv: argv.into_iter().map(Into::into).collect(),
        }
    }
}

/// Where the last stage's stdout goes.
#[derive(Clone, Debug)]
pub enum Sink {
    Null,
    File(PathBuf),
}

/// A complete case to measure: a pipeline of one or more stages.
#[derive(Clone, Debug)]
pub struct Run {
    pub procs: Vec<Proc>,
    pub sink: Sink,
    /// CPU to pin every stage to, so that a pipeline cannot use more of the machine than a single
    /// process can. Without this a two- or three-process baseline overlaps its stages and finishes
    /// in less elapsed time than its own CPU time, which is not a comparison against a
    /// single-threaded engine — it is a comparison against a different amount of hardware.
    pub pin_cpu: Option<usize>,
}

impl Run {
    pub fn single(argv: Vec<String>) -> Self {
        Run {
            procs: vec![Proc::new(argv)],
            sink: Sink::Null,
            pin_cpu: None,
        }
    }
    pub fn pipeline(stages: Vec<Vec<String>>) -> Self {
        Run {
            procs: stages.into_iter().map(Proc::new).collect(),
            sink: Sink::Null,
            pin_cpu: None,
        }
    }
    pub fn to_shell(&self) -> String {
        self.procs
            .iter()
            .map(|p| p.argv.join(" "))
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub wall_ns: u64,
    pub user_ns: u64,
    pub sys_ns: u64,
    /// Maximum over the stages, in kibibytes.
    pub max_rss_kb: u64,
    /// Summed over the stages.
    pub blk_read_bytes: u64,
    /// Raw wait status per stage, in pipeline order.
    pub statuses: Vec<i32>,
}

impl Metrics {
    pub fn cpu_ns(&self) -> u64 {
        self.user_ns + self.sys_ns
    }
    /// A stage may legitimately die of `SIGPIPE` when a later stage exits early; that is how
    /// `zstd -dc | tail | head` terminates. Only the last stage has to exit cleanly.
    pub fn ok(&self) -> bool {
        let Some((last, rest)) = self.statuses.split_last() else {
            return false;
        };
        let exited_zero = |s: &i32| libc::WIFEXITED(*s) && libc::WEXITSTATUS(*s) == 0;
        let sigpipe = |s: &i32| libc::WIFSIGNALED(*s) && libc::WTERMSIG(*s) == libc::SIGPIPE;
        exited_zero(last) && rest.iter().all(|s| exited_zero(s) || sigpipe(s))
    }
}

fn tv_ns(tv: libc::timeval) -> u64 {
    (tv.tv_sec as u64) * 1_000_000_000 + (tv.tv_usec as u64) * 1_000
}

fn cstr(s: &str) -> Result<CString> {
    Ok(CString::new(s)?)
}

fn open_path(p: &Path, flags: i32, mode: libc::mode_t) -> Result<i32> {
    let c = CString::new(p.as_os_str().as_bytes())?;
    let fd = unsafe { libc::open(c.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 {
        bail!("open {}: {}", p.display(), std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// Runs `run` once and returns its aggregated metrics.
pub fn run_once(run: &Run) -> Result<Metrics> {
    let n = run.procs.len();
    if n == 0 {
        bail!("empty run");
    }

    // Everything that can allocate happens before the fork.
    let owned: Vec<Vec<CString>> = run
        .procs
        .iter()
        .map(|p| p.argv.iter().map(|a| cstr(a)).collect::<Result<Vec<_>>>())
        .collect::<Result<Vec<_>>>()?;
    let ptrs: Vec<Vec<*const libc::c_char>> = owned
        .iter()
        .map(|v| {
            let mut p: Vec<*const libc::c_char> = v.iter().map(|c| c.as_ptr()).collect();
            p.push(std::ptr::null());
            p
        })
        .collect();

    let devnull = Path::new("/dev/null");
    let null_r = open_path(devnull, libc::O_RDONLY, 0)?;
    let null_w = open_path(devnull, libc::O_WRONLY, 0)?;
    let sink_fd = match &run.sink {
        Sink::Null => null_w,
        Sink::File(p) => open_path(p, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644)?,
    };

    let mut pipes: Vec<(i32, i32)> = Vec::with_capacity(n.saturating_sub(1));
    for _ in 0..n.saturating_sub(1) {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            bail!("pipe: {}", std::io::Error::last_os_error());
        }
        pipes.push((fds[0], fds[1]));
    }

    // fds the child must not inherit beyond its own stdio.
    let mut to_close: Vec<i32> = vec![null_r, null_w];
    if sink_fd != null_w {
        to_close.push(sink_fd);
    }
    for (r, w) in &pipes {
        to_close.push(*r);
        to_close.push(*w);
    }

    let t0 = Instant::now();
    let mut pids: Vec<libc::pid_t> = Vec::with_capacity(n);
    for i in 0..n {
        let in_fd = if i == 0 { null_r } else { pipes[i - 1].0 };
        let out_fd = if i == n - 1 { sink_fd } else { pipes[i].1 };
        let pid = unsafe { spawn(&ptrs[i], in_fd, out_fd, null_w, &to_close, run.pin_cpu) };
        if pid < 0 {
            bail!("fork: {}", std::io::Error::last_os_error());
        }
        pids.push(pid);
    }

    for (r, w) in &pipes {
        unsafe {
            libc::close(*r);
            libc::close(*w);
        }
    }

    let mut m = Metrics::default();
    for pid in &pids {
        let mut status: i32 = 0;
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::wait4(*pid, &mut status, 0, &mut ru) };
        if r < 0 {
            bail!("wait4: {}", std::io::Error::last_os_error());
        }
        m.user_ns += tv_ns(ru.ru_utime);
        m.sys_ns += tv_ns(ru.ru_stime);
        m.max_rss_kb = m.max_rss_kb.max(ru.ru_maxrss.max(0) as u64);
        m.blk_read_bytes += (ru.ru_inblock.max(0) as u64) * 512;
        m.statuses.push(status);
    }
    m.wall_ns = t0.elapsed().as_nanos() as u64;

    unsafe {
        libc::close(null_r);
        libc::close(null_w);
        if sink_fd != null_w {
            libc::close(sink_fd);
        }
    }
    Ok(m)
}

/// Between `fork` and `execvp` only async-signal-safe calls are made, and nothing allocates.
unsafe fn spawn(
    argv: &[*const libc::c_char],
    in_fd: i32,
    out_fd: i32,
    err_fd: i32,
    to_close: &[i32],
    pin_cpu: Option<usize>,
) -> libc::pid_t {
    let pid = unsafe { libc::fork() };
    if pid != 0 {
        return pid;
    }
    unsafe {
        // Set before exec so it is inherited, and so any thread the program starts later lands on
        // the same CPU. `sched_setaffinity` is a plain syscall and safe to call after fork.
        if let Some(cpu) = pin_cpu {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(cpu, &mut set);
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        }
        libc::dup2(in_fd, 0);
        libc::dup2(out_fd, 1);
        libc::dup2(err_fd, 2);
        for fd in to_close {
            if *fd > 2 {
                libc::close(*fd);
            }
        }
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::execvp(argv[0], argv.as_ptr());
        libc::_exit(127);
    }
}

/// Logical bytes read from `targets`, measured in a separate untimed `strace` pass.
///
/// Each stage is traced by its own `strace`, and counted are successful
/// `read`/`pread64`/`readv`/`preadv` returns on file descriptors whose `-y` annotation names one of
/// `targets`. Reads from pipes are therefore excluded, which is what makes the number comparable
/// between a single process and a pipeline.
///
/// `-f` is required, not optional: zstd 1.5.4 and later read their input on a separate asyncio
/// thread, so without it the input reads of `zstd -dc` are invisible and the baseline silently
/// reports zero bytes read. Following threads means the log interleaves, so the parser has to
/// rejoin `<unfinished ...>` with `<... read resumed>`.
///
/// Anything read through `mmap` stays invisible. `rg` is invoked with `--no-mmap` for that reason;
/// nothing else under test maps its input.
pub fn io_bytes(run: &Run, targets: &[PathBuf]) -> Result<u64> {
    let dir = std::env::temp_dir().join(format!("szbench-strace-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let mut traced = run.clone();
    let mut logs = Vec::new();
    for (i, p) in traced.procs.iter_mut().enumerate() {
        let log = dir.join(format!("t{i}.log"));
        let mut argv: Vec<String> = vec![
            "strace".into(),
            "-f".into(),
            "-qq".into(),
            "-s".into(),
            "0".into(),
            "-y".into(),
            "-e".into(),
            "signal=none".into(),
            "-e".into(),
            "trace=read,pread64,readv,preadv".into(),
            "-o".into(),
            log.to_string_lossy().into_owned(),
            "--".into(),
        ];
        argv.extend(p.argv.iter().cloned());
        p.argv = argv;
        logs.push(log);
    }
    run_once(&traced)?;

    let names: Vec<String> = targets
        .iter()
        .map(|t| t.to_string_lossy().into_owned())
        .collect();
    let mut total = 0u64;
    for log in &logs {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        total += parse_trace(&text, &names);
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(total)
}

const READ_CALLS: [&str; 4] = ["read(", "pread64(", "readv(", "preadv("];

fn parse_trace(text: &str, targets: &[String]) -> u64 {
    let mut total = 0u64;
    // tid -> path of the file a suspended read was issued against.
    let mut pending: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();

    for line in text.lines() {
        let tid = line.split_whitespace().next().unwrap_or("");

        if line.contains("resumed>") {
            let Some(path) = pending.remove(tid) else {
                continue;
            };
            if targets.iter().any(|t| t == path)
                && let Some(n) = ret_value(line)
            {
                total += n;
            }
            continue;
        }

        let Some(open) = READ_CALLS
            .iter()
            .filter_map(|name| find_call(line, name))
            .min()
        else {
            continue;
        };
        // `read(3</path/to/file>, ""..., 65536) = 65536`
        let rest = &line[open + 1..];
        let Some(lt) = rest.find('<') else { continue };
        let Some(gt) = rest.find('>') else { continue };
        if gt < lt || !rest[..lt].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let path = &rest[lt + 1..gt];

        if line.ends_with("<unfinished ...>") {
            pending.insert(tid, path);
            continue;
        }
        if targets.iter().any(|t| t == path)
            && let Some(n) = ret_value(line)
        {
            total += n;
        }
    }
    total
}

fn ret_value(line: &str) -> Option<u64> {
    let eq = line.rfind(" = ")?;
    let tok = line[eq + 3..].split_whitespace().next()?;
    match tok.parse::<i64>() {
        Ok(n) if n > 0 => Some(n as u64),
        _ => None,
    }
}

/// Position of the `(` of a syscall named `name` (which includes the paren), rejecting matches
/// that are the tail of a longer identifier such as `pread64` inside `read`.
fn find_call(line: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(idx) = line[from..].find(name) {
        let at = from + idx;
        let prev_ok = at == 0
            || !line.as_bytes()[at - 1].is_ascii_alphanumeric() && line.as_bytes()[at - 1] != b'_';
        if prev_ok {
            return Some(at + name.len() - 1);
        }
        from = at + 1;
    }
    None
}

/// Evicts `paths` from the page cache without disturbing the rest of the machine.
///
/// This is per-file eviction, not a global `drop_caches`; see [`drop_caches`] for that. It works
/// on the NFS mount too, since the client caches pages the same way.
pub fn evict(paths: &[PathBuf]) -> Result<()> {
    for p in paths {
        if !p.exists() {
            continue;
        }
        let fd = open_path(p, libc::O_RDONLY, 0)?;
        unsafe {
            libc::fsync(fd);
            libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
            libc::close(fd);
        }
    }
    Ok(())
}

/// Global page cache drop. Needs root; disturbs everything else running on the machine.
pub fn drop_caches() -> Result<()> {
    let st = std::process::Command::new("sudo")
        .args(["-n", "sh", "-c", "sync; echo 3 > /proc/sys/vm/drop_caches"])
        .status()?;
    if !st.success() {
        bail!("drop_caches failed: {st}");
    }
    Ok(())
}
