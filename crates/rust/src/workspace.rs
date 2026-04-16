//! Cargo workspace introspection: `[workspace].members`, `Cargo.lock` hashing,
//! cwd → package-name detection, and the crateName↔drv mapping for source
//! tracking.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
    #[serde(default)]
    metadata: WorkspaceMetadata,
}

#[derive(Deserialize, Default)]
struct WorkspaceMetadata {
    #[serde(default)]
    bob: BobMetadata,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct BobMetadata {
    /// Extra paths (relative to Cargo.toml, globs allowed) whose contents
    /// invalidate bob's nix-instantiate cache. See `lock_hash`.
    #[serde(default)]
    eval_inputs: Vec<String>,
}

fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join("Cargo.toml");
    let s =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    toml::from_str(&s).map_err(|e| format!("parsing {}: {e}", path.display()))
}

/// Backend contribution to the eval-cache key: `Cargo.lock` (dep graph) plus
/// any `[workspace.metadata.bob].eval-inputs` files (crate overrides, pinned
/// toolchain, …). The cli further mixes in `bob.nix` and `bob.toml` extras;
/// see `main::eval_cache_key`. Projects that can't put bob config into their
/// upstream Cargo.toml use `bob.toml` instead — this path is for first-class
/// adopters who prefer one less file.
pub fn lock_hash(repo_root: &Path) -> Result<String, String> {
    let mut h = blake3::Hasher::new();
    let lock = std::fs::read(repo_root.join("Cargo.lock"))
        .map_err(|e| format!("reading Cargo.lock: {e}"))?;
    h.update(&lock);

    let extras = read_manifest(repo_root)
        .ok()
        .and_then(|m| m.workspace)
        .map(|w| w.metadata.bob.eval_inputs)
        .unwrap_or_default();
    bob_core::resolve::hash_eval_inputs(&mut h, repo_root, &extras)?;

    Ok(h.finalize().to_hex()[..16].to_string())
}

/// `package_name → relative_path` from the root `Cargo.toml`'s
/// `[workspace].members`, with glob expansion (same `glob` crate Cargo uses).
///
/// Memoized per process: this is called from `resolve_attr`, `list_targets`
/// and `unit_hashes` with the same `repo_root` on every `bob build`, and the
/// toml parse + per-member manifest reads cost ~15ms on a 450-member
/// workspace — noticeable on the no-op path. The result only changes when
/// `Cargo.toml` is edited, which already invalidates the eval cache anyway.
pub fn workspace_members(repo_root: &Path) -> Result<&'static BTreeMap<String, PathBuf>, String> {
    type Memo = (PathBuf, Result<BTreeMap<String, PathBuf>, String>);
    static CACHE: OnceLock<Memo> = OnceLock::new();
    let (cached_root, result) = CACHE.get_or_init(|| {
        (
            repo_root.to_path_buf(),
            compute_workspace_members(repo_root),
        )
    });
    // bob never calls this with two different roots in one process, but
    // assert it so a future refactor that does fails loudly instead of
    // returning the wrong workspace.
    debug_assert_eq!(
        cached_root, repo_root,
        "workspace_members memo keyed on first repo_root"
    );
    result.as_ref().map_err(String::clone)
}

fn compute_workspace_members(repo_root: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
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
        for (name, rel) in members {
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

        // Bypass the process-global memo so this test's tmpdir doesn't
        // collide with whichever repo_root another test cached first.
        let m = compute_workspace_members(&root).unwrap();
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
