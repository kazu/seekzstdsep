use seekzstdsep::{copy_and_replace, with_file_lock};
use std::{
    fs,
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
fn another_process_is_rejected_and_only_forced_exit_leaves_a_lock() {
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
        let lock_path = dir.path().join(".lock.data");
        assert!(lock_path.is_file());
        let error = copy_and_replace(&path, |_| panic!("second writer entered")).unwrap_err();
        assert!(error.to_string().contains("creating writer lock"));
        assert!(with_file_lock::<()>(&path, |_| panic!("direct writer entered")).is_err());
        if kill {
            child.kill().unwrap();
        } else {
            child.stdin.take().unwrap().write_all(b"release\n").unwrap();
        }
        let status = child.wait().unwrap();
        assert_eq!(status.success(), !kill);
        assert_eq!(lock_path.is_file(), kill);
        if kill {
            assert!(copy_and_replace(&path, |_| panic!("stale lock was ignored")).is_err());
            fs::remove_file(&lock_path).unwrap();
        }
        copy_and_replace(&path, |file| {
            file.write_all(b"updated!")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"updated!");
        assert!(!lock_path.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
