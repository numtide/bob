//! cc target discovery and source-change tracking.
//!
//! There is no `Cargo.toml`-equivalent manifest. The drv itself carries
//! `bobCcSrc` (set by `lib/cc.nix`), so the build graph is the source of
//! truth for `unit_hashes`. For `resolve_attr`/`list_targets`/
//! `detect_from_cwd` — which run *before* the graph exists — we walk the repo
//! once for `CMakeLists.txt`/`meson.build` files and index their `project()`
//! names. That's a heuristic (the `bob.nix` `cc.<name>` attr is what actually
//! gets evaluated), but it lets `bob build .` and typo-suggestions work
//! without a separate config file.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bob_core::resolve::EvalCache;
use bob_core::{BuildGraph, OwnHash};

use super::MARK;

/// Directories whose contents must not contribute to a unit's source hash.
/// `build/` is the conventional cmake/meson out-of-tree dir when developers
/// build by hand inside the worktree; `target/` shows up when a Rust crate
/// shares the directory.
fn skip_dir(name: &OsStr) -> bool {
    matches!(name.to_str(), Some("build" | "target"))
}

/// `drv_path → (own-source hash, live src dir)` for every cc unit in the
/// graph. The src dir comes straight from the `bobCcSrc` env attr, so this
/// needs no manifest and stays in lock-step with whatever `bob.nix` marked.
pub fn unit_hashes(repo_root: &Path, g: &BuildGraph) -> HashMap<String, OwnHash> {
    let mut own = HashMap::new();
    for (drv_path, node) in &g.nodes {
        let Some(rel) = node.drv.env.get(MARK) else {
            continue;
        };
        match EvalCache::source_hash(repo_root, Path::new(rel), &skip_dir) {
            Ok(hash) => {
                own.insert(
                    drv_path.clone(),
                    OwnHash {
                        hash,
                        src_dir: repo_root.join(rel),
                    },
                );
            }
            Err(e) => eprintln!("  warn: hashing cc unit {rel}: {e}"),
        }
    }
    own
}

/// `project()` name → directory, discovered by a one-shot walk of the repo.
/// Memoized per process — same justification as the Rust backend's
/// `workspace_members`: this is hit from `resolve_attr`, `list_targets`, and
/// `detect_from_cwd` on every `bob build`.
pub fn cc_targets(repo_root: &Path) -> &'static BTreeMap<String, PathBuf> {
    static CACHE: OnceLock<(PathBuf, BTreeMap<String, PathBuf>)> = OnceLock::new();
    let (cached_root, map) = CACHE.get_or_init(|| (repo_root.to_path_buf(), discover(repo_root)));
    debug_assert_eq!(
        cached_root, repo_root,
        "cc_targets memo keyed on first root"
    );
    map
}

fn discover(repo_root: &Path) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    // Cap the walk: a monorepo can have hundreds of thousands of dirs. We
    // only need top-level project files, and nested `CMakeLists.txt` under a
    // root one are `add_subdirectory` children, not standalone projects.
    walk(repo_root, repo_root, 6, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, depth: u8, out: &mut BTreeMap<String, PathBuf>) {
    if depth == 0 {
        return;
    }
    // A directory with its own project() is a leaf for our purposes — don't
    // descend, its subdirs' CMakeLists are part of *this* project.
    if let Some(name) = project_name(dir) {
        let rel = dir.strip_prefix(root).unwrap_or(dir).to_path_buf();
        out.entry(name).or_insert(rel);
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let n = e.file_name();
        let ns = n.to_string_lossy();
        if ns.starts_with('.') || skip_dir(&n) || ns == "node_modules" {
            continue;
        }
        walk(root, &e.path(), depth - 1, out);
    }
}

/// Walk up from cwd; first dir whose `CMakeLists.txt`/`meson.build` declares
/// a `project()` wins.
pub fn detect_from_cwd() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let mut dir = cwd.as_path();
    loop {
        if let Some(name) = project_name(dir) {
            return Some(name);
        }
        dir = dir.parent()?;
    }
}

/// Extract `project(<name> …)` from `CMakeLists.txt` or `meson.build`. Cheap
/// line-scan, not a real parser — both formats put the call on its own line
/// in practice, and we only need the first positional arg.
pub(crate) fn project_name(dir: &Path) -> Option<String> {
    for f in ["CMakeLists.txt", "meson.build"] {
        let p = dir.join(f);
        let Ok(s) = std::fs::read_to_string(&p) else {
            continue;
        };
        for line in s.lines() {
            let line = line.trim_start();
            // Both: `project(` is the keyword; cmake is case-insensitive.
            let rest = line
                .strip_prefix("project(")
                .or_else(|| line.strip_prefix("project ("))
                .or_else(|| line.strip_prefix("PROJECT("))
                .or_else(|| line.strip_prefix("Project("));
            let Some(rest) = rest else { continue };
            // First token up to `,` `)` or whitespace, with optional quotes.
            let tok: String = rest
                .trim_start()
                .trim_start_matches(['\'', '"'])
                .chars()
                .take_while(|c| !matches!(c, ',' | ')' | '\'' | '"') && !c.is_whitespace())
                .collect();
            // cmake `project(${VAR})` / meson run-time names — can't resolve.
            if tok.is_empty() || tok.contains(['$', '@']) {
                continue;
            }
            return Some(tok);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("bob-cc-ws-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn project_name_variants() {
        let d = tmpdir();
        // cmake: case-insensitive keyword, optional whitespace, VERSION etc.
        fs::write(
            d.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.20)\nProject( libfoo VERSION 1.0 LANGUAGES C CXX)\n",
        )
        .unwrap();
        assert_eq!(project_name(&d).as_deref(), Some("libfoo"));

        // meson: quoted first arg, kwargs after.
        fs::write(
            d.join("meson.build"),
            "project('libbar', 'c', version: '1.0')\n",
        )
        .unwrap();
        // CMakeLists.txt is checked first, so remove it.
        fs::remove_file(d.join("CMakeLists.txt")).unwrap();
        assert_eq!(project_name(&d).as_deref(), Some("libbar"));

        // Variable interpolation → unresolvable.
        fs::write(d.join("meson.build"), "project(@NAME@, 'c')\n").unwrap();
        assert_eq!(project_name(&d), None);

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn discover_stops_at_project_root() {
        let d = tmpdir();
        fs::create_dir_all(d.join("a/sub")).unwrap();
        fs::create_dir_all(d.join("b")).unwrap();
        fs::write(d.join("a/CMakeLists.txt"), "project(a)\n").unwrap();
        // sub is add_subdirectory fodder, not a standalone target.
        fs::write(d.join("a/sub/CMakeLists.txt"), "project(a_sub)\n").unwrap();
        fs::write(d.join("b/meson.build"), "project('b', 'c')\n").unwrap();

        let m = discover(&d);
        assert_eq!(m.get("a"), Some(&PathBuf::from("a")));
        assert_eq!(m.get("b"), Some(&PathBuf::from("b")));
        assert!(!m.contains_key("a_sub"));
        let _ = fs::remove_dir_all(&d);
    }
}
