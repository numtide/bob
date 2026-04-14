//! Resolve workspace member names to drv paths via nix-instantiate.
//!
//! The eval cache is keyed on `(member, Cargo.lock hash)` only. Source
//! changes do NOT invalidate it: we always reuse the cached drv and let
//! `compute_workspace_overrides()` (in main.rs) detect per-crate source
//! changes and cascade them through the build graph as cache-key overrides.
//! This avoids the ~2s nix-instantiate on every edit while staying correct
//! for transitive dependency changes.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Path to nix-instantiate. Must be a Nix that has
/// `builtins.resolveCargoWorkspace` (cargo-nix-plugin) for the repo's
/// `bob.nix` to evaluate. Override with `BOB_NIX_INSTANTIATE` to point at a
/// patched build; otherwise resolved from PATH.
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

    /// Build a map of package_name → relative_path from workspace Cargo.toml.
    pub fn workspace_members(repo_root: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
        let cargo_toml = repo_root.join("Cargo.toml");
        let contents = std::fs::read_to_string(&cargo_toml)
            .map_err(|e| format!("reading {}: {e}", cargo_toml.display()))?;

        let mut members = BTreeMap::new();
        let mut in_members = false;

        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("members") && trimmed.contains('[') {
                in_members = true;
                continue;
            }
            if in_members {
                if trimmed.starts_with(']') {
                    in_members = false;
                    continue;
                }
                let item =
                    trimmed.trim_matches(|c: char| c == '"' || c == ',' || c.is_whitespace());
                if !item.is_empty() && !item.starts_with('#') {
                    if let Some(name) = read_package_name(repo_root, item) {
                        members.insert(name, PathBuf::from(item));
                    }
                }
            }
        }

        Ok(members)
    }

    /// Hash Cargo.lock (captures dependency versions).
    fn lock_hash(repo_root: &Path) -> Result<String, String> {
        let lock_path = repo_root.join("Cargo.lock");
        let contents = std::fs::read(&lock_path).map_err(|e| format!("reading Cargo.lock: {e}"))?;
        let hash = blake3::hash(&contents);
        Ok(hash.to_hex()[..16].to_string())
    }

    /// Hash a crate's source directory to detect file changes.
    /// Uses a two-level approach: first hashes mtimes (fast ~0.1ms),
    /// then only reads file contents if the mtime hash changed since
    /// last time. The mtime→content mapping is cached under
    /// `$XDG_CACHE_HOME/bob/mtime/<blake3(abs_dir)>` so we don't litter
    /// the worktree (this now runs on every workspace crate, not just the
    /// build target).
    pub fn source_hash(repo_root: &Path, crate_dir: &Path) -> Result<String, String> {
        let abs_dir = repo_root.join(crate_dir);
        let dir_key = blake3::hash(abs_dir.to_string_lossy().as_bytes()).to_hex()[..32].to_string();
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(&abs_dir, &mut files);
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

    /// Load a cached drv path for a given (member, lock_hash).
    fn load(&self, member: &str, lock_hash: &str) -> Option<String> {
        let path = self.cache_dir.join(format!("{member}.{lock_hash}.drv"));
        std::fs::read_to_string(&path).ok()
    }

    /// Save a resolved drv path.
    fn save(&self, member: &str, lock_hash: &str, drv_path: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| format!("creating eval cache dir: {e}"))?;
        std::fs::write(
            self.cache_dir.join(format!("{member}.{lock_hash}.drv")),
            drv_path,
        )
        .map_err(|e| format!("writing eval cache: {e}"))?;
        Ok(())
    }

    /// Resolve a single workspace member name to its drv path.
    ///
    /// Two paths:
    /// 1. Cache hit (same Cargo.lock) → return cached drv (~1ms). Source
    ///    changes are handled later via per-crate overrides against the
    ///    build graph, so they don't invalidate this cache.
    /// 2. Miss → nix-instantiate (~2s)
    pub fn resolve_one(&self, repo_root: &Path, member: &str) -> Result<ResolveResult, String> {
        let members = Self::workspace_members(repo_root)?;
        if !members.contains_key(member) {
            let available: Vec<&str> = members.keys().map(|s| s.as_str()).take(10).collect();
            return Err(format!(
                "unknown workspace member '{member}'. some available: {}",
                available.join(", ")
            ));
        }

        let lock_hash = Self::lock_hash(repo_root)?;

        // Path 1: cached drv for this lock hash
        if let Some(drv_path) = self.load(member, &lock_hash) {
            if Path::new(&drv_path).exists() {
                eprintln!(" \x1b[1;32mResolved\x1b[0m {member} \x1b[2m(cached)\x1b[0m");
                return Ok(ResolveResult { drv_path });
            }
        }

        // Path 2: full nix-instantiate against the repo's bob.nix.
        // bob.nix must evaluate to an attrset with
        // `workspaceMembers.<name>.build` (the cargo-nix-plugin convention).
        eprintln!(" \x1b[1;36mResolving\x1b[0m '{member}' via nix-instantiate...");

        let expr = format!(
            "(import {root}/bob.nix {{}}).workspaceMembers.{member}.build",
            root = repo_root.display(),
        );

        let output = Command::new(nix_instantiate())
            .arg("--expr")
            .arg(&expr)
            .output()
            .map_err(|e| format!("running nix-instantiate: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("nix-instantiate failed for '{member}': {stderr}"));
        }

        let drv_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !drv_path.ends_with(".drv") {
            return Err(format!("unexpected nix-instantiate output: {drv_path}"));
        }

        self.save(member, &lock_hash, &drv_path)?;

        Ok(ResolveResult { drv_path })
    }
}

/// Read the `name` field from a crate's Cargo.toml [package] section.
fn read_package_name(repo_root: &Path, rel_path: &str) -> Option<String> {
    let cargo_toml = repo_root.join(rel_path).join("Cargo.toml");
    let contents = std::fs::read_to_string(&cargo_toml).ok()?;

    let mut in_package = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed == "[package]" {
            in_package = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_package = false;
            continue;
        }
        if in_package && trimmed.starts_with("name") {
            if let Some((_, rhs)) = trimmed.split_once('=') {
                let rhs = rhs.trim().trim_matches('"');
                return Some(rhs.to_string());
            }
        }
    }
    None
}

/// Recursively collect all files in a directory.
fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "target" || name.starts_with('.') {
                continue;
            }
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}
