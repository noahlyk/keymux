//! A temporary folder for one test, removed when the test ends, whether it passed or failed.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh, empty folder under the system temp folder, named `<prefix>-<pid>-<n>` so tests
/// running in parallel never share one. Dropping it (at the end of the test, or while a failed
/// test unwinds) deletes it and everything in it.
pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(prefix: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::TestDir;

    #[test]
    fn the_folder_exists_while_held_and_is_gone_after() {
        let dir = TestDir::new("keymux-test-dir-self");
        let other = TestDir::new("keymux-test-dir-self");
        assert_ne!(dir.path(), other.path(), "each one is its own folder");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let path = dir.path().to_path_buf();
        assert!(path.is_dir());
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn a_failing_test_still_removes_its_folder() {
        let path = std::panic::catch_unwind(|| {
            let dir = TestDir::new("keymux-test-dir-panic");
            let path = dir.path().to_path_buf();
            std::panic::panic_any(path);
        })
        .unwrap_err()
        .downcast::<std::path::PathBuf>()
        .unwrap();
        assert!(!path.exists());
    }
}
