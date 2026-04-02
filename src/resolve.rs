//! Resolve workspace member names to drv paths via nix-instantiate.
//!
//! Uses a fast content-hash cache to skip the ~2s nix evaluation when
//! source files haven't changed. Cache key: blake3 of Cargo.lock +
//! all files in the crate's source directory (sorted, deterministic).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Cached mapping from workspace member name → drv path.
pub struct EvalCache {
    cache_dir: PathBuf,
}

const NIX: &str = "nix-instantiate";

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

        // Parse the members array from [workspace]. Format:
        //   members = [
        //       "path/to/crate",
        //       ...
        //   ]
        let mut members = BTreeMap::new();
        let mut in_members = false;

        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("members") && trimmed.contains('[') {
                in_members = true;
                // Handle inline: members = ["a", "b"]
                if let Some(rest) = trimmed.strip_prefix("members") {
                    let rest = rest.trim().trim_start_matches('=').trim();
                    if rest.contains(']') {
                        // Single-line array
                        for item in rest.trim_matches(|c| c == '[' || c == ']').split(',') {
                            let item = item.trim().trim_matches('"');
                            if !item.is_empty() {
                                if let Some(name) = read_package_name(repo_root, item) {
                                    members.insert(name, PathBuf::from(item));
                                }
                            }
                        }
                        in_members = false;
                    }
                }
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

    /// Hash a crate's source directory + Cargo.lock to create a cache key
    /// that captures all inputs affecting the drv path.
    fn source_hash(repo_root: &Path, crate_dir: &Path) -> Result<String, String> {
        let mut hasher = blake3::Hasher::new();

        // Include Cargo.lock (dependency versions affect drv)
        let lock_path = repo_root.join("Cargo.lock");
        if lock_path.exists() {
            let contents = std::fs::read(&lock_path)
                .map_err(|e| format!("reading Cargo.lock: {e}"))?;
            hasher.update(b"Cargo.lock");
            hasher.update(&contents);
        }

        // Hash all files in the crate's source directory (sorted for determinism)
        let abs_dir = repo_root.join(crate_dir);
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(&abs_dir, &mut files);
        files.sort();

        for file in &files {
            // Use relative path as part of hash (catches renames)
            let rel = file.strip_prefix(&abs_dir).unwrap_or(file);
            hasher.update(rel.to_string_lossy().as_bytes());

            let contents = std::fs::read(file)
                .map_err(|e| format!("reading {}: {e}", file.display()))?;
            hasher.update(&contents);
        }

        Ok(hasher.finalize().to_hex()[..32].to_string())
    }

    /// Load a cached drv path for a given source hash.
    fn load_one(&self, member: &str, hash: &str) -> Option<String> {
        let path = self.cache_dir.join(format!("{member}-{hash}.drv"));
        std::fs::read_to_string(&path).ok()
    }

    /// Save a drv path to the cache.
    fn save_one(&self, member: &str, hash: &str, drv_path: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| format!("creating eval cache dir: {e}"))?;
        std::fs::write(
            self.cache_dir.join(format!("{member}-{hash}.drv")),
            drv_path,
        )
        .map_err(|e| format!("writing eval cache: {e}"))?;
        Ok(())
    }

    /// Resolve a single workspace member name to its drv path.
    ///
    /// Fast path (~1ms): hash the crate's source dir + Cargo.lock,
    /// check if we have a cached drv for that hash.
    /// Slow path (~2s): run nix-instantiate, cache the result.
    pub fn resolve_one(&self, repo_root: &Path, member: &str) -> Result<String, String> {
        // Find the crate's source directory
        let members = Self::workspace_members(repo_root)?;
        let crate_dir = members.get(member).ok_or_else(|| {
            let available: Vec<&str> = members.keys().map(|s| s.as_str()).take(10).collect();
            format!(
                "unknown workspace member '{member}'. some available: {}",
                available.join(", ")
            )
        })?;

        // Hash source dir + Cargo.lock
        let hash = Self::source_hash(repo_root, crate_dir)?;

        // Fast path: check cache
        if let Some(drv_path) = self.load_one(member, &hash) {
            // Verify the drv still exists in the nix store
            if Path::new(&drv_path).exists() {
                eprintln!(" \x1b[1;32mResolved\x1b[0m {member} \x1b[2m(cached)\x1b[0m");
                return Ok(drv_path);
            }
        }

        // Slow path: nix-instantiate
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
            member = member,
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
            return Err(format!(
                "unexpected output from nix-instantiate: {drv_path}"
            ));
        }

        // Cache for next time
        self.save_one(member, &hash, &drv_path)?;

        Ok(drv_path)
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
            // Skip target/ and hidden dirs
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
