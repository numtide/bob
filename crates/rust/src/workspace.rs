//! Cargo workspace introspection: `[workspace].members`, `Cargo.lock` hashing,
//! cwd → package-name detection, and the crateName↔drv mapping for source
//! tracking.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use bob_core::resolve::EvalCache;
use bob_core::{BuildGraph, OwnHash};

/// blake3 of `Cargo.lock` — gates the eval-cache (drv reuse is sound as long
/// as the dependency graph is unchanged).
pub fn lock_hash(repo_root: &Path) -> Result<String, String> {
    let lock_path = repo_root.join("Cargo.lock");
    let contents = std::fs::read(&lock_path).map_err(|e| format!("reading Cargo.lock: {e}"))?;
    Ok(blake3::hash(&contents).to_hex()[..16].to_string())
}

/// `package_name → relative_path` from the root `Cargo.toml`'s
/// `[workspace].members`.
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
            let item = trimmed.trim_matches(|c: char| c == '"' || c == ',' || c.is_whitespace());
            if !item.is_empty() && !item.starts_with('#') {
                if let Some(name) = read_package_name(&repo_root.join(item)) {
                    members.insert(name, PathBuf::from(item));
                }
            }
        }
    }

    Ok(members)
}

/// `[package].name` from `dir/Cargo.toml`.
pub fn read_package_name(dir: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
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
                return Some(rhs.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Walk up from cwd looking for the nearest `Cargo.toml` with `[package].name`.
pub fn detect_from_cwd() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let mut dir = cwd.as_path();
    loop {
        if let Some(name) = read_package_name(dir) {
            return Some(name);
        }
        dir = dir.parent()?;
    }
}

/// Map `drv_path → (own_source_hash, live_src_dir)` for every workspace member
/// present in the graph. crateName (== Cargo.toml `[package].name`) is matched
/// against `[workspace].members`; the `version == "0.0.0"` heuristic is how
/// cargo-nix-plugin tags local crates so we prefer those over a same-named
/// crates.io dep.
pub fn unit_hashes(repo_root: &Path, g: &BuildGraph) -> HashMap<String, OwnHash> {
    let is_local = |drv: &str| {
        g.nodes[drv]
            .drv
            .env
            .get("version")
            .map(|v| v == "0.0.0")
            .unwrap_or(false)
    };
    let mut name_to_drv: HashMap<String, String> = HashMap::new();
    for (drv, node) in &g.nodes {
        let Some(name) = node.drv.env.get("crateName") else {
            continue;
        };
        let local = is_local(drv);
        match name_to_drv.get(name) {
            Some(_) if !local => {}
            Some(prev) if is_local(prev) => {
                // Resolver v1 unifies workspace features so this shouldn't
                // happen today. If crate2nix ever emits two feature-variant
                // drvs for one workspace member, the loser would silently
                // miss its own_hash and serve stale source on edits.
                eprintln!(
                    "  warn: workspace crate '{name}' has multiple drvs ({prev} and {drv}); \
                     source-change tracking will only cover one"
                );
                name_to_drv.insert(name.clone(), drv.clone());
            }
            _ => {
                name_to_drv.insert(name.clone(), drv.clone());
            }
        }
    }

    let mut own: HashMap<String, OwnHash> = HashMap::new();
    if let Ok(members) = workspace_members(repo_root) {
        for (name, rel) in &members {
            if let Some(drv) = name_to_drv.get(name) {
                match EvalCache::source_hash(repo_root, rel, &|n| n == "target") {
                    Ok(hash) => {
                        own.insert(
                            drv.clone(),
                            OwnHash {
                                hash,
                                src_dir: repo_root.join(rel),
                            },
                        );
                    }
                    Err(e) => eprintln!("  warn: hashing {}: {e}", rel.display()),
                }
            }
        }
    }
    own
}
