mod common;

use common::{assert_decompresses_to, compress_fixture, compress_frames, fixture_records};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::{Command, Output, Stdio},
};

const BIN: &str = env!("CARGO_BIN_EXE_seekzstdsep");

fn run(path: &Path, input: &Path, flags: &[&str]) -> Output {
    Command::new(BIN)
        .arg("append")
        .arg(path)
        .arg(input)
        .args(flags)
        .output()
        .unwrap()
}

fn succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn copy_flag_publishes_append_without_changing_an_open_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let before = fs::read(&path).unwrap();
    let mut reader = File::open(&path).unwrap();
    let input = dir.path().join("input");
    fs::write(&input, b"added\n").unwrap();
    succeeded(&run(&path, &input, &["--copy"]));
    let mut held = Vec::new();
    reader.read_to_end(&mut held).unwrap();
    assert_eq!(held, before);
    assert_decompresses_to(
        &path,
        &[fixture_records().concat(), b"added\n".to_vec()].concat(),
    );
}

#[test]
fn copy_flag_accepts_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let mut child = Command::new(BIN)
        .arg("append")
        .arg(&path)
        .arg("--copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"stdin\n").unwrap();
    succeeded(&child.wait_with_output().unwrap());
    assert_decompresses_to(
        &path,
        &[fixture_records().concat(), b"stdin\n".to_vec()].concat(),
    );
}

#[test]
fn copy_and_direct_keep_existing_finder_frame_and_refusal_behavior() {
    for (body, flags) in [
        (
            b"record 7\n".as_slice(),
            vec!["--finder", "fixed", "--finder-arg", "9"],
        ),
        (b"record 7\n".as_slice(), vec!["--level", "1"]),
        (b"record 7\n".as_slice(), vec!["--separator", "absent"]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let groups = vec![b"record 1\nrecord 2\n".to_vec(); 3];
        let path = compress_frames(dir.path(), "copy", &groups);
        let direct = dir.path().join("direct");
        fs::copy(&path, &direct).unwrap();
        let input = dir.path().join("input");
        fs::write(&input, body).unwrap();
        let expected = run(&direct, &input, &flags);
        let mut copy_flags = flags.clone();
        copy_flags.push("--copy");
        let actual = run(&path, &input, &copy_flags);
        assert_eq!(actual.status.success(), expected.status.success());
        if !actual.status.success() {
            assert!(String::from_utf8_lossy(&actual.stderr).contains("Error:"));
        }
        assert_eq!(fs::read(&path).unwrap(), fs::read(&direct).unwrap());
    }
    let dir = tempfile::tempdir().unwrap();
    let groups = vec![b"record 1\nrecord 2\n".to_vec(); 3];
    let path = compress_frames(dir.path(), "copy", &groups);
    let input = compress_frames(dir.path(), "input", &groups);
    let direct = dir.path().join("direct");
    fs::copy(&path, &direct).unwrap();
    let flags = [
        "--input-seekable",
        "--input-from",
        "2",
        "--input-cnt",
        "2",
        "--check-input-frames",
    ];
    succeeded(&run(&direct, &input, &flags));
    let mut copy_flags = flags.to_vec();
    copy_flags.push("--copy");
    succeeded(&run(&path, &input, &copy_flags));
    assert_eq!(fs::read(path).unwrap(), fs::read(direct).unwrap());
}

#[test]
fn copy_flag_keeps_fragment_refusal_and_separator_insertion() {
    for insert in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = compress_frames(
            dir.path(),
            "target",
            &[b"a\nb\n".to_vec(), b"c\nd\n".to_vec(), b"fragment".to_vec()],
        );
        let before = fs::read(&path).unwrap();
        let input = dir.path().join("input");
        fs::write(&input, b"added\n").unwrap();
        let flags = if insert {
            vec!["--copy", "--insert-separator"]
        } else {
            vec!["--copy"]
        };
        let result = run(&path, &input, &flags);
        if insert {
            succeeded(&result);
            assert_decompresses_to(&path, b"a\nb\nc\nd\nfragment\nadded\n");
        } else {
            assert!(!result.status.success());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }
}

#[test]
fn cli_copy_cooperates_with_a_library_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let input = dir.path().join("input");
    fs::write(&input, b"direct\n").unwrap();
    let err = seekzstdsep::copy_and_replace(&path, |_| {
        succeeded(&run(&path, &input, &["--copy"]));
        Ok(())
    })
    .unwrap_err();
    assert!(err.to_string().contains("conflict"));
    let lock = path.with_file_name(format!(
        ".{}.seekzstdsep.lock",
        path.file_name().unwrap().to_str().unwrap()
    ));
    assert!(lock.is_file());
    assert_decompresses_to(
        &path,
        &[fixture_records().concat(), b"direct\n".to_vec()].concat(),
    );
}

#[cfg(target_os = "linux")]
#[test]
fn copy_creation_failure_reports_direct_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let original = compress_fixture(dir.path());
    let mut parent = dir.path().canonicalize().unwrap();
    for _ in 0..30 {
        let remaining = 4030 - parent.as_os_str().len();
        if remaining == 0 {
            break;
        }
        parent.push("d".repeat((remaining - 1).min(200)));
        fs::create_dir(&parent).unwrap();
    }
    assert_eq!(parent.as_os_str().len(), 4030);
    let path = parent.join("x");
    fs::copy(&original, &path).unwrap();
    let input = dir.path().join("input");
    fs::write(&input, b"fallback\n").unwrap();
    let output = run(&path, &input, &["--copy"]);
    succeeded(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("appended directly"));
    assert_decompresses_to(
        &path,
        &[fixture_records().concat(), b"fallback\n".to_vec()].concat(),
    );
}
