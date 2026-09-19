#[allow(dead_code)]
#[path = "../src/file_update.rs"]
mod file_update;

use file_update::{CopyMode, FileOps, System, copy_and_replace_using};
use std::{
    cell::Cell,
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
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
        let lock = dir.path().join(".lock.data");
        fs::write(&path, b"original").unwrap();
        Self { dir, path, lock }
    }

    fn clean(&self) {
        assert!(!self.lock.exists());
        assert_eq!(fs::read_dir(self.dir.path()).unwrap().count(), 1);
    }

    fn locked(&self) {
        let error = File::options()
            .write(true)
            .create_new(true)
            .open(&self.lock)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
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
    Rename,
}

struct Probe<'a> {
    fixture: &'a Fixture,
    failure: Failure,
    copies: Cell<usize>,
    ordinary: Cell<usize>,
    renames: Cell<usize>,
}

impl<'a> Probe<'a> {
    fn new(fixture: &'a Fixture, failure: Failure) -> Self {
        Self {
            fixture,
            failure,
            copies: Cell::new(0),
            ordinary: Cell::new(0),
            renames: Cell::new(0),
        }
    }
}

impl FileOps for Probe<'_> {
    fn create_copy(&self, path: &Path) -> io::Result<tempfile::NamedTempFile> {
        self.fixture.locked();
        if matches!(self.failure, Failure::Temp) {
            return Err(injected());
        }
        let temp = System.create_copy(path)?;
        assert_eq!(temp.path(), path.with_file_name(".tmp.data"));
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
        self.renames.set(self.renames.get() + 1);
        if matches!(self.failure, Failure::Rename) {
            return Err(injected());
        }
        System.replace(from, to)
    }
}

#[test]
fn ordinary_copy_clears_a_partial_reflink_and_the_entire_update_is_locked() {
    let fixture = Fixture::new();
    let probe = Probe::new(&fixture, Failure::None);
    let mode = copy_and_replace_using(
        &fixture.path,
        |file| {
            fixture.locked();
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
    assert_eq!(mode, CopyMode::Replaced);
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
}

#[test]
fn copy_and_rename_failures_leave_the_original_and_remove_owned_files() {
    for failure in [Failure::Temp, Failure::Copy, Failure::Rename] {
        let fixture = Fixture::new();
        let probe = Probe::new(&fixture, failure);
        let calls = Cell::new(0);
        let result = copy_and_replace_using(
            &fixture.path,
            |file| {
                calls.set(calls.get() + 1);
                fixture.locked();
                file.write_all(b"changed!")?;
                Ok(())
            },
            &probe,
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("injected failure"));
        assert_eq!(calls.get(), usize::from(matches!(failure, Failure::Rename)));
        assert_eq!(fs::read(&fixture.path).unwrap(), b"original");
        fixture.clean();
    }
}

#[test]
fn existing_lock_is_rejected_without_touching_files_or_calling_update() {
    let fixture = Fixture::new();
    let temp = fixture.dir.path().join(".tmp.data");
    fs::write(&fixture.lock, b"another writer").unwrap();
    fs::write(&temp, b"another copy").unwrap();
    let update = |_: &mut File| -> anyhow::Result<()> { panic!("lock was ignored") };
    for result in [
        file_update::with_file_lock(&fixture.path, update),
        file_update::copy_and_replace(&fixture.path, update).map(|_| ()),
    ] {
        let error = result.unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
    }
    assert_eq!(fs::read(&fixture.lock).unwrap(), b"another writer");
    assert_eq!(fs::read(temp).unwrap(), b"another copy");
    assert_eq!(fs::read(&fixture.path).unwrap(), b"original");
}

#[test]
fn existing_temporary_file_is_preserved_and_our_lock_is_removed() {
    let fixture = Fixture::new();
    let temp = fixture.dir.path().join(".tmp.data");
    fs::write(&temp, b"leftover copy").unwrap();
    let error = file_update::copy_and_replace(&fixture.path, |_| panic!("temp was overwritten"))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert!(!fixture.lock.exists());
    assert_eq!(fs::read(temp).unwrap(), b"leftover copy");
    assert_eq!(fs::read(&fixture.path).unwrap(), b"original");
}

#[test]
fn missing_target_does_not_leave_a_lock() {
    for copy in [false, true] {
        let fixture = Fixture::new();
        fs::remove_file(&fixture.path).unwrap();
        let update = |_: &mut File| -> anyhow::Result<()> { panic!("missing target accepted") };
        let result = if copy {
            file_update::copy_and_replace(&fixture.path, update).map(|_| ())
        } else {
            file_update::with_file_lock(&fixture.path, update)
        };
        assert!(result.is_err());
        assert_eq!(fs::read_dir(fixture.dir.path()).unwrap().count(), 0);
    }
}

#[test]
fn callback_error_or_panic_cleans_up_without_retrying() {
    for copy in [false, true] {
        for panic in [false, true] {
            let fixture = Fixture::new();
            let calls = Cell::new(0);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let update = |file: &mut File| -> anyhow::Result<()> {
                    calls.set(calls.get() + 1);
                    fixture.locked();
                    file.write_all(b"partial!")?;
                    assert!(!panic, "callback panic");
                    anyhow::bail!("callback failure")
                };
                if copy {
                    file_update::copy_and_replace(&fixture.path, update).map(|_| ())
                } else {
                    file_update::with_file_lock(&fixture.path, update)
                }
            }));
            if panic {
                assert!(result.is_err());
            } else {
                assert!(
                    result
                        .unwrap()
                        .unwrap_err()
                        .to_string()
                        .contains("callback failure")
                );
            }
            assert_eq!(calls.get(), 1);
            assert_eq!(
                fs::read(&fixture.path).unwrap(),
                if copy { b"original" } else { b"partial!" }
            );
            fixture.clean();
        }
    }
}

#[test]
fn lock_removal_failure_is_reported_even_after_publication() {
    let fixture = Fixture::new();
    let error = file_update::copy_and_replace(&fixture.path, |file| {
        file.write_all(b"updated!")?;
        fs::remove_file(&fixture.lock)?;
        fs::create_dir(&fixture.lock)?;
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains("removing writer lock"));
    assert_eq!(fs::read(&fixture.path).unwrap(), b"updated!");
    assert!(!fixture.dir.path().join(".tmp.data").exists());
}

#[test]
fn separate_targets_can_be_updated_while_another_target_is_locked() {
    let a = Fixture::new();
    let b = a.dir.path().join("other");
    fs::write(&b, b"original").unwrap();
    file_update::with_file_lock(&a.path, |_| {
        file_update::copy_and_replace(&b, |file| {
            file.write_all(b"other!!!")?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    assert_eq!(fs::read(&b).unwrap(), b"other!!!");
    assert_eq!(fs::read_dir(a.dir.path()).unwrap().count(), 2);
}

#[cfg(unix)]
#[test]
fn permissions_owner_and_group_survive_replacement() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&fixture.path).unwrap();
    file_update::copy_and_replace(&fixture.path, |_| Ok(())).unwrap();
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
        assert!(
            file_update::copy_and_replace(&alias, |_| panic!("invalid target accepted")).is_err()
        );
        assert_eq!(fs::read(&fixture.path).unwrap(), b"original");
        assert_eq!(fs::read_dir(fixture.dir.path()).unwrap().count(), 2);
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
        assert!(
            file_update::copy_and_replace(&fixture.path, |_| panic!("alias bypassed lock"))
                .is_err()
        );
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
        copy_and_replace_using(&fixture.path, |_| Ok(()), &ReflinkOnly).unwrap(),
        CopyMode::Replaced
    );
    assert_eq!(fs::read(&fixture.path).unwrap(), b"original");
    fixture.clean();
}
