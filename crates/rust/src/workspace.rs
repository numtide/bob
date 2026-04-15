//! Cargo workspace introspection: `[workspace].members`, `Cargo.lock` hashing,
//! cwd → package-name detection, and the crateName↔drv mapping for source
//! tracking.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use bob_core::resolve::EvalCache;
use bob_core::{BuildGraph, OwnHash};
use serde::Deserialize;

#[derive(Deserialize)]
struct Manifest {
    package: Option<Package>,
    workspace: Option<Workspace>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
}

#[derive(Deserialize)]
struct Workspace {
    #[serde(default)]
    members: Vec<String>,
}

fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join("Cargo.toml");
    let s =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    toml::from_str(&s).map_err(|e| format!("parsing {}: {e}", path.display()))
}

/// blake3 of `Cargo.lock` — gates the eval-cache (drv reuse is sound as long
/// as the dependency graph is unchanged).
pub fn lock_hash(repo_root: &Path) -> Result<String, String> {
    let lock_path = repo_root.join("Cargo.lock");
    let contents = std::fs::read(&lock_path).map_err(|e| format!("reading Cargo.lock: {e}"))?;
    Ok(blake3::hash(&contents).to_hex()[..16].to_string())
}

/// `package_name → relative_path` from the root `Cargo.toml`'s
/// `[workspace].members`, with glob expansion (same `glob` crate Cargo uses).
pub fn workspace_members(repo_root: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    let entries = read_manifest(repo_root)?
        .workspace
        .map(|w| w.members)
        .unwrap_or_default();

    let mut members = BTreeMap::new();
    for entry in &entries {
        for rel in expand_member(repo_root, entry)? {
            if let Some(name) = read_package_name(&repo_root.join(&rel)) {
                members.insert(name, rel);
            }
        }
    }
    Ok(members)
}

/// Expand one `[workspace].members` entry. Cargo treats every entry as a glob,
/// but `glob::glob_with` walks the filesystem component-by-component (≈2
/// `statx` per component) even for literal patterns — on a 450-member
/// workspace with absolute repo_root that's ~15k extra syscalls per call. So
/// short-circuit literals and only glob when there's an actual metacharacter.
fn expand_member(repo_root: &Path, entry: &str) -> Result<Vec<PathBuf>, String> {
    if !entry.contains(['*', '?', '[']) {
        return Ok(vec![PathBuf::from(entry)]);
    }
    // require_literal_separator matches Cargo: `*` never crosses `/`.
    let opts = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    let pattern = repo_root.join(entry);
    let mut out = Vec::new();
    for hit in glob::glob_with(&pattern.to_string_lossy(), opts)
        .map_err(|e| format!("bad members glob '{entry}': {e}"))?
        .flatten()
    {
        out.push(
            hit.strip_prefix(repo_root)
                .map(Path::to_path_buf)
                .unwrap_or(hit),
        );
    }
    Ok(out)
}

/// `[package].name` from `dir/Cargo.toml`. Missing files return `None`
/// silently (glob hits without a manifest, walking past the repo root in
/// `detect_from_cwd`); malformed TOML is logged so a member doesn't quietly
/// drop out of source-change tracking.
pub fn read_package_name(dir: &Path) -> Option<String> {
    match read_manifest(dir) {
        Ok(m) => m.package.map(|p| p.name),
        Err(e) if dir.join("Cargo.toml").exists() => {
            eprintln!("  warn: {e}");
            None
        }
        Err(_) => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_member(root: &Path, rel: &str, name: &str) {
        let dir = root.join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\n"),
        )
        .unwrap();
    }

    fn tmpdir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("bob-ws-test-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Covers our integration logic, not TOML parsing: glob expansion,
    /// literal entries, skipping glob hits without a manifest, and the
    /// package-name → relative-path mapping.
    #[test]
    fn workspace_members_glob_and_literal() {
        let root = tmpdir();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\", \"tools/standalone\"]\n",
        )
        .unwrap();
        write_member(&root, "crates/a", "alpha");
        write_member(&root, "crates/b", "beta");
        write_member(&root, "crates/b/nested", "nope"); // `*` must not cross `/`
        write_member(&root, "tools/standalone", "standalone");
        // glob hit without a Cargo.toml — must be skipped
        fs::create_dir_all(root.join("crates/ignored")).unwrap();

        let m = workspace_members(&root).unwrap();
        assert_eq!(m.get("alpha"), Some(&PathBuf::from("crates/a")));
        assert_eq!(m.get("beta"), Some(&PathBuf::from("crates/b")));
        assert_eq!(
            m.get("standalone"),
            Some(&PathBuf::from("tools/standalone"))
        );
        assert!(!m.contains_key("nope"));
        assert_eq!(m.len(), 3);
        let _ = fs::remove_dir_all(&root);
    }
}
