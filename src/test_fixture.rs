//! Panic-safe fixture directory shared by test helpers whose fixtures must sit
//! at a fixed on-disk anchor (so `temp_dir` / `tempfile::TempDir` do not fit).

use std::path::{Path, PathBuf};

/// Best-effort permission restore over the whole tree (dirs 0o700, files
/// 0o600). Uses `symlink_metadata` and never follows symlinks, so nothing
/// outside the fixture tree is touched.
#[cfg(unix)]
fn restore_tree_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return;
    }
    if file_type.is_dir() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                restore_tree_permissions(&entry.path());
            }
        }
    } else if file_type.is_file() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Panic-safe fixture directory: removed on `Drop`, so an assert failing
/// mid-test no longer leaks the directory.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct FixtureDir(PathBuf);

#[cfg(unix)]
impl FixtureDir {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self(dir)
    }
}

#[cfg(unix)]
impl std::ops::Deref for FixtureDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

#[cfg(unix)]
impl AsRef<Path> for FixtureDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(unix)]
impl Drop for FixtureDir {
    fn drop(&mut self) {
        if std::fs::remove_dir_all(&self.0).is_ok() {
            return;
        }
        // A test may panic while a subtree is chmod 0o000; restore permissions
        // and retry. Never panic in `Drop` (a double panic aborts).
        restore_tree_permissions(&self.0);
        if let Err(err) = std::fs::remove_dir_all(&self.0) {
            if self.0.exists() {
                // `eprintln!` panics on a broken stderr pipe (SIGABRT while
                // unwinding), so write best-effort and ignore I/O errors.
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr(),
                    "FixtureDir: failed to remove {}: {err}",
                    self.0.display()
                );
            }
        }
    }
}
