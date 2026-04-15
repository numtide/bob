//! Content-addressed artifact cache.
//!
//! Cache key = blake3(drv_path) — the drv path already encodes all inputs
//! (source hash, dep hashes, compiler flags, feature selection) via Nix's
//! own hashing.
//!
//! Layout:
//!   $XDG_CACHE_HOME/bob/
//!     artifacts/<blake3-of-drv-path>/
//!       out/    — corresponds to $out
//!       lib/    — corresponds to $lib
//!     tmp/      — in-progress builds, renamed atomically on completion

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub struct ArtifactCache {
    root: PathBuf,
}

/// Exclusive lock on the cache root, released on drop. Prevents two
/// concurrent `bob build` runs from tearing down each other's `tmp/<key>`.
pub struct CacheLock(#[allow(dead_code)] File);

impl Default for ArtifactCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtifactCache {
    pub fn new() -> Self {
        let cache_dir = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            format!("{home}/.cache")
        });
        Self {
            root: PathBuf::from(cache_dir).join("bob"),
        }
    }

    #[cfg(test)]
    pub fn from_path(root: PathBuf) -> Self {
        Self { root }
    }

    /// The cache key for a derivation: blake3 of the drv path.
    /// The drv path itself is a content hash of all build inputs.
    pub fn cache_key(drv_path: &str) -> String {
        let hash = blake3::hash(drv_path.as_bytes());
        hash.to_hex().to_string()
    }

    /// Cache key that incorporates a source hash suffix.
    /// Used when reusing an old drv with overridden source.
    pub fn cache_key_with_source(drv_path: &str, source_hash: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(drv_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(source_hash.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// Path where a cached artifact lives.
    pub fn artifact_dir(&self, drv_path: &str) -> PathBuf {
        self.root.join("artifacts").join(Self::cache_key(drv_path))
    }

    /// Path where a cached artifact lives, by raw key.
    pub fn artifact_dir_by_key(&self, key: &str) -> PathBuf {
        self.root.join("artifacts").join(key)
    }

    /// Check if a build result is cached, by raw key.
    pub fn is_cached_key(&self, key: &str) -> bool {
        self.artifact_dir_by_key(key).exists()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Take an exclusive `flock` on `<root>/.lock`. Blocks until acquired;
    /// returns a guard that releases on drop (via fd close).
    pub fn lock_exclusive(&self) -> Result<CacheLock, String> {
        std::fs::create_dir_all(&self.root).map_err(|e| format!("creating cache root: {e}"))?;
        let path = self.root.join(".lock");
        let f = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("opening {}: {e}", path.display()))?;
        // Non-blocking try first so we can print a hint before blocking.
        let fd = f.as_raw_fd();
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            eprintln!("  waiting for cache lock ({}) …", path.display());
            if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
                return Err(format!(
                    "locking {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(CacheLock(f))
    }

    /// Persistent per-unit incremental-compilation state. Unlike
    /// `artifact_dir` (replaced on each build), this persists across builds
    /// so the backend's compiler can reuse work (`-C incremental`, `GOCACHE`,
    /// …). Keyed on drv_path so source edits don't cold-start it.
    pub fn incremental_dir(&self, drv_path: &str) -> PathBuf {
        self.root
            .join("incremental")
            .join(Self::cache_key(drv_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bob-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cache_key_stable() {
        let cache = ArtifactCache::from_path(tempdir());
        let drv = "/nix/store/aaaa-test.drv";
        let k = ArtifactCache::cache_key(drv);
        assert_eq!(k.len(), 64);
        assert!(!cache.is_cached_key(&k));
        assert_eq!(cache.artifact_dir(drv), cache.artifact_dir_by_key(&k));
        assert_ne!(
            ArtifactCache::cache_key_with_source(drv, "a"),
            ArtifactCache::cache_key_with_source(drv, "b"),
        );
    }
}
