//! Resolve target names to drv paths via nix-instantiate against `bob.nix`.
//!
//! The eval cache is keyed on `(target, eval_key(repo_root, lock_hash))`.
//! `lock_hash` is the backend's contribution (e.g. blake3 of Cargo.lock plus
//! any `[workspace.metadata.bob].eval-inputs`); [`eval_key`] mixes in the
//! backend-agnostic Nix-side inputs:
//!   - `bob.nix` itself (the file we evaluate), and
//!   - any `eval-inputs` declared in an optional `bob.toml` next to it — the
//!     out-of-tree alternative for users who can't put bob config into an
//!     upstream manifest.
//!
//! Source changes do NOT invalidate it: we always reuse the cached drv and
//! let `overrides::cascade` detect per-unit source changes and cascade them
//! through the build graph as cache-key overrides. This avoids the ~2s
//! nix-instantiate on every edit while staying correct for transitive
//! dependency changes. Nix-side changes (overrides, nixpkgs pin) DO
//! invalidate, via the eval-inputs above. We can't know what `bob.nix`
//! transitively imports without evaluating it (which is what we're caching),
//! so anything beyond `bob.nix` must be declared.

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

    fn cache_path(&self, target: &str, eval_key: &str) -> PathBuf {
        // Hash the attr/target so dotted attr paths don't produce nested dirs.
        let key = blake3::hash(target.as_bytes()).to_hex()[..16].to_string();
        self.cache_dir.join(format!("{key}.{eval_key}.drv"))
    }

    /// Resolve a backend-supplied attr path under `(import bob.nix {})` to a
    /// drv path.
    ///
    /// 1. Cache hit (same [`eval_key`]) → return cached drv (~1ms). Source
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
        let cache_path = self.cache_path(target, &eval_key(repo_root, lock_hash)?);

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

/// Combine the backend's `lock_hash` with the backend-agnostic Nix-side
/// inputs into the eval-cache key. See module doc for what's covered.
fn eval_key(repo_root: &Path, lock_hash: &str) -> Result<String, String> {
    let mut h = blake3::Hasher::new();
    h.update(lock_hash.as_bytes());

    let bob_nix =
        std::fs::read(repo_root.join("bob.nix")).map_err(|e| format!("reading bob.nix: {e}"))?;
    h.update(b"\0bob.nix\0");
    h.update(&bob_nix);

    if let Ok(s) = std::fs::read_to_string(repo_root.join("bob.toml")) {
        #[derive(serde::Deserialize, Default)]
        #[serde(rename_all = "kebab-case")]
        struct BobToml {
            #[serde(default)]
            eval_inputs: Vec<String>,
        }
        let cfg: BobToml = toml::from_str(&s).map_err(|e| format!("parsing bob.toml: {e}"))?;
        hash_eval_inputs(&mut h, repo_root, &cfg.eval_inputs)?;
    }

    Ok(h.finalize().to_hex()[..16].to_string())
}

/// Mix a list of eval-input globs (relative to `root`) into `h`: the glob
/// strings themselves (so adding/removing an entry invalidates even if the
/// referenced file is unchanged) followed by the sorted contents of every
/// match. Missing/unreadable files are silently skipped — they contribute
/// only via their glob string, so creating the file later still flips the
/// key. Shared by [`eval_key`] (`bob.toml`) and backends
/// (`[workspace.metadata.bob]`) so both speak the same glob dialect.
pub fn hash_eval_inputs(
    h: &mut blake3::Hasher,
    root: &Path,
    globs: &[String],
) -> Result<(), String> {
    if globs.is_empty() {
        return Ok(());
    }
    // require_literal_separator: `*` never crosses `/`, matching the
    // member-glob semantics users already know from Cargo.
    let opts = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    // Hash the declared globs as a set (sorted) so reordering the config
    // doesn't invalidate; the set itself is hashed so adding/removing an
    // entry does, even before any matching file exists.
    let mut sorted_globs: Vec<&str> = globs.iter().map(String::as_str).collect();
    sorted_globs.sort_unstable();
    let mut paths: Vec<PathBuf> = Vec::new();
    for g in sorted_globs {
        h.update(b"\0glob\0");
        h.update(g.as_bytes());
        let pat = root.join(g);
        for hit in glob::glob_with(&pat.to_string_lossy(), opts)
            .map_err(|e| format!("bad eval-inputs glob '{g}': {e}"))?
            .flatten()
        {
            if hit.is_file() {
                paths.push(hit);
            }
        }
    }
    paths.sort();
    paths.dedup();
    for p in &paths {
        if let Ok(bytes) = std::fs::read(p) {
            h.update(b"\0file\0");
            h.update(
                p.strip_prefix(root)
                    .unwrap_or(p)
                    .as_os_str()
                    .as_encoded_bytes(),
            );
            h.update(b"\0");
            h.update(&bytes);
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("bob-evalin-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn key(root: &Path, globs: &[&str]) -> String {
        let mut h = blake3::Hasher::new();
        let globs: Vec<String> = globs.iter().map(|s| s.to_string()).collect();
        hash_eval_inputs(&mut h, root, &globs).unwrap();
        h.finalize().to_hex().to_string()
    }

    /// The eval-inputs hash must move on exactly the edits that change what
    /// nix-instantiate would see, and stay put otherwise. Covers: content
    /// edit, glob match-set growth, declaration change with no fs change,
    /// and order-insensitivity of the declared set.
    #[test]
    fn eval_inputs_hash_invalidation() {
        let root = tmpdir();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("flake.lock"), "v1").unwrap();
        fs::write(root.join("sub/a.nix"), "a").unwrap();

        let g = ["flake.lock", "sub/*.nix"];
        let base = key(&root, &g);
        assert_eq!(base, key(&root, &g), "deterministic");

        // Editing a matched file flips the key.
        fs::write(root.join("flake.lock"), "v2").unwrap();
        let edited = key(&root, &g);
        assert_ne!(base, edited);

        // Adding a file the glob now matches flips it again.
        fs::write(root.join("sub/b.nix"), "b").unwrap();
        let added = key(&root, &g);
        assert_ne!(edited, added);

        // `*` must not cross `/` (require_literal_separator) — a nested file
        // doesn't match `sub/*.nix`, but `**/*.nix` does.
        fs::create_dir_all(root.join("sub/deep")).unwrap();
        fs::write(root.join("sub/deep/c.nix"), "c").unwrap();
        assert_eq!(added, key(&root, &g));
        let rec = key(&root, &["flake.lock", "sub/**/*.nix"]);
        fs::write(root.join("sub/deep/c.nix"), "c2").unwrap();
        assert_ne!(rec, key(&root, &["flake.lock", "sub/**/*.nix"]));

        // Declaring an extra glob flips the key even if it matches nothing,
        // so creating the file later still invalidates relative to the
        // pre-declaration state.
        assert_ne!(added, key(&root, &["flake.lock", "sub/*.nix", "missing"]));

        // Declaration order doesn't matter (both globs and matched paths are
        // sorted before hashing).
        assert_eq!(
            key(&root, &["sub/a.nix", "sub/b.nix"]),
            key(&root, &["sub/b.nix", "sub/a.nix"]),
        );

        let _ = fs::remove_dir_all(&root);
    }
}
