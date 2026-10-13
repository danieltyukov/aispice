//! Reading a package: only what is in it.
//!
//! A symbolic link could lead to a sibling directory (another package's
//! evidence, say) or anywhere on the machine, and a FIFO would block a read,
//! so the validator follows neither: each file is opened relative to the
//! package without following a link in any part of its path, without
//! blocking, and read only if it is a regular file. On Windows a junction or
//! any other reparse point counts as a link. ChipLeap reads packages the same
//! way (`chipleap.macro.check._open`).

use super::evidence::sha256_file_capped;
use super::limits::{ReadError, read_file_capped};
use std::fs::File;
use std::io;
use std::path::Path;

/// What a package holds at a path, its last part not followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Missing,
    File,
    Dir,
    Link,
    Other,
}

pub(crate) fn kind(root: &Path, rel: &str) -> Kind {
    let Ok(meta) = std::fs::symlink_metadata(root.join(rel)) else {
        return Kind::Missing;
    };
    let t = meta.file_type();
    if t.is_symlink() || reparse_point(&meta) {
        Kind::Link
    } else if t.is_file() {
        Kind::File
    } else if t.is_dir() {
        Kind::Dir
    } else {
        Kind::Other
    }
}

#[cfg(windows)]
fn reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

/// Package file `rel` (`name` or `evidence/name`) opened for reading,
/// without following a link in any part of it, and only if it is a regular
/// file.
pub(crate) fn open(root: &Path, rel: &str) -> io::Result<File> {
    let parts: Vec<&str> = rel.split('/').collect();
    let (name, dirs) = parts.split_last().unwrap_or((&"", &[]));
    let file = open_nofollow(root, dirs, name)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("it is not a regular file"));
    }
    Ok(file)
}

/// A package file's bytes, within `limit`.
pub fn read_package_file(root: &Path, rel: &str, limit: u64) -> Result<Vec<u8>, ReadError> {
    read_file_capped(open(root, rel)?, limit)
}

/// A package file's SHA-256, within `limit`.
pub fn sha256_package_file(root: &Path, rel: &str, limit: u64) -> Result<String, ReadError> {
    sha256_file_capped(open(root, rel)?, limit)
}

/// `error`, or `why` when `path` turns out to be a link: what refused it.
#[cfg(unix)]
fn link_or(path: &Path, error: io::Error, why: &str) -> io::Error {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => io::Error::other(why.to_string()),
        _ => error,
    }
}

/// Each directory is opened from the one before it with `O_NOFOLLOW`, and
/// the file from the last; the file without blocking, so a FIFO opens and is
/// then refused as not a regular file.
#[cfg(unix)]
fn open_nofollow(root: &Path, dirs: &[&str], name: &str) -> io::Result<File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    let read = OFlags::RDONLY | OFlags::CLOEXEC;
    // The package directory itself may be a link; only what is inside it is
    // read without following one.
    let mut dir = openat(CWD, root, read | OFlags::DIRECTORY, Mode::empty())?;
    let mut at = root.to_path_buf();
    for d in dirs {
        at.push(d);
        let flags = read | OFlags::DIRECTORY | OFlags::NOFOLLOW;
        dir = openat(&dir, *d, flags, Mode::empty())
            .map_err(|e| link_or(&at, e.into(), &format!("{d} is a symbolic link")))?;
    }
    at.push(name);
    let flags = read | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let fd = openat(&dir, name, flags, Mode::empty())
        .map_err(|e| link_or(&at, e.into(), "it is a symbolic link"))?;
    Ok(File::from(fd))
}

/// Each directory is held open without delete sharing, so it cannot be
/// renamed or replaced by a junction until the file is open, and every
/// handle is opened on the reparse point itself, never on what it leads to.
#[cfg(windows)]
fn open_nofollow(root: &Path, dirs: &[&str], name: &str) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut held = Vec::with_capacity(dirs.len());
    let mut at = root.to_path_buf();
    for d in dirs {
        at.push(d);
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&at)?;
        let meta = dir.metadata()?;
        if reparse_point(&meta) {
            return Err(io::Error::other(format!("{d} is a symbolic link")));
        }
        if !meta.is_dir() {
            return Err(io::Error::other(format!("{d} is not a directory")));
        }
        held.push(dir);
    }
    at.push(name);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&at)?;
    if reparse_point(&file.metadata()?) {
        return Err(io::Error::other("it is a symbolic link"));
    }
    drop(held);
    Ok(file)
}

/// Elsewhere, each part is looked at before the file is opened.
#[cfg(not(any(unix, windows)))]
fn open_nofollow(root: &Path, dirs: &[&str], name: &str) -> io::Result<File> {
    let mut at = root.to_path_buf();
    for d in dirs {
        at.push(d);
        if std::fs::symlink_metadata(&at)?.file_type().is_symlink() {
            return Err(io::Error::other(format!("{d} is a symbolic link")));
        }
    }
    at.push(name);
    if std::fs::symlink_metadata(&at)?.file_type().is_symlink() {
        return Err(io::Error::other("it is a symbolic link"));
    }
    File::open(&at)
}
