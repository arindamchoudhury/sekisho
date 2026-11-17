//! Reading an entry's files from a checkout. Nothing outside the entry's
//! folder is ever read: a symlink anywhere, or a folder that leaves the
//! checkout, refuses the entry (D166).

use std::fs;
use std::io::Read;
use std::path::{Component, Path};

/// The size limits of Global Constraints (D166).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// One file.
    pub file_bytes: u64,
    /// All of one entry's files.
    pub entry_bytes: u64,
    pub entry_files: usize,
    pub skill_md_bytes: u64,
    /// Catalog files in one `catalog` source.
    pub catalog_files: usize,
    /// An A2A agent's card.
    pub card_bytes: u64,
    /// A plugin's manifest or a marketplace file.
    pub manifest_bytes: u64,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        file_bytes: 10 << 20,
        entry_bytes: 50 << 20,
        entry_files: 5_000,
        skill_md_bytes: 1 << 20,
        catalog_files: 1_000,
        card_bytes: 1 << 20,
        manifest_bytes: 5 << 20,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One file of an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    /// Relative to the entry's folder, with `/` between parts.
    pub path: String,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

/// Why an entry's files can't be read. Paths are relative to the checkout.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("{path} leaves the repository's folder")]
    Escapes { path: String },
    #[error("{path} is a symlink; entries can't hold symlinks")]
    Symlink { path: String },
    #[error("{what} is larger than {limit} bytes")]
    TooBig { what: String, limit: u64 },
    #[error("the entry has more than {limit} files")]
    TooMany { limit: usize },
    #[error("{path}: {reason}")]
    Unreadable { path: String, reason: String },
}

/// `path` relative to `root`, with `/` between parts, for messages.
fn shown(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let parts: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

/// Reads every file under `dir`, an entry's folder inside the checkout
/// `root`, sorted by path. `.git` is skipped.
pub fn read_entry_files(
    root: &Path,
    dir: &Path,
    limits: &Limits,
) -> Result<Vec<TreeFile>, TreeError> {
    let Ok(rel) = dir.strip_prefix(root) else {
        return Err(TreeError::Escapes {
            path: "the entry's folder".to_owned(),
        });
    };
    // Only plain names below the checkout, and none of them a symlink.
    let mut here = root.to_path_buf();
    for part in rel.components() {
        let Component::Normal(name) = part else {
            return Err(TreeError::Escapes {
                path: shown(root, dir),
            });
        };
        here.push(name);
        let meta = fs::symlink_metadata(&here).map_err(|e| TreeError::Unreadable {
            path: shown(root, &here),
            reason: e.kind().to_string(),
        })?;
        if meta.file_type().is_symlink() {
            return Err(TreeError::Symlink {
                path: shown(root, &here),
            });
        }
    }
    if !here.is_dir() {
        return Err(TreeError::Unreadable {
            path: shown(root, dir),
            reason: "not a folder".to_owned(),
        });
    }
    let mut walk = Walk {
        root,
        limits,
        total: 0,
        files: Vec::new(),
    };
    walk.folder(dir, "")?;
    let mut files = walk.files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

struct Walk<'a> {
    root: &'a Path,
    limits: &'a Limits,
    total: u64,
    files: Vec<TreeFile>,
}

impl Walk<'_> {
    fn unreadable(&self, path: &Path, e: std::io::Error) -> TreeError {
        TreeError::Unreadable {
            path: shown(self.root, path),
            reason: e.kind().to_string(),
        }
    }

    fn folder(&mut self, dir: &Path, prefix: &str) -> Result<(), TreeError> {
        let mut items = fs::read_dir(dir)
            .map_err(|e| self.unreadable(dir, e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| self.unreadable(dir, e))?;
        items.sort_by_key(|i| i.file_name());
        for item in items {
            let path = item.path();
            let Some(name) = item.file_name().to_str().map(str::to_owned) else {
                return Err(TreeError::Unreadable {
                    path: shown(self.root, &path),
                    reason: "the name isn't UTF-8".to_owned(),
                });
            };
            // Never follows a link: this is the item's own type.
            let kind = item.file_type().map_err(|e| self.unreadable(&path, e))?;
            if kind.is_symlink() {
                return Err(TreeError::Symlink {
                    path: shown(self.root, &path),
                });
            }
            if name == ".git" {
                continue;
            }
            let rel = format!("{prefix}{name}");
            if kind.is_dir() {
                self.folder(&path, &format!("{rel}/"))?;
            } else if kind.is_file() {
                self.file(&path, rel)?;
            } else {
                return Err(TreeError::Unreadable {
                    path: shown(self.root, &path),
                    reason: "not a regular file".to_owned(),
                });
            }
        }
        Ok(())
    }

    fn file(&mut self, path: &Path, rel: String) -> Result<(), TreeError> {
        if self.files.len() == self.limits.entry_files {
            return Err(TreeError::TooMany {
                limit: self.limits.entry_files,
            });
        }
        let file = fs::File::open(path).map_err(|e| self.unreadable(path, e))?;
        let meta = file.metadata().map_err(|e| self.unreadable(path, e))?;
        let mut bytes = Vec::new();
        // Read one byte past the limit, so a file that grew is caught too.
        file.take(self.limits.file_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| self.unreadable(path, e))?;
        let size = bytes.len() as u64;
        if size > self.limits.file_bytes {
            return Err(TreeError::TooBig {
                what: shown(self.root, path),
                limit: self.limits.file_bytes,
            });
        }
        self.total += size;
        if self.total > self.limits.entry_bytes {
            return Err(TreeError::TooBig {
                what: "the entry".to_owned(),
                limit: self.limits.entry_bytes,
            });
        }
        self.files.push(TreeFile {
            path: rel,
            bytes,
            executable: is_executable(&meta),
        });
        Ok(())
    }
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    false
}
