//! A path the user typed that cannot be opened, created or removed is reported with the path in
//! the message and exit 1, in every subcommand.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::tempdir;

const BIN: &str = env!("CARGO_BIN_EXE_seekzstdsep");

/// Runs the binary and returns its exit code and stderr.
fn run(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("Failed to run the binary");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Asserts that `args` exits 1 with a message naming `path`.
fn assert_reports(args: &[&str], path: &Path) {
    let (code, stderr) = run(args);
    let cmd = args.join(" ");
    assert_eq!(code, Some(1), "`{cmd}` exited with {code:?}: {stderr}");
    assert!(
        stderr.contains(&path.display().to_string()),
        "`{cmd}` did not name {}: {stderr}",
        path.display()
    );
}

/// Writes a small jsonl file into `dir` and compresses it through the binary. The frame size is
/// small enough to make several frames, which is what `copy-range` needs to read a separator off.
fn seekable(dir: &Path) -> PathBuf {
    let raw = dir.join("records.jsonl");
    let body: String = (0..64).map(|i| format!("{{\"i\":{i}}}\n")).collect();
    fs::write(&raw, body).expect("Failed to write the input");
    let out = dir.join("records.seek.zst");
    let (code, stderr) = run(&[
        "compress",
        raw.to_str().unwrap(),
        out.to_str().unwrap(),
        "--frame-size",
        "128",
    ]);
    assert_eq!(code, Some(0), "compress failed: {stderr}");
    out
}

#[test]
fn an_input_that_does_not_open_is_reported_with_its_path() {
    let dir = tempdir().expect("Failed to create a temporary directory");
    let good = seekable(dir.path());
    let missing = dir.path().join("missing.seek.zst");
    let out = dir.path().join("out.seek.zst");
    let (g, m, o) = (
        good.to_str().unwrap(),
        missing.to_str().unwrap(),
        out.to_str().unwrap(),
    );

    assert_reports(&["cat", m, "--from", "0", "--cnt", "1"], &missing);
    assert_reports(&["compress", m, o], &missing);
    assert_reports(&["inspect", m], &missing);
    assert_reports(&["truncate", m, "--records", "0"], &missing);
    assert_reports(&["append", m], &missing);
    assert_reports(&["append", g, m], &missing);
    assert_reports(&["copy-range", m, o, "--from", "0"], &missing);
}

#[test]
fn an_output_that_does_not_open_is_reported_with_its_path() {
    let dir = tempdir().expect("Failed to create a temporary directory");
    let good = seekable(dir.path());
    let raw = dir.path().join("records.jsonl");
    let unwritable = dir.path().join("no-such-dir").join("out.seek.zst");
    let (g, r, u) = (
        good.to_str().unwrap(),
        raw.to_str().unwrap(),
        unwritable.to_str().unwrap(),
    );

    assert_reports(&["compress", r, u], &unwritable);
    assert_reports(&["copy-range", g, u, "--from", "0"], &unwritable);
}

#[test]
fn an_input_that_is_not_a_zst_is_reported_with_its_path() {
    let dir = tempdir().expect("Failed to create a temporary directory");
    let plain = dir.path().join("plain.jsonl");
    fs::write(&plain, b"{\"i\":0}\n").expect("Failed to write the input");
    let p = plain.to_str().unwrap();

    assert_reports(&["inspect", p], &plain);
    assert_reports(&["cat", p, "--from", "0", "--cnt", "1"], &plain);
}

#[test]
fn a_zst_without_a_seek_table_is_reported_with_its_path() {
    let dir = tempdir().expect("Failed to create a temporary directory");
    let plain = dir.path().join("plain.zst");
    let body = zstd::encode_all(&b"{\"i\":0}\n"[..], 3).expect("Failed to compress");
    fs::write(&plain, body).expect("Failed to write the input");
    let p = plain.to_str().unwrap();

    assert_reports(&["inspect", p], &plain);
    assert_reports(&["cat", p, "--from", "0", "--cnt", "1"], &plain);
}

#[cfg(unix)]
#[test]
fn an_input_that_does_not_get_removed_is_reported_with_its_path() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().expect("Failed to create a temporary directory");
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).expect("Failed to create the directory");
    let raw = locked.join("records.jsonl");
    fs::write(&raw, b"{\"i\":0}\n").expect("Failed to write the input");
    let out = dir.path().join("out.seek.zst");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500))
        .expect("Failed to make the directory read-only");

    assert_reports(
        &[
            "compress",
            raw.to_str().unwrap(),
            out.to_str().unwrap(),
            "--rm",
        ],
        &raw,
    );

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))
        .expect("Failed to make the directory writable again");
}

#[test]
fn a_zst_whose_seek_table_holds_no_frame_is_reported_with_its_path() {
    let dir = tempdir().expect("Failed to create a temporary directory");
    let empty = dir.path().join("no-frames.zst");
    // A seekable zst that is nothing but a seek table footer saying it has no frames: the
    // skippable frame's magic and length, then 0 frames, the descriptor and the seekable magic.
    let mut body = Vec::new();
    body.extend_from_slice(&0x184D_2A5E_u32.to_le_bytes());
    body.extend_from_slice(&9_u32.to_le_bytes());
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.push(0);
    body.extend_from_slice(&0x8F92_EAB1_u32.to_le_bytes());
    fs::write(&empty, body).expect("Failed to write the input");
    let p = empty.to_str().unwrap();

    assert_reports(&["inspect", p], &empty);
    assert_reports(&["cat", p, "--from", "0", "--cnt", "1"], &empty);
}
