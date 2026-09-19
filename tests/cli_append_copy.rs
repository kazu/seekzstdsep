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
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
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
fn cli_append_and_truncate_are_rejected_while_a_library_copy_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let input = dir.path().join("input");
    fs::write(&input, b"direct\n").unwrap();
    seekzstdsep::copy_and_replace(&path, |_| {
        let output = run(&path, &input, &["--copy"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("creating writer lock"));
        let output = Command::new(BIN)
            .arg("truncate")
            .arg(&path)
            .args(["--records", "234", "--copy"])
            .output()?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("creating writer lock"));
        Ok(())
    })
    .unwrap();
    assert_decompresses_to(&path, &fixture_records().concat());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn copy_creation_failure_is_an_error_without_direct_update() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let before = fs::read(&path).unwrap();
    let temp = path.with_file_name(format!(
        ".tmp.{}",
        path.file_name().unwrap().to_str().unwrap()
    ));
    fs::write(&temp, b"leftover copy").unwrap();
    let input = dir.path().join("input");
    fs::write(&input, b"added\n").unwrap();
    let output = run(&path, &input, &["--copy"]);
    assert!(!output.status.success());
    assert_eq!(fs::read(&path).unwrap(), before);
    let output = Command::new(BIN)
        .arg("truncate")
        .arg(&path)
        .args(["--records", "234", "--copy"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&temp).unwrap(), b"leftover copy");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    fs::remove_file(temp).unwrap();
    let output = Command::new(BIN)
        .arg("truncate")
        .arg(&path)
        .args(["--records", "234", "--copy"])
        .output()
        .unwrap();
    succeeded(&output);
    assert_decompresses_to(&path, &fixture_records()[..234].concat());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}
