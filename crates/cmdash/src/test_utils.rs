#![doc(hidden)]
//! Shared test utilities for integration tests.
//!
//! This module provides lightweight test helpers that are always compiled
//! (not gated behind `#[cfg(test)]`) so integration tests in `crates/*/tests/`
//! can import them via `cmdash::test_utils::*`.
//!
//! [`TestDir`] is an RAII wrapper around a temporary directory that
//! auto-cleans on drop. [`make_isolated_test_dir`] creates one with a
//! unique name to avoid parallel-test collisions.

/// RAII wrapper around a temporary test directory that
/// automatically removes the directory (and its contents) when
/// dropped. Derefs to `Path` so callers can use `dir.join(...)`
/// transparently.
#[doc(hidden)]
pub struct TestDir(std::path::PathBuf);

impl std::ops::Deref for TestDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for TestDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Create an isolated temporary directory for a test. The
/// directory name is derived from `prefix` plus a nanosecond
/// timestamp so parallel test runs never collide. The returned
/// [`TestDir`] cleans up automatically when it goes out of scope.
#[doc(hidden)]
pub fn make_isolated_test_dir(prefix: &str) -> TestDir {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{}_{}", prefix, nanos));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create test dir {:?}: {}", dir, e));
    TestDir(dir)
}
