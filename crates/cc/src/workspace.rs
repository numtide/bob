//! cc unit discovery via `(import bob.nix {}).cc`.
//!
//! Units are identified by **drv path**, not by a marker in the drv env.
//! `lib/cc.nix`'s `unit` attaches `bobCcSrc` as a Nix-level attribute
//! (`drv // { bobCcSrc = …; }`), which leaves `drvPath` unchanged — so the
//! drv referenced by some Rust crate's `buildInputs` and the one under
//! `cc.<name>` are the same store path. We evaluate the `cc` attrset once
//! to get `{ <name> = { drvPath, src }; … }` and use that map for both
//! `is_unit` (drvPath ∈ map) and source-change tracking.
//!
//! The eval is cached on `blake3(bob.nix)`: the unit set is fully determined
//! by `bob.nix` plus whatever it imports, and core's `resolve::eval_key`
//! already covers the imports via `bob.toml` `eval-inputs`. A stale cache
//! (bob.nix unchanged but an imported file moved a drvPath) degrades to
//! "unit not recognised → boundary input" — safe, just loses incrementality
//! for that unit until bob.nix is touched or the cache is cleared.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use bob_core::resolve::EvalCache;
use bob_core::{BuildGraph, OwnHash};

/// One declared cc unit: its bob.nix attr name and live source dir.
#[derive(Debug)]
pub struct CcUnit {
    pub name: String,
    pub src: PathBuf,
}

/// Directories whose contents must not contribute to a unit's source hash.
fn skip_dir(name: &OsStr) -> bool {
    matches!(name.to_str(), Some("build" | "target"))
}

/// drvPath → { name, src }, evaluated once from `(import bob.nix {}).cc`.
/// Memoized per process (same justification as the rust backend's
/// `workspace_members`); the on-disk cache survives across runs.
pub fn cc_units(repo_root: &Path) -> &'static HashMap<String, CcUnit> {
    static CACHE: OnceLock<(PathBuf, HashMap<String, CcUnit>)> = OnceLock::new();
    let (cached_root, map) = CACHE.get_or_init(|| {
        let m = load(repo_root).unwrap_or_else(|e| {
            // Loud: a silent empty map here means cc edits never invalidate
            // anything and the user has no idea why. Keep going (rust-only
            // repos legitimately have no `cc` attr) but make it visible.
            eprintln!("\x1b[1;31m  cc backend disabled\x1b[0m: {e}");
            HashMap::new()
        });
        (repo_root.to_path_buf(), m)
    });
    debug_assert_eq!(cached_root, repo_root, "cc_units memo keyed on first root");
    map
}

fn load(repo_root: &Path) -> Result<HashMap<String, CcUnit>, String> {
    let bob_nix = repo_root.join("bob.nix");
    let key = blake3::hash(
        &std::fs::read(&bob_nix).map_err(|e| format!("reading {}: {e}", bob_nix.display()))?,
    )
    .to_hex()[..16]
        .to_string();

    let cache_home = std::env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
    let cache_path = PathBuf::from(cache_home)
        .join("bob")
        .join("eval")
        .join(format!("ccunits.{key}.json"));

    if let Ok(s) = std::fs::read_to_string(&cache_path) {
        if let Ok(m) = parse(&s) {
            return Ok(m);
        }
    }

    // The expression must tolerate `cc` being absent (pure-Rust repos) and
    // entries missing `bobCcSrc` (a bare drv someone put under `cc.<name>`
    // without going through `lib/cc.nix`). `--strict` forces the inner attrs
    // so `--json` doesn't emit thunks. `nix-instantiate` matches the
    // resolver path (BOB_NIX_INSTANTIATE) so any extra builtins bob.nix
    // needs are present.
    let nix_instantiate =
        std::env::var("BOB_NIX_INSTANTIATE").unwrap_or_else(|_| "nix-instantiate".into());
    let expr = format!(
        r#"builtins.mapAttrs
             (_: v: {{ drv = v.drvPath; src = v.bobCcSrc or null; }})
             ((import {root}/bob.nix {{}}).cc or {{}})"#,
        root = repo_root.display()
    );
    let out = Command::new(&nix_instantiate)
        .args(["--eval", "--json", "--strict", "--expr", &expr])
        .output()
        .map_err(|e| format!("running {nix_instantiate}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "evaluating cc units: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let json = String::from_utf8_lossy(&out.stdout).into_owned();
    let m = parse(&json)?;

    let _ = std::fs::create_dir_all(cache_path.parent().unwrap());
    let _ = std::fs::write(&cache_path, &json);
    Ok(m)
}

/// Parse `{ "<name>": { "drv": "<path>", "src": "<rel>" | null }, … }` into
/// drvPath → CcUnit. Hand-rolled to keep bob-cc free of a serde_json dep
/// (bob-core already pulls it, but the trait bound here is simple enough).
fn parse(s: &str) -> Result<HashMap<String, CcUnit>, String> {
    // The JSON is one flat object of objects with two known string fields.
    // We pull it apart with the same minimal scanner shape rustc_wrap uses,
    // rather than adding serde_json to this crate for one call site.
    let mut m = HashMap::new();
    let mut i = 0usize;
    let b = s.as_bytes();
    let str_at = |i: &mut usize| -> Option<String> {
        while *i < b.len() && b[*i] != b'"' {
            *i += 1;
        }
        if *i >= b.len() {
            return None;
        }
        *i += 1;
        let start = *i;
        while *i < b.len() && b[*i] != b'"' {
            // nix-instantiate --json never emits escapes in drv paths or our
            // src rels (no quotes/backslashes by construction), so a naive
            // scan to the closing quote is sufficient.
            *i += 1;
        }
        let v = std::str::from_utf8(&b[start..*i]).ok()?.to_string();
        *i += 1;
        Some(v)
    };
    // Outer { "name": { "drv": "…", "src": "…" }, … }
    while i < b.len() {
        let Some(name) = str_at(&mut i) else { break };
        let mut drv = None;
        let mut src = None;
        // Inner object: exactly two keys, order from nix is alphabetical
        // (drv, src) but don't rely on it.
        while i < b.len() && b[i] != b'}' {
            let Some(k) = str_at(&mut i) else { break };
            // value: either a string or `null`
            while i < b.len() && b[i] != b'"' && b[i] != b'n' && b[i] != b'}' {
                i += 1;
            }
            let v = if i < b.len() && b[i] == b'"' {
                str_at(&mut i)
            } else {
                // null
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                None
            };
            match k.as_str() {
                "drv" => drv = v,
                "src" => src = v,
                _ => {}
            }
        }
        if let (Some(drv), Some(src)) = (drv, src) {
            m.insert(
                drv,
                CcUnit {
                    name,
                    src: PathBuf::from(src),
                },
            );
        }
        while i < b.len() && b[i] != b',' && b[i] != b'}' {
            i += 1;
        }
        if i < b.len() && b[i] == b'}' {
            // close of inner or outer — advance past inner, stop on outer.
            i += 1;
        }
    }
    Ok(m)
}

/// `drv_path → (own-source hash, live src dir)` for every cc unit in the
/// graph. The src dir comes from the drvPath→src map, so the same drv that
/// `cargoNix*` references is tracked — no marker needed in the drv env.
pub fn unit_hashes(repo_root: &Path, g: &BuildGraph) -> HashMap<String, OwnHash> {
    let units = cc_units(repo_root);
    let mut own = HashMap::new();
    for drv_path in g.nodes.keys() {
        let Some(u) = units.get(drv_path) else {
            continue;
        };
        match EvalCache::source_hash(repo_root, &u.src, &skip_dir) {
            Ok(hash) => {
                own.insert(
                    drv_path.clone(),
                    OwnHash {
                        hash,
                        src_dir: repo_root.join(&u.src),
                    },
                );
            }
            Err(e) => eprintln!("  warn: hashing cc unit {}: {e}", u.name),
        }
    }
    own
}

/// Declared cc target names — for `list_targets` (typo suggestions) and
/// `resolve_attr` gating.
pub fn cc_target_names(repo_root: &Path) -> BTreeMap<String, ()> {
    cc_units(repo_root)
        .values()
        .map(|u| (u.name.clone(), ()))
        .collect()
}

/// Walk up from cwd; first dir whose `CMakeLists.txt`/`meson.build` declares
/// a `project()` wins. This is best-effort — the returned name only resolves
/// if the user named the `cc.<attr>` after the project, which `lib/cc.nix`
/// doesn't enforce.
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

fn project_name(dir: &Path) -> Option<String> {
    for f in ["CMakeLists.txt", "meson.build"] {
        let Ok(s) = std::fs::read_to_string(dir.join(f)) else {
            continue;
        };
        for line in s.lines() {
            let rest = line
                .trim_start()
                .strip_prefix("project(")
                .or_else(|| line.trim_start().strip_prefix("project ("))
                .or_else(|| line.trim_start().strip_prefix("PROJECT("))
                .or_else(|| line.trim_start().strip_prefix("Project("));
            let Some(rest) = rest else { continue };
            let tok: String = rest
                .trim_start()
                .trim_start_matches(['\'', '"'])
                .chars()
                .take_while(|c| !matches!(c, ',' | ')' | '\'' | '"') && !c.is_whitespace())
                .collect();
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

    #[test]
    fn parse_cc_units_json() {
        let json = r#"{"ndl":{"drv":"/nix/store/aaa-kmdlib.drv","src":"extra-code/b16/aws-neuron-kmdlib"},"pjrt":{"drv":"/nix/store/bbb-pjrt.drv","src":"extra-code/b16/pjrt"}}"#;
        let m = parse(json).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m["/nix/store/aaa-kmdlib.drv"].name, "ndl");
        assert_eq!(
            m["/nix/store/aaa-kmdlib.drv"].src,
            PathBuf::from("extra-code/b16/aws-neuron-kmdlib")
        );
        assert_eq!(m["/nix/store/bbb-pjrt.drv"].name, "pjrt");
    }

    #[test]
    fn parse_tolerates_null_src_and_empty() {
        // A bare drv under cc.<name> without bobCcSrc → src: null → skipped.
        let json = r#"{"bare":{"drv":"/nix/store/x.drv","src":null}}"#;
        assert!(parse(json).unwrap().is_empty());
        assert!(parse("{}").unwrap().is_empty());
    }

    #[test]
    fn project_name_variants() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("bob-cc-pn-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("CMakeLists.txt"),
            "Project( libfoo VERSION 1.0 LANGUAGES C CXX)\n",
        )
        .unwrap();
        assert_eq!(project_name(&d).as_deref(), Some("libfoo"));
        std::fs::remove_file(d.join("CMakeLists.txt")).unwrap();
        std::fs::write(d.join("meson.build"), "project('libbar', 'c')\n").unwrap();
        assert_eq!(project_name(&d).as_deref(), Some("libbar"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
