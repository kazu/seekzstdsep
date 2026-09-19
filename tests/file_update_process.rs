use fs2::FileExt;
use seekzstdsep::{copy_and_replace, with_file_lock};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
};

#[test]
fn lock_child() {
    let Some(path) = std::env::var_os("SEEKZSTDSEP_LOCK_TEST_TARGET") else {
        return;
    };
    with_file_lock(path, |_| {
        println!("LOCK_HELD");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn another_process_is_excluded_and_exit_does_not_leave_a_stale_lock() {
    for kill in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        fs::write(&path, b"original").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lock_child", "--nocapture"])
            .env("SEEKZSTDSEP_LOCK_TEST_TARGET", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let held = output
            .by_ref()
            .lines()
            .any(|line| line.unwrap() == "LOCK_HELD");
        assert!(held, "child did not acquire its lock");
        let lock_path = dir.path().join(".data.seekzstdsep.lock");
        let lock = File::options()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let err = FileExt::try_lock_exclusive(&lock).unwrap_err();
        assert_eq!(err.kind(), fs2::lock_contended_error().kind());
        if kill {
            child.kill().unwrap();
        } else {
            child.stdin.take().unwrap().write_all(b"release\n").unwrap();
        }
        let status = child.wait().unwrap();
        assert_eq!(status.success(), !kill);
        assert!(lock_path.is_file());
        FileExt::try_lock_exclusive(&lock).unwrap();
        FileExt::unlock(&lock).unwrap();
        copy_and_replace(&path, |file| {
            file.write_all(b"updated!")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"updated!");
        assert!(lock_path.is_file());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}
