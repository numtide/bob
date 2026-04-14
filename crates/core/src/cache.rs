//! Content-addressed artifact cache.
//!
//! Cache key = blake3(drv_path) — the drv path already encodes all inputs
//! (source hash, dep hashes, rustc flags, features) via Nix's own hashing.
//!
//! Layout:
//!   $XDG_CACHE_HOME/bob/
//!     artifacts/<blake3-of-drv-path>/
//!       out/    — corresponds to $out
//!       lib/    — corresponds to $lib
//!     tmp/      — in-progress builds, renamed atomically on completion

use std::path::{Path, PathBuf};

pub struct ArtifactCache {
    root: PathBuf,
}

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

    /// Persistent incremental compilation cache for a crate.
    /// Unlike artifact_dir (which is replaced on each build),
    /// the incremental dir persists across builds so rustc can
    /// reuse compilation state.
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
