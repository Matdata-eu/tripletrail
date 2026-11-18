//! Temporary folders for tests and benches.
//!
//! A [`TestDir`] is made under the system's temporary folder (`TMPDIR`) with a name that starts
//! with `tripletrail-`, and is removed with everything in it when dropped: at the end of the
//! test, and also when the test fails or panics. A test that starts a server or a process that
//! writes into the folder keeps the `TestDir` until that server or process has stopped.
//!
//! To look at the files after a run, set `TRIPLETRAIL_KEEP_TMP=1`: the folders are then kept,
//! and each one's path is printed (on standard error, which `cargo test` shows for a failing
//! test, or with `--nocapture`).

use std::ffi::OsStr;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// The environment variable that keeps the folders after the test.
pub const KEEP_VAR: &str = "TRIPLETRAIL_KEEP_TMP";

/// The start of every folder's name, which CI looks for after the tests to find leaks.
pub const PREFIX: &str = "tripletrail-";

/// A temporary folder, removed when dropped unless [`KEEP_VAR`] is set. It dereferences to its
/// [`Path`], so `dir.join("a.ttl")` and `&dir` (as a `&Path`) work as with a `PathBuf`.
#[derive(Debug)]
pub struct TestDir {
    dir: tempfile::TempDir,
    keep: bool,
}

/// Whether [`KEEP_VAR`] asks to keep the folders: set to anything but empty, `0` or `false`.
pub fn keep() -> bool {
    std::env::var_os(KEEP_VAR).is_some_and(|v| !matches!(v.to_str(), Some("" | "0" | "false")))
}

/// A new empty folder named `tripletrail-<name>-<random>` in the system's temporary folder.
///
/// # Panics
///
/// When the folder cannot be made.
#[must_use]
pub fn tempdir(name: &str) -> TestDir {
    let prefix = format!("{PREFIX}{name}-");
    let keep = keep();
    // Removed by `TestDir`'s own `drop`, which tries again while something still writes.
    let dir = tempfile::Builder::new()
        .prefix(&prefix)
        .disable_cleanup(true)
        .tempdir()
        .unwrap_or_else(|e| panic!("making a temporary folder {prefix}*: {e}"));
    if keep {
        eprintln!("{KEEP_VAR}: keeping {}", dir.path().display());
    }
    TestDir { dir, keep }
}

impl Drop for TestDir {
    /// Removes the folder. A thread that is still finishing (a server's background work,
    /// say) may add a file while it is removed, which fails the removal: it is tried again for
    /// up to five seconds.
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let path = self.dir.path();
        for _ in 0..50 {
            match std::fs::remove_dir_all(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound && path.exists() => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                _ => return,
            }
        }
        eprintln!("could not remove the temporary folder {}", path.display());
    }
}

impl TestDir {
    /// The folder's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `relative` in this folder, as a path that keeps the folder until it is dropped: for a
    /// helper that makes a file and returns its path.
    #[must_use]
    pub fn into_path_of(self, relative: impl AsRef<Path>) -> TestPath {
        TestPath {
            path: self.dir.path().join(relative),
            _dir: self,
        }
    }
}

/// A path in a [`TestDir`] that it keeps: the folder is removed when this is dropped.
#[derive(Debug)]
pub struct TestPath {
    path: PathBuf,
    _dir: TestDir,
}

impl TestPath {
    /// The path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for TestPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for TestPath {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

impl Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.dir.path()
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        self.dir.path()
    }
}

impl AsRef<OsStr> for TestDir {
    fn as_ref(&self) -> &OsStr {
        self.dir.path().as_os_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_when_dropped_even_on_panic() {
        let dir = tempdir("testdir-self");
        let path = dir.to_path_buf();
        assert!(path.is_dir());
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tripletrail-testdir-self-")
        );
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        drop(dir);
        if !keep() {
            assert!(!path.exists());
        }
        let path = std::thread::spawn(|| {
            let dir = tempdir("testdir-panic");
            let path = dir.to_path_buf();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _dir = dir;
                panic!("a failing test");
            }));
            assert!(result.is_err());
            path
        })
        .join()
        .unwrap();
        if !keep() {
            assert!(!path.exists());
        }
    }
}
