//! Content-addressed artifact cache.
//!
//! Cache key = blake3(drv_path) — the drv path already encodes all inputs
//! (source hash, dep hashes, rustc flags, features) via Nix's own hashing.
//!
//! Layout:
//!   $XDG_CACHE_HOME/nix-inc/
//!     artifacts/<blake3-of-drv-path>/
//!       out/    — corresponds to $out
//!       lib/    — corresponds to $lib
//!     tmp/      — in-progress builds, renamed atomically on completion

use std::path::{Path, PathBuf};

pub struct ArtifactCache {
    root: PathBuf,
}

impl ArtifactCache {
    pub fn new() -> Self {
        let cache_dir = std::env::var("XDG_CACHE_HOME")
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").expect("HOME not set");
                format!("{home}/.cache")
            });
        Self {
            root: PathBuf::from(cache_dir).join("nix-inc"),
        }
    }

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

    /// The "out" output directory for a cached build.
    pub fn out_dir(&self, drv_path: &str) -> PathBuf {
        self.artifact_dir(drv_path).join("out")
    }

    /// The "lib" output directory for a cached build.
    pub fn lib_dir(&self, drv_path: &str) -> PathBuf {
        self.artifact_dir(drv_path).join("lib")
    }

    /// Temp directory for in-progress builds.
    pub fn tmp_dir(&self, drv_path: &str) -> PathBuf {
        self.root.join("tmp").join(Self::cache_key(drv_path))
    }

    /// Check if a build result is cached.
    pub fn is_cached(&self, drv_path: &str) -> bool {
        self.artifact_dir(drv_path).exists()
    }

    /// Check if a build result is cached, by raw key.
    pub fn is_cached_key(&self, key: &str) -> bool {
        self.artifact_dir_by_key(key).exists()
    }

    /// Atomically commit a completed build from tmp to artifacts.
    pub fn commit(&self, drv_path: &str) -> Result<(), std::io::Error> {
        self.commit_key(&Self::cache_key(drv_path))
    }

    /// Atomically commit by raw key.
    pub fn commit_key(&self, key: &str) -> Result<(), std::io::Error> {
        let tmp = self.root.join("tmp").join(key);
        let dest = self.artifact_dir_by_key(key);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&tmp, &dest)
    }

    /// Remove cached artifact.
    pub fn invalidate(&self, drv_path: &str) -> Result<(), std::io::Error> {
        let dir = self.artifact_dir(drv_path);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Prepare a fresh tmp directory for building.
    pub fn prepare_tmp(&self, drv_path: &str) -> Result<PathBuf, std::io::Error> {
        let tmp = self.tmp_dir(drv_path);
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)?;
        }
        std::fs::create_dir_all(&tmp)?;
        Ok(tmp)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Persistent incremental compilation cache for a crate.
    /// Unlike artifact_dir (which is replaced on each build),
    /// the incremental dir persists across builds so rustc can
    /// reuse compilation state.
    pub fn incremental_dir(&self, drv_path: &str) -> PathBuf {
        self.root.join("incremental").join(Self::cache_key(drv_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn cache_lifecycle() {
        let tmp = tempdir();
        let cache = ArtifactCache::from_path(tmp.clone());
        let drv = "/nix/store/aaaa-test.drv";

        assert!(!cache.is_cached(drv));

        // Prepare tmp, write something, commit
        let build_dir = cache.prepare_tmp(drv).unwrap();
        fs::create_dir_all(build_dir.join("out")).unwrap();
        fs::write(build_dir.join("out").join("hello"), b"world").unwrap();

        cache.commit(drv).unwrap();
        assert!(cache.is_cached(drv));
        assert_eq!(
            fs::read_to_string(cache.out_dir(drv).join("hello")).unwrap(),
            "world"
        );

        // Invalidate
        cache.invalidate(drv).unwrap();
        assert!(!cache.is_cached(drv));
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nib-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
