use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Seek, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

/// How a [`copy_and_replace`] call published its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// Replaced the target with a completed copy.
    Replaced,
    /// Updated the original file directly under the writer lock.
    Direct,
}

/// Updates a private copy, then replaces `path` if its contents have not changed.
///
/// The callback receives a read/write file at offset zero and must finish writing before
/// returning. Only temporary-file creation or copy failure runs it on the original under lock
/// ([`CopyMode::Direct`]); errors there may leave partial writes. Other errors do not fall
/// back. A content conflict does not replace the target. The callback is never retried.
///
/// Uses the target and locking requirements of [`with_file_lock`]. On replacement, existing
/// readers keep the old file and new readers see the completed file. Atomic replacement requires
/// filesystem support and readers that permit it; power-loss durability is not guaranteed.
///
/// Preserves permissions and, on Unix, owner/group; ACLs and extended attributes are not copied.
/// The caller must keep any compressed-frame input stable until the callback returns.
///
/// # Examples
///
/// ```
/// use seekzstdsep::{copy_and_replace, append_records, with_file_lock, CopyMode, OnMissingSeparator};
/// # let dir = tempfile::tempdir()?;
/// # let path = dir.path().join("records.seek.zst");
/// # let mut bytes = Vec::new();
/// # seekzstdsep::convert_to_seekable_zst_reader(
/// #     &b"record 1\nrecord 2\nrecord 3\nrecord 4\nrecord 5\nrecord 6\n"[..],
/// #     &mut bytes, 16, true, b"\n", None)?;
/// # std::fs::write(&path, bytes)?;
/// let append = |file: &mut std::fs::File| {
///     append_records(file, &b"record 7\n"[..], b"\n", OnMissingSeparator::Refuse, 0, None)
/// };
/// let mode = copy_and_replace(&path, append)?;
/// assert_eq!(mode, CopyMode::Replaced);
/// with_file_lock(&path, append)?;
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn copy_and_replace(
    path: impl AsRef<Path>,
    update: impl FnOnce(&mut File) -> anyhow::Result<()>,
) -> anyhow::Result<CopyMode> {
    copy_and_replace_using(path.as_ref(), update, &System)
}

/// Opens `path` read/write at offset zero after locking, and holds the lock through `update`.
///
/// Use the supplied handle, not one opened before locking. Errors may leave partial writes.
/// Do not recursively lock the same target. All writers must cooperate through this function
/// or [`copy_and_replace`]; existing `&mut File` operations do not lock themselves.
///
/// Requires an existing writable regular file in a trusted directory and filesystem support
/// for advisory locks. Rejects final-component symlinks and, on Unix, multiple hard links.
/// The persistent lock is `.<filename>.seekzstdsep.lock` in the canonical parent directory;
/// do not delete it or replace that directory while writers are using it.
///
/// See the [shared example](copy_and_replace#examples).
pub fn with_file_lock<T>(
    path: impl AsRef<Path>,
    update: impl FnOnce(&mut File) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    with_file_lock_using(path.as_ref(), update, &System)
}

pub(crate) fn with_file_lock_using<T>(
    path: &Path,
    update: impl FnOnce(&mut File) -> anyhow::Result<T>,
    ops: &impl FileOps,
) -> anyhow::Result<T> {
    let path = resolve_target(path)?;
    let lock = open_lock(&path)?;
    ops.lock(&lock)?;
    let result = update(&mut open_current(&path)?)?;
    ops.unlock(&lock)?;
    Ok(result)
}

pub(crate) fn copy_and_replace_using(
    path: &Path,
    update: impl FnOnce(&mut File) -> anyhow::Result<()>,
    ops: &impl FileOps,
) -> anyhow::Result<CopyMode> {
    let path = resolve_target(path)?;
    let lock = open_lock(&path)?;
    ops.lock(&lock)?;
    let mut original = open_current(&path)?;
    let metadata = original.metadata()?;
    let hash = ops.hash(&mut original)?;
    let copy = match ops.create_copy(&path, &hash) {
        Ok(mut temp) => match ops.copy(&mut original, temp.as_file_mut()) {
            Ok(()) => Some(temp),
            Err(_) => {
                temp.close().context("removing failed update copy")?;
                None
            }
        },
        Err(_) => None,
    };
    let mut temp = match copy {
        Some(temp) => temp,
        None => {
            original.rewind()?;
            update(&mut original)?;
            ops.unlock(&lock)?;
            return Ok(CopyMode::Direct);
        }
    };
    drop(original);
    let mut published = false;
    let result = (|| {
        ops.unlock(&lock)?;
        temp.as_file_mut().rewind()?;
        update(temp.as_file_mut())?;
        preserve_metadata(&temp, &metadata)?;
        ops.lock(&lock)?;
        let mut current = open_current(&path)?;
        if ops.hash(&mut current)? != hash {
            bail!(
                "update conflict: {} changed since it was copied",
                path.display()
            );
        }
        drop(current);
        ops.replace(temp.path(), &path)?;
        published = true;
        ops.unlock(&lock)?;
        Ok(CopyMode::Replaced)
    })();
    if !published {
        temp.close().context("removing update temporary file")?;
    }
    result
}

fn resolve_target(path: &Path) -> anyhow::Result<PathBuf> {
    let name = path.file_name().context("target must name a file")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent.canonicalize()?.join(name))
}

fn open_lock(path: &Path) -> anyhow::Result<File> {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().context("target must name a file")?);
    name.push(".seekzstdsep.lock");
    let path = path.with_file_name(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => check_regular(&metadata)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}

fn check_regular(metadata: &Metadata) -> anyhow::Result<()> {
    if !metadata.file_type().is_file() {
        bail!("target and lock must be regular files, not symlinks");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!("target and lock must not have multiple hard links");
        }
    }
    Ok(())
}

fn open_current(path: &Path) -> anyhow::Result<File> {
    check_regular(&fs::symlink_metadata(path)?)?;
    Ok(OpenOptions::new().read(true).write(true).open(path)?)
}

fn preserve_metadata(temp: &NamedTempFile, original: &Metadata) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, chown};
        let copied = temp.as_file().metadata()?;
        if (copied.uid(), copied.gid()) != (original.uid(), original.gid()) {
            chown(temp.path(), Some(original.uid()), Some(original.gid()))?;
        }
    }
    temp.as_file().set_permissions(original.permissions())?;
    Ok(())
}

pub(crate) struct System;

pub(crate) trait FileOps {
    fn lock(&self, file: &File) -> io::Result<()> {
        FileExt::lock_exclusive(file)
    }

    fn unlock(&self, file: &File) -> io::Result<()> {
        FileExt::unlock(file)
    }

    fn hash(&self, file: &mut File) -> io::Result<String> {
        file.rewind()?;
        let mut hash = HashWriter(Sha256::new());
        io::copy(file, &mut hash)?;
        file.rewind()?;
        Ok(format!("{:x}", hash.0.finalize()))
    }

    fn create_copy(&self, path: &Path, hash: &str) -> io::Result<NamedTempFile> {
        tempfile::Builder::new()
            .prefix(&format!(".seekzstdsep-{hash}-"))
            .tempfile_in(path.parent().unwrap())
    }

    fn reflink(&self, from: &File, to: &File) -> io::Result<()> {
        if let Some(len) = NonZeroU64::new(from.metadata()?.len()) {
            reflink_copy::ReflinkBlockBuilder::new(from, to, len).reflink_block()?;
        }
        Ok(())
    }

    fn ordinary_copy(&self, from: &mut File, to: &mut File) -> io::Result<()> {
        to.set_len(0)?;
        to.rewind()?;
        from.rewind()?;
        io::copy(from, to)?;
        Ok(())
    }

    fn copy(&self, from: &mut File, to: &mut File) -> io::Result<()> {
        self.reflink(from, to)
            .or_else(|_| self.ordinary_copy(from, to))
    }

    fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
}

impl FileOps for System {}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
