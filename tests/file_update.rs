#[allow(dead_code)]
#[path = "../src/file_update.rs"]
mod file_update;

use file_update::{AppendMode, FileOps, System, append_copy_using, with_file_lock_using};
use fs2::FileExt;
use std::{
    cell::Cell,
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::mpsc,
};

struct Fixture {
    dir: tempfile::TempDir,
    path: PathBuf,
    lock: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let lock = dir.path().join(".data.seekzstdsep.lock");
        fs::write(&path, b"original").unwrap();
        Self { dir, path, lock }
    }

    fn clean(&self) {
        assert!(self.lock.is_file());
        assert_eq!(fs::read_dir(self.dir.path()).unwrap().count(), 2);
    }

    fn locked(&self) {
        let file = File::options()
            .read(true)
            .write(true)
            .open(&self.lock)
            .unwrap();
        let error = FileExt::try_lock_exclusive(&file).unwrap_err();
        assert_eq!(error.kind(), fs2::lock_contended_error().kind());
    }

    fn unlocked(&self) {
        let file = File::options()
            .read(true)
            .write(true)
            .open(&self.lock)
            .unwrap();
        FileExt::try_lock_exclusive(&file).unwrap();
    }
}

fn injected() -> io::Error {
    io::Error::other("injected failure")
}

#[derive(Clone, Copy)]
enum Failure {
    None,
    Temp,
    Copy,
    FirstHash,
    LastHash,
    FirstLock,
    LastLock,
    Unlock,
    Rename,
}

struct Probe<'a> {
    fixture: &'a Fixture,
    failure: Failure,
    locks: Cell<usize>,
    hashes: Cell<usize>,
    copies: Cell<usize>,
    ordinary: Cell<usize>,
    renames: Cell<usize>,
}

impl<'a> Probe<'a> {
    fn new(fixture: &'a Fixture, failure: Failure) -> Self {
        Self {
            fixture,
            failure,
            locks: Cell::new(0),
            hashes: Cell::new(0),
            copies: Cell::new(0),
            ordinary: Cell::new(0),
            renames: Cell::new(0),
        }
    }
}

impl FileOps for Probe<'_> {
    fn lock(&self, file: &File) -> io::Result<()> {
        let n = self.locks.get() + 1;
        self.locks.set(n);
        if matches!(
            (self.failure, n),
            (Failure::FirstLock, 1) | (Failure::LastLock, 2)
        ) {
            return Err(injected());
        }
        System.lock(file)
    }

    fn unlock(&self, file: &File) -> io::Result<()> {
        if matches!(self.failure, Failure::Unlock) {
            return Err(injected());
        }
        System.unlock(file)
    }

    fn hash(&self, file: &mut File) -> io::Result<String> {
        self.fixture.locked();
        let n = self.hashes.get() + 1;
        self.hashes.set(n);
        if matches!(
            (self.failure, n),
            (Failure::FirstHash, 1) | (Failure::LastHash, 2)
        ) {
            return Err(injected());
        }
        System.hash(file)
    }

    fn create_copy(&self, path: &Path, hash: &str) -> io::Result<tempfile::NamedTempFile> {
        self.fixture.locked();
        if matches!(self.failure, Failure::Temp) {
            return Err(injected());
        }
        let temp = System.create_copy(path, hash)?;
        assert_eq!(temp.path().parent(), path.parent());
        assert!(
            temp.path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains(hash)
        );
        Ok(temp)
    }

    fn reflink(&self, _from: &File, mut to: &File) -> io::Result<()> {
        self.fixture.locked();
        self.copies.set(self.copies.get() + 1);
        to.write_all(b"partially failed reflink with extra trailing bytes")?;
        Err(injected())
    }

    fn ordinary_copy(&self, from: &mut File, to: &mut File) -> io::Result<()> {
        self.fixture.locked();
        self.ordinary.set(self.ordinary.get() + 1);
        if matches!(self.failure, Failure::Copy) {
            from.read_exact(&mut [0; 3])?;
            return Err(injected());
        }
        System.ordinary_copy(from, to)
    }

    fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.fixture.locked();
        assert_eq!(self.hashes.get(), 2);
        self.renames.set(self.renames.get() + 1);
        if matches!(self.failure, Failure::Rename) {
            return Err(injected());
        }
        System.replace(from, to)
    }
}

#[test]
fn ordinary_copy_clears_a_partial_reflink_and_all_critical_intervals_are_locked() {
    let fixture = Fixture::new();
    let probe = Probe::new(&fixture, Failure::None);
    let mode = append_copy_using(
        &fixture.path,
        |file| {
            fixture.unlocked();
            assert_eq!(file.stream_position()?, 0);
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            assert_eq!(content, "original");
            file.write_all(b" added")?;
            Ok(())
        },
        &probe,
    )
    .unwrap();
    assert_eq!(mode, AppendMode::Replaced);
    assert_eq!(
        (
            probe.copies.get(),
            probe.ordinary.get(),
            probe.renames.get()
        ),
        (1, 1, 1)
    );
    assert_eq!(fs::read(&fixture.path).unwrap(), b"original added");
    fixture.clean();
    fixture.unlocked();
}

#[test]
fn only_copy_failures_fall_back_with_the_unconsumed_input_and_lock() {
    for failure in [Failure::Temp, Failure::Copy] {
        let fixture = Fixture::new();
        let probe = Probe::new(&fixture, failure);
        let mut input = io::Cursor::new(b"added");
        let calls = Cell::new(0);
        let mode = append_copy_using(
            &fixture.path,
            |file| {
                calls.set(calls.get() + 1);
                fixture.locked();
                assert_eq!(input.position(), 0);
                assert_eq!(file.stream_position()?, 0);
                file.seek(io::SeekFrom::End(0))?;
                io::copy(&mut input, file)?;
                Ok(())
            },
            &probe,
        )
        .unwrap();
        assert_eq!(mode, AppendMode::Direct);
        assert_eq!(calls.get(), 1);
        assert_eq!(probe.renames.get(), 0);
        assert_eq!(fs::read(&fixture.path).unwrap(), b"originaladded");
        fixture.clean();
        fixture.unlocked();
    }
}

#[test]
fn hash_lock_unlock_and_rename_failures_never_run_a_direct_fallback() {
    for failure in [
        Failure::FirstHash,
        Failure::LastHash,
        Failure::FirstLock,
        Failure::LastLock,
        Failure::Unlock,
        Failure::Rename,
    ] {
        let fixture = Fixture::new();
        let probe = Probe::new(&fixture, failure);
        let calls = Cell::new(0);
        let result = append_copy_using(
            &fixture.path,
            |file| {
                fixture.unlocked();
                calls.set(calls.get() + 1);
                file.write_all(b"changed!")?;
                Ok(())
            },
            &probe,
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("injected failure"));
        assert!(calls.get() <= 1);
        assert_eq!(fs::read(&fixture.path).unwrap(), b"original");
        fixture.clean();
        fixture.unlocked();
    }
}

#[test]
fn final_hash_reopens_the_path_instead_of_hashing_the_retired_handle() {
    let fixture = Fixture::new();
    let probe = Probe::new(&fixture, Failure::None);
    let result = append_copy_using(
        &fixture.path,
        |file| {
            file_update::append_copy(&fixture.path, |current| {
                current.write_all(b"new data")?;
                Ok(())
            })?;
            file.write_all(b"stale!!!")?;
            Ok(())
        },
        &probe,
    );
    assert!(result.unwrap_err().to_string().contains("conflict"));
    assert_eq!(probe.renames.get(), 0);
    assert_eq!(fs::read(&fixture.path).unwrap(), b"new data");
    fixture.clean();
}

#[test]
fn a_failed_direct_callback_is_not_retried_or_rolled_back() {
    let fixture = Fixture::new();
    let probe = Probe::new(&fixture, Failure::Copy);
    let mut calls = 0;
    let result = append_copy_using(
        &fixture.path,
        |file| {
            calls += 1;
            fixture.locked();
            file.write_all(b"partial!")?;
            anyhow::bail!("callback failure")
        },
        &probe,
    );
    assert!(result.unwrap_err().to_string().contains("callback failure"));
    assert_eq!(calls, 1);
    assert_eq!(fs::read(&fixture.path).unwrap(), b"partial!");
    fixture.clean();
    fixture.unlocked();
}

#[test]
fn direct_and_copy_open_the_target_only_after_obtaining_the_lock() {
    struct Waiting(mpsc::Sender<()>, mpsc::Receiver<()>);
    impl FileOps for Waiting {
        fn lock(&self, file: &File) -> io::Result<()> {
            self.0.send(()).unwrap();
            self.1.recv().unwrap();
            System.lock(file)
        }
    }
    for copy in [false, true] {
        let fixture = Fixture::new();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        std::thread::scope(|s| {
            let handle = s.spawn(|| {
                let ops = Waiting(ready_tx, go_rx);
                let check = |file: &mut File| {
                    let mut content = String::new();
                    file.read_to_string(&mut content)?;
                    assert_eq!(content, "replacement");
                    anyhow::bail!("checked current file")
                };
                if copy {
                    append_copy_using(&fixture.path, check, &ops).map(|_| ())
                } else {
                    with_file_lock_using(&fixture.path, check, &ops)
                }
            });
            ready_rx.recv().unwrap();
            file_update::with_file_lock(&fixture.path, |_| {
                let replacement = fixture.dir.path().join("replacement");
                fs::write(&replacement, b"replacement")?;
                fs::rename(replacement, &fixture.path)?;
                Ok(())
            })
            .unwrap();
            go_tx.send(()).unwrap();
            assert!(
                handle
                    .join()
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("checked current file")
            );
        });
        fixture.clean();
    }
}

#[test]
fn separate_targets_can_be_updated_while_another_target_is_locked() {
    let a = Fixture::new();
    let b = Fixture::new();
    file_update::with_file_lock(&a.path, |_| {
        file_update::append_copy(&b.path, |file| {
            file.write_all(b"other!!!")?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    assert_eq!(fs::read(&b.path).unwrap(), b"other!!!");
}

#[test]
fn full_content_hash_detects_changes_with_the_same_size_and_mtime() {
    let fixture = Fixture::new();
    let times = fs::metadata(&fixture.path).unwrap();
    let err = file_update::append_copy(&fixture.path, |_| {
        file_update::with_file_lock(&fixture.path, |file| {
            file.write_all(b"changed!")?;
            file.sync_all()?;
            file.set_times(fs::FileTimes::new().set_modified(times.modified()?))?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap_err();
    assert!(err.to_string().contains("conflict"));
    assert_eq!(
        fs::metadata(&fixture.path).unwrap().modified().unwrap(),
        times.modified().unwrap()
    );
    assert_eq!(fs::read(&fixture.path).unwrap(), b"changed!");
    fixture.clean();
}

#[cfg(unix)]
#[test]
fn permissions_owner_and_group_survive_replacement() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&fixture.path).unwrap();
    file_update::append_copy(&fixture.path, |_| Ok(())).unwrap();
    let after = fs::metadata(&fixture.path).unwrap();
    assert_eq!(
        (before.uid(), before.gid(), before.mode()),
        (after.uid(), after.gid(), after.mode())
    );
    assert_ne!(before.ino(), after.ino());
    fixture.clean();
}

#[cfg(unix)]
#[test]
fn symlinks_and_hardlinks_are_refused_without_touching_their_contents() {
    for hard in [false, true] {
        let fixture = Fixture::new();
        let alias = fixture.dir.path().join("alias");
        if hard {
            fs::hard_link(&fixture.path, &alias).unwrap();
        } else {
            std::os::unix::fs::symlink(&fixture.path, &alias).unwrap();
        }
        let result = file_update::append_copy(&alias, |_| panic!("invalid target accepted"));
        assert!(result.is_err());
        assert_eq!(fs::read(&fixture.path).unwrap(), b"original");
    }
}

#[cfg(unix)]
#[test]
fn parent_directory_aliases_share_the_fixed_lock() {
    let fixture = Fixture::new();
    let other = tempfile::tempdir().unwrap();
    let alias = other.path().join("alias");
    std::os::unix::fs::symlink(fixture.dir.path(), &alias).unwrap();
    file_update::with_file_lock(alias.join("data"), |_| {
        fixture.locked();
        Ok(())
    })
    .unwrap();
    fixture.clean();
}

#[test]
fn actual_reflink_success_does_not_invoke_ordinary_copy() {
    struct ReflinkOnly;
    impl FileOps for ReflinkOnly {
        fn ordinary_copy(&self, _: &mut File, _: &mut File) -> io::Result<()> {
            panic!("ordinary copy called after successful reflink")
        }
    }
    let fixture = Fixture::new();
    let source = File::open(&fixture.path).unwrap();
    let destination = tempfile::tempfile().unwrap();
    if let Err(error) = System.reflink(&source, &destination) {
        eprintln!("reflink not supported on test filesystem: {error}");
        return;
    }
    drop(source);
    assert_eq!(
        append_copy_using(&fixture.path, |_| Ok(()), &ReflinkOnly).unwrap(),
        AppendMode::Replaced
    );
    assert_eq!(fs::read(&fixture.path).unwrap(), b"original");
    fixture.clean();
}
