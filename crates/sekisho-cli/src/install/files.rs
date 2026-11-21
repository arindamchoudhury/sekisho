//! Reading and writing files for an install: no symlink between a root and
//! a file (D207), atomic writes (a temporary file renamed in the same
//! folder), and a journal that puts back what an install wrote when a
//! later write fails.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::InstallError;
use crate::adapters::RelPath;

/// What is at a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disk {
    Missing,
    File(Vec<u8>),
    /// A folder, or anything else that isn't a file.
    Folder,
}

impl Disk {
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Disk::File(b) => Some(b),
            _ => None,
        }
    }
}

fn io(path: &Path, e: std::io::Error) -> InstallError {
    InstallError::Io(format!("{}: {e}", path.display()))
}

/// What is at `path`, without following a link there.
pub fn read(path: &Path) -> Result<Disk, InstallError> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Disk::Missing),
        // A path through a file: nothing can be there.
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Ok(Disk::Missing),
        Err(e) => Err(io(path, e)),
        Ok(meta) if meta.is_file() => std::fs::read(path).map(Disk::File).map_err(|e| io(path, e)),
        Ok(_) => Ok(Disk::Folder),
    }
}

/// A symlink, or on Windows any reparse point (junctions included).
fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// Refuses a link anywhere from `root` (exclusive) to `root/rel`: an install
/// never writes through one (D207). The root itself may be a link.
pub fn check_no_links(root: &Path, rel: &RelPath) -> Result<(), InstallError> {
    let mut at = root.to_path_buf();
    for segment in rel.segments() {
        at.push(segment);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if is_link(&meta) => {
                return Err(InstallError::Refused(format!(
                    "{} is a link: an install never writes through one",
                    at.display()
                )));
            }
            Ok(_) => {}
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

/// Writes `bytes` to `path` through a temporary file in its folder.
pub fn write_atomic(path: &Path, bytes: &[u8], executable: bool) -> Result<(), InstallError> {
    let folder = path
        .parent()
        .ok_or_else(|| InstallError::Io(format!("{} has no folder", path.display())))?;
    std::fs::create_dir_all(folder).map_err(|e| io(folder, e))?;
    let mut temp = tempfile::NamedTempFile::new_in(folder).map_err(|e| io(folder, e))?;
    temp.write_all(bytes).map_err(|e| io(path, e))?;
    temp.flush().map_err(|e| io(path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(mode))
            .map_err(|e| io(path, e))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    temp.persist(path).map_err(|e| io(path, e.error))?;
    Ok(())
}

/// Removes `path` and then each folder above it that is left empty, up to
/// `stop` (kept).
pub fn remove(path: &Path, stop: &Path) -> Result<(), InstallError> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(path, e)),
    }
    prune(path.parent(), stop);
    Ok(())
}

/// Removes empty folders from `from` up to `stop` (kept).
pub fn prune(mut from: Option<&Path>, stop: &Path) {
    while let Some(folder) = from {
        if folder == stop || !folder.starts_with(stop) {
            break;
        }
        // Fails when not empty, which ends the walk.
        if std::fs::remove_dir(folder).is_err() {
            break;
        }
        from = folder.parent();
    }
}

/// What an install changed, to put back if a later step fails.
#[derive(Default)]
pub struct Journal {
    /// Each path written or removed, what was there before (and whether it
    /// was executable), and the root to prune empty folders up to.
    undo: Vec<(PathBuf, Disk, bool, PathBuf)>,
}

/// The file at `path` may be run.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

impl Journal {
    /// Writes `bytes`, remembering what was there.
    pub fn write(
        &mut self,
        path: &Path,
        bytes: &[u8],
        executable: bool,
        root: &Path,
    ) -> Result<(), InstallError> {
        let before = read(path)?;
        let was = is_executable(path);
        write_atomic(path, bytes, executable)?;
        self.undo
            .push((path.to_path_buf(), before, was, root.to_path_buf()));
        Ok(())
    }

    /// Removes a file, remembering it.
    pub fn remove(&mut self, path: &Path, root: &Path) -> Result<(), InstallError> {
        let before = read(path)?;
        let was = is_executable(path);
        remove(path, root)?;
        self.undo
            .push((path.to_path_buf(), before, was, root.to_path_buf()));
        Ok(())
    }

    /// Puts back everything, newest first. Best effort: an install that
    /// failed reports its own error, not this one's.
    pub fn roll_back(self) {
        for (path, before, was, root) in self.undo.into_iter().rev() {
            match before {
                Disk::File(bytes) => {
                    let _ = write_atomic(&path, &bytes, was);
                }
                Disk::Missing => {
                    let _ = remove(&path, &root);
                }
                Disk::Folder => {}
            }
        }
    }
}
