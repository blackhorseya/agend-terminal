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

/// Diagnostic to emit when the final `remove_dir_all` of `path` failed with
/// `err`, or `None` when nothing needs reporting.
///
/// Silent only when `path` is positively confirmed gone (`NotFound`). Any
/// other outcome of the probe, such as `PermissionDenied` because a parent
/// lost search permission, means "cannot confirm" and is reported:
/// `Path::exists()` folds those errors into `false` and would hide residue.
#[cfg(unix)]
fn leftover_report(path: &Path, err: &std::io::Error) -> Option<String> {
    match std::fs::symlink_metadata(path) {
        Err(probe) if probe.kind() == std::io::ErrorKind::NotFound => None,
        Ok(_) => Some(format!(
            "FixtureDir: failed to remove {}: {err}",
            path.display()
        )),
        Err(probe) => Some(format!(
            "FixtureDir: failed to remove {}: {err} (cannot confirm it is gone: {probe})",
            path.display()
        )),
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
            if let Some(msg) = leftover_report(&self.0, &err) {
                // `eprintln!` panics on a broken stderr pipe (SIGABRT while
                // unwinding), so write best-effort and ignore I/O errors.
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "{msg}");
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Restores the parent's mode on every exit path (including a failing
    /// assert) so the surrounding tempdir can always be deleted.
    struct ModeRestore(PathBuf);
    impl Drop for ModeRestore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    /// A parent without search permission makes the fixture unreachable, so
    /// `exists()` answers false while the directory is still on disk. That is
    /// "cannot confirm it is gone", which must be reported, not swallowed.
    #[test]
    fn unconfirmable_removal_is_reported() {
        let root = std::env::temp_dir().join(format!(
            "agend-fixture-dir-unsearchable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let parent = root.join("parent");
        let fixture = parent.join("fixture");
        std::fs::create_dir_all(&fixture).expect("mkdir fixture");
        let _restore = ModeRestore(parent.clone());
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000))
            .expect("chmod parent 000");
        let err = std::fs::remove_dir_all(&fixture).expect_err("unreachable fixture");
        let report = leftover_report(&fixture, &err);
        // Restore before asserting so the residue below is always cleaned.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).ok();
        let still_there = fixture.exists();
        let _ = std::fs::remove_dir_all(&root);
        assert!(still_there, "probe premise: the fixture must still exist");
        assert!(
            report.is_some(),
            "removal failed on an unreachable path but nothing would be reported"
        );
    }

    /// Negative control: a path that is positively gone stays silent.
    #[test]
    fn confirmed_missing_path_is_not_reported() {
        let gone = std::env::temp_dir().join(format!(
            "agend-fixture-dir-gone-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let err = std::fs::remove_dir_all(&gone).expect_err("missing path");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(leftover_report(&gone, &err), None);
    }
}
