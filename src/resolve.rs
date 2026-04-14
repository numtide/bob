//! Resolve target names to drv paths via nix-instantiate against `bob.nix`.
//!
//! The eval cache is keyed on `(target, lock_hash)` only — the backend
//! supplies `lock_hash` (e.g. blake3 of Cargo.lock / go.sum). Source changes
//! do NOT invalidate it: we always reuse the cached drv and let
//! `overrides::cascade` detect per-unit source changes and cascade them
//! through the build graph as cache-key overrides. This avoids the ~2s
//! nix-instantiate on every edit while staying correct for transitive
//! dependency changes.

use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Path to nix-instantiate. The repo's `bob.nix` may need extra builtins
/// (e.g. `resolveCargoWorkspace`); point `BOB_NIX_INSTANTIATE` at a patched
/// build, otherwise resolved from PATH.
fn nix_instantiate() -> String {
    std::env::var("BOB_NIX_INSTANTIATE").unwrap_or_else(|_| "nix-instantiate".into())
}

/// Result of resolving a workspace member.
pub struct ResolveResult {
    /// The .drv path (may be from a previous eval; source overrides are
    /// computed separately against the build graph).
    pub drv_path: String,
}

pub struct EvalCache {
    cache_dir: PathBuf,
}

impl EvalCache {
    pub fn new(cache_root: &Path) -> Self {
        Self {
            cache_dir: cache_root.join("eval"),
        }
    }

    /// Hash a unit's source directory to detect file changes.
    /// Uses a two-level approach: first hashes mtimes (fast ~0.1ms),
    /// then only reads file contents if the mtime hash changed since
    /// last time. The mtime→content mapping is cached under
    /// `$XDG_CACHE_HOME/bob/mtime/<blake3(abs_dir)>` so we don't litter
    /// the worktree (this runs on every workspace unit, not just the
    /// build target).
    ///
    /// `skip_dir` filters out per-language build dirs (e.g. `target/`).
    /// Dot-dirs are always skipped.
    pub fn source_hash(
        repo_root: &Path,
        unit_dir: &Path,
        skip_dir: &dyn Fn(&OsStr) -> bool,
    ) -> Result<String, String> {
        let abs_dir = repo_root.join(unit_dir);
        let dir_key = blake3::hash(abs_dir.to_string_lossy().as_bytes()).to_hex()[..32].to_string();
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(&abs_dir, skip_dir, &mut files);
        files.sort();

        // Fast path: hash file paths + mtimes + sizes
        let mut mtime_hasher = blake3::Hasher::new();
        for file in &files {
            let rel = file.strip_prefix(&abs_dir).unwrap_or(file);
            mtime_hasher.update(rel.to_string_lossy().as_bytes());
            if let Ok(meta) = std::fs::metadata(file) {
                use std::os::unix::fs::MetadataExt;
                mtime_hasher.update(&meta.mtime().to_le_bytes());
                mtime_hasher.update(&meta.mtime_nsec().to_le_bytes());
                mtime_hasher.update(&meta.size().to_le_bytes());
            }
        }
        let mtime_key = mtime_hasher.finalize().to_hex()[..32].to_string();

        // Check if we already computed a content hash for this mtime snapshot
        let cache_home = std::env::var("XDG_CACHE_HOME")
            .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
        let mtime_cache_path = PathBuf::from(cache_home)
            .join("bob")
            .join("mtime")
            .join(&dir_key);
        if let Ok(cached) = std::fs::read_to_string(&mtime_cache_path) {
            // Format: "<mtime_key> <content_hash>"
            if let Some((mk, ch)) = cached.split_once(' ') {
                if mk == mtime_key {
                    return Ok(ch.trim().to_string());
                }
            }
        }

        // Slow path: hash actual file contents
        let mut hasher = blake3::Hasher::new();
        for file in &files {
            let rel = file.strip_prefix(&abs_dir).unwrap_or(file);
            hasher.update(rel.to_string_lossy().as_bytes());
            let contents =
                std::fs::read(file).map_err(|e| format!("reading {}: {e}", file.display()))?;
            hasher.update(&contents);
        }
        let content_hash = hasher.finalize().to_hex()[..32].to_string();

        // Cache the mtime→content mapping
        let _ = std::fs::create_dir_all(mtime_cache_path.parent().unwrap());
        let _ = std::fs::write(&mtime_cache_path, format!("{mtime_key} {content_hash}"));

        Ok(content_hash)
    }

    fn cache_path(&self, target: &str, lock_hash: &str) -> PathBuf {
        // Hash the attr/target so dotted attr paths don't produce nested dirs.
        let key = blake3::hash(target.as_bytes()).to_hex()[..16].to_string();
        self.cache_dir.join(format!("{key}.{lock_hash}.drv"))
    }

    /// Resolve a backend-supplied attr path under `(import bob.nix {})` to a
    /// drv path.
    ///
    /// 1. Cache hit (same `lock_hash`) → return cached drv (~1ms). Source
    ///    changes are handled later via per-unit overrides against the build
    ///    graph, so they don't invalidate this cache.
    /// 2. Miss → nix-instantiate (~2s).
    pub fn resolve_one(
        &self,
        repo_root: &Path,
        target: &str,
        attr: &str,
        lock_hash: &str,
    ) -> Result<ResolveResult, String> {
        let cache_path = self.cache_path(target, lock_hash);

        if let Ok(drv_path) = std::fs::read_to_string(&cache_path) {
            if Path::new(&drv_path).exists() {
                eprintln!(" \x1b[1;32mResolved\x1b[0m {target} \x1b[2m(cached)\x1b[0m");
                return Ok(ResolveResult { drv_path });
            }
        }

        eprintln!(" \x1b[1;36mResolving\x1b[0m '{target}' via nix-instantiate...");

        let expr = format!(
            "(import {root}/bob.nix {{}}).{attr}",
            root = repo_root.display(),
        );

        let output = Command::new(nix_instantiate())
            .arg("--expr")
            .arg(&expr)
            .output()
            .map_err(|e| format!("running nix-instantiate: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("nix-instantiate failed for '{attr}': {stderr}"));
        }

        let drv_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !drv_path.ends_with(".drv") {
            return Err(format!("unexpected nix-instantiate output: {drv_path}"));
        }

        let _ = std::fs::create_dir_all(&self.cache_dir);
        std::fs::write(&cache_path, &drv_path).map_err(|e| format!("writing eval cache: {e}"))?;

        Ok(ResolveResult { drv_path })
    }
}

/// Recursively collect all files in a directory, skipping dot-dirs and
/// anything `skip_dir` rejects.
fn collect_files(dir: &Path, skip_dir: &dyn Fn(&OsStr) -> bool, files: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') || skip_dir(&name) {
                continue;
            }
            collect_files(&path, skip_dir, files);
        } else {
            files.push(path);
        }
    }
}
