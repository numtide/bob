//! Resolve workspace member names to drv paths via nix-instantiate.
//!
//! Uses a fast content-hash cache to skip the ~2s nix evaluation when
//! source files haven't changed. When source changes but deps don't
//! (Cargo.lock unchanged), reuses the previous drv and overrides `src`
//! with a local filtered copy — eliminating the nix eval entirely.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const NIX: &str = "nix-instantiate";

/// Result of resolving a workspace member.
pub struct ResolveResult {
    /// The .drv path (may be from a previous eval if using local src override).
    pub drv_path: String,
    /// If set, override `src` env var with this local directory instead of
    /// the store path baked into the drv. Used when we skip nix-instantiate.
    pub src_override: Option<PathBuf>,
    /// Extra suffix to mix into cache keys, capturing source changes not
    /// reflected in drv_path. Empty when drv_path is exact.
    pub source_hash: String,
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
    fn workspace_members(repo_root: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
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
                let item = trimmed.trim_matches(|c: char| c == '"' || c == ',' || c.is_whitespace());
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
        let contents = std::fs::read(&lock_path)
            .map_err(|e| format!("reading Cargo.lock: {e}"))?;
        let hash = blake3::hash(&contents);
        Ok(hash.to_hex()[..16].to_string())
    }

    /// Hash a crate's source directory to detect file changes.
    /// Uses a two-level approach: first hashes mtimes (fast ~0.1ms),
    /// then only reads file contents if the mtime hash changed since
    /// last time (cached in `eval/mtime-<dir-hash>`).
    fn source_hash(repo_root: &Path, crate_dir: &Path) -> Result<String, String> {
        let abs_dir = repo_root.join(crate_dir);
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
        let mtime_cache_path = abs_dir
            .join("target")
            .join(".nix-inc-mtime-cache");
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
            let contents = std::fs::read(file)
                .map_err(|e| format!("reading {}: {e}", file.display()))?;
            hasher.update(&contents);
        }
        let content_hash = hasher.finalize().to_hex()[..32].to_string();

        // Cache the mtime→content mapping
        let _ = std::fs::create_dir_all(mtime_cache_path.parent().unwrap());
        let _ = std::fs::write(&mtime_cache_path, format!("{mtime_key} {content_hash}"));

        Ok(content_hash)
    }

    /// Load a cached drv path for a given (member, lock_hash, source_hash).
    fn load_exact(&self, member: &str, lock_hash: &str, src_hash: &str) -> Option<String> {
        let path = self.cache_dir.join(format!("{member}.{lock_hash}.{src_hash}.drv"));
        std::fs::read_to_string(&path).ok()
    }

    /// Find any cached drv for this member with the same lock_hash
    /// (same deps, possibly different source).
    fn load_any_for_lock(&self, member: &str, lock_hash: &str) -> Option<String> {
        let prefix = format!("{member}.{lock_hash}.");
        let entries = std::fs::read_dir(&self.cache_dir).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && name.ends_with(".drv") {
                return std::fs::read_to_string(entry.path()).ok();
            }
        }
        None
    }

    /// Save a resolved drv path.
    fn save(&self, member: &str, lock_hash: &str, src_hash: &str, drv_path: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| format!("creating eval cache dir: {e}"))?;
        std::fs::write(
            self.cache_dir.join(format!("{member}.{lock_hash}.{src_hash}.drv")),
            drv_path,
        )
        .map_err(|e| format!("writing eval cache: {e}"))?;
        Ok(())
    }

    /// Resolve a single workspace member name to its drv path.
    ///
    /// Three paths, fastest first:
    /// 1. Exact cache hit (same source + deps) → return drv_path (~1ms)
    /// 2. Same deps, different source → reuse drv, override src (~1ms)
    /// 3. Full miss → nix-instantiate (~2s)
    pub fn resolve_one(&self, repo_root: &Path, member: &str) -> Result<ResolveResult, String> {
        let members = Self::workspace_members(repo_root)?;
        let crate_dir = members.get(member).ok_or_else(|| {
            let available: Vec<&str> = members.keys().map(|s| s.as_str()).take(10).collect();
            format!(
                "unknown workspace member '{member}'. some available: {}",
                available.join(", ")
            )
        })?;

        let lock_hash = Self::lock_hash(repo_root)?;
        let src_hash = Self::source_hash(repo_root, crate_dir)?;

        // Path 1: exact match — same deps AND same source
        if let Some(drv_path) = self.load_exact(member, &lock_hash, &src_hash) {
            if Path::new(&drv_path).exists() {
                eprintln!(" \x1b[1;32mResolved\x1b[0m {member} \x1b[2m(cached)\x1b[0m");
                return Ok(ResolveResult {
                    drv_path,
                    src_override: None,
                    source_hash: src_hash,
                });
            }
        }

        // Path 2: same deps, different source — reuse drv, override src
        if let Some(base_drv) = self.load_any_for_lock(member, &lock_hash) {
            if Path::new(&base_drv).exists() {
                let local_src = create_source_snapshot(repo_root, crate_dir, &self.cache_dir)?;
                eprintln!(" \x1b[1;32mResolved\x1b[0m {member} \x1b[2m(reusing drv, local src)\x1b[0m");
                return Ok(ResolveResult {
                    drv_path: base_drv,
                    src_override: Some(local_src),
                    source_hash: src_hash,
                });
            }
        }

        // Path 3: full nix-instantiate
        eprintln!(" \x1b[1;36mResolving\x1b[0m '{member}' via nix-instantiate...");

        let expr = format!(
            r#"
            let
              pkgs = import <nixpkgs> {{}};
              utils = import {root}/bob.nix {{ inherit pkgs; }};
              cargoNix = utils.cargoNix {{}};
            in cargoNix.workspaceMembers.{member}.build
            "#,
            root = repo_root.display(),
        );

        let output = Command::new(NIX)
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

        self.save(member, &lock_hash, &src_hash, &drv_path)?;

        Ok(ResolveResult {
            drv_path,
            src_override: None,
            source_hash: src_hash,
        })
    }
}

/// Create a filtered copy of the crate source directory (excludes target/, .git).
/// Returns the path to the snapshot directory.
fn create_source_snapshot(repo_root: &Path, crate_dir: &Path, cache_dir: &Path) -> Result<PathBuf, String> {
    let src = repo_root.join(crate_dir);
    let snapshot_dir = cache_dir.join("snapshots");
    let dest = snapshot_dir.join(crate_dir.file_name().unwrap_or_default());

    // Remove previous snapshot
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .map_err(|e| format!("removing old snapshot: {e}"))?;
    }
    std::fs::create_dir_all(&dest)
        .map_err(|e| format!("creating snapshot dir: {e}"))?;

    copy_filtered(&src, &dest)?;
    Ok(dest)
}

/// Recursively copy a directory, excluding target/ and hidden dirs.
fn copy_filtered(src: &Path, dest: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(src)
        .map_err(|e| format!("reading {}: {e}", src.display()))?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str == "target" || name_str.starts_with('.') {
            continue;
        }

        let src_path = entry.path();
        let dest_path = dest.join(&name);

        if src_path.is_dir() {
            std::fs::create_dir_all(&dest_path)
                .map_err(|e| format!("creating dir {}: {e}", dest_path.display()))?;
            copy_filtered(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)
                .map_err(|e| format!("copying {}: {e}", src_path.display()))?;
        }
    }
    Ok(())
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
