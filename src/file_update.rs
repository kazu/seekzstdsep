use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Seek},
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use tempfile::NamedTempFile;

/// How a [`copy_and_replace`] call published its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// Replaced the target with a completed copy.
    Replaced,
    /// Legacy direct-update mode; [`copy_and_replace`] no longer returns this variant.
    Direct,
}

/// Updates a private copy, then replaces `path` while holding its writer lock.
///
/// The callback receives a read/write file at offset zero and must finish writing before
/// returning. Creates `.tmp.<filename>` beside the target and refuses an existing temporary
/// file. Copy or callback failure leaves the target unchanged; there is no direct-update
/// fallback. Success returns [`CopyMode::Replaced`]. The callback is never retried.
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
/// copy_and_replace(&path, |file| seekzstdsep::truncate(file, 4, b"\n"))?;
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
/// Requires an existing writable regular file in a trusted directory. Rejects final-component
/// symlinks and, on Unix, multiple hard links. Exclusively creates `.lock.<filename>` in the
/// canonical parent directory; an existing lock causes an immediate error. Removes its own
/// lock on completion, including errors. A removal failure is reported as an error even if
/// the update was published. Forced termination can leave a lock or temporary copy behind;
/// remove these only after verifying that no writer is active. Do not replace the directory
/// while writers are using it.
///
/// See the [shared example](copy_and_replace#examples).
pub fn with_file_lock<T>(
    path: impl AsRef<Path>,
    update: impl FnOnce(&mut File) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let path = resolve_target(path.as_ref())?;
    let lock = create_sibling(&path, ".lock.").context("creating writer lock")?;
    let result = (|| update(&mut open_current(&path)?))();
    lock.close().context("removing writer lock")?;
    result
}

pub(crate) fn copy_and_replace_using(
    path: &Path,
    update: impl FnOnce(&mut File) -> anyhow::Result<()>,
    ops: &impl FileOps,
) -> anyhow::Result<CopyMode> {
    let path = resolve_target(path)?;
    with_file_lock(&path, |original| {
        let metadata = original.metadata()?;
        let mut temp = ops.create_copy(&path)?;
        let result = (|| {
            ops.copy(original, temp.as_file_mut())?;
            temp.as_file_mut().rewind()?;
            update(temp.as_file_mut())?;
            preserve_metadata(&temp, &metadata)?;
            ops.replace(temp.path(), &path)?;
            Ok(CopyMode::Replaced)
        })();
        if result.is_err() {
            temp.close().context("removing update temporary file")?;
        }
        result
    })
}

fn resolve_target(path: &Path) -> anyhow::Result<PathBuf> {
    let name = path.file_name().context("target must name a file")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent.canonicalize()?.join(name))
}

fn create_sibling(path: &Path, prefix: &str) -> io::Result<NamedTempFile> {
    let mut name = std::ffi::OsString::from(prefix);
    name.push(path.file_name().unwrap());
    tempfile::Builder::new()
        .prefix(&name)
        .rand_bytes(0)
        .tempfile_in(path.parent().unwrap())
}

fn check_regular(metadata: &Metadata) -> anyhow::Result<()> {
    if !metadata.file_type().is_file() {
        bail!("target must be a regular file, not a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!("target must not have multiple hard links");
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
    fn create_copy(&self, path: &Path) -> io::Result<NamedTempFile> {
        create_sibling(path, ".tmp.")
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
