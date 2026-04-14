//! Build executor: replays buildRustCrate phases outside the Nix sandbox.
//!
//! For each crate, we:
//! 1. Create a temp build directory
//! 2. Export the drv's env vars (with paths rewritten)
//! 3. Source stdenv/setup
//! 4. Run genericBuild (which sequences configure → build → install)
//!
//! The output ($out, $lib) is pointed at our cache tmp dir.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cache::ArtifactCache;
use crate::drv::Derivation;
use crate::rewrite::PathRewriter;

/// Per-crate pipelining configuration.
///
/// When set, the build wraps `rustc` to emit `metadata,link` and signal
/// `__META_READY__` on fd 3 as soon as the fat rmeta (with MIR) is written.
/// The scheduler dispatches that to start dependents before this crate's
/// codegen finishes (cargo-style pipelining).
pub struct PipelineConfig {
    /// Basename of THIS crate's rmeta (`lib{libName}-{metadata}.rmeta`). The
    /// wrapper only signals when rustc emits exactly this artifact — build
    /// scripts that probe rustc (`--emit=metadata probe.rs`) emit other rmetas
    /// that must not fire dependents.
    pub expected_rmeta: String,
    /// Skip the cdylib/staticlib second pass. Set for non-root crates: nothing
    /// in the dependency path consumes the `.so` (downstream Rust crates only
    /// read the rlib), so linking it just burns CPU on every iteration. Root
    /// targets always link — the `.so` IS the product (pyo3 modules).
    pub skip_link_pass: bool,
    /// Additional `(from, to)` rewrites applied AFTER the standard prefix
    /// rewrites — swaps each in-flight dep's `<dep_tmp>/lib/lib/<f>.rlib` (the
    /// path that ends up in LIB_RUSTC_OPTS after prefix rewriting) to the
    /// dep's immutable early-published `<dep_tmp>/rmeta/<f>.rmeta`.
    pub rmeta_rewrites: Vec<(String, String)>,
}

/// Override for a crate's cache key (and optionally its source) when reusing
/// a cached drv whose inputs have effectively changed.
///
/// `source_hash` is the *effective* hash: it incorporates this crate's own
/// source content AND the effective hashes of all its workspace deps. This
/// cascades invalidation through the DAG without changing drv paths, so a
/// change to a workspace crate's source produces a new key for that crate and
/// every downstream workspace crate, while crates.io deps (which never sit
/// downstream of workspace crates) keep their plain `blake3(drv_path)` key.
#[derive(Clone, Debug)]
pub struct SourceOverride {
    /// Local source directory to use instead of the store path in the drv.
    /// `None` when only the cache key changes (i.e., this crate's own source
    /// is unchanged but a dep's effective hash differs) — currently every
    /// overridden crate is a workspace crate so this is always `Some`.
    pub src_path: Option<PathBuf>,
    /// Effective source hash, mixed into the cache key.
    pub source_hash: String,
}

/// Result of executing a single crate build.
#[derive(Debug)]
pub struct BuildResult {
    pub success: bool,
    pub duration: std::time::Duration,
    pub stdout: String,
    pub stderr: String,
}

/// Build a single crate derivation using a persistent worker, with mid-build
/// `__META_READY__` signalling for rmeta pipelining.
#[allow(clippy::too_many_arguments)]
pub fn build_crate_with_worker_signaled(
    drv_path: &str,
    drv: &Derivation,
    cache: &ArtifactCache,
    rewriter: &PathRewriter,
    worker: &mut crate::worker::Worker,
    src_override: Option<&SourceOverride>,
    pl: &PipelineConfig,
    on_meta_ready: impl FnOnce(PathBuf),
) -> Result<BuildResult, String> {
    let effective_key = match src_override {
        Some(ov) => ArtifactCache::cache_key_with_source(drv_path, &ov.source_hash),
        None => ArtifactCache::cache_key(drv_path),
    };
    let crate_name = drv
        .env
        .get("crateName")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let start = std::time::Instant::now();

    // No is_cached short-circuit here: the scheduler already filtered cached
    // crates, and a root cdylib that was previously committed rlib-only must
    // be rebuilt even though is_cached_key() returns true. Clear any stale
    // artifact so the hardlink commit at the end gets a clean destination.
    let dest = cache.artifact_dir_by_key(&effective_key);
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&dest);
    }

    // tmp/<key> is reset by the scheduler under lock (before publishing it via
    // output_map) so dependents can't see stale bytes. lib/lib/ is precreated
    // so dependents' configure-phase symlink walk over `$dep/lib` doesn't race
    // a missing dir before this crate's installPhase populates it.
    let tmp = cache.root().join("tmp").join(&effective_key);
    std::fs::create_dir_all(tmp.join("lib").join("lib"))
        .map_err(|e| format!("creating tmp dir: {e}"))?;
    // Swap in-flight deps' .rlib paths for their early-written .rmeta. Applied
    // AFTER store-prefix rewrites so they match the already-rewritten cache/tmp
    // paths. Transitive in-flight deps under `-L dependency=target/deps` are
    // handled later by rustc_wrap::wait_closure_done_and_relink, which polls
    // each dep's `done` marker and re-symlinks rlibs into target/deps.
    let apply_rmeta_rewrites = |s: &mut String| {
        for (from, to) in &pl.rmeta_rewrites {
            if s.contains(from) {
                *s = s.replace(from, to);
            }
        }
    };

    let out_dir = tmp.join("out");
    let lib_dir = tmp.join("lib");

    let mut script = String::new();

    if drv.is_structured_attrs() {
        // Mirror what Nix's builder does for __structuredAttrs: write both
        // .attrs.json and .attrs.sh, source .attrs.sh BEFORE $stdenv/setup
        // so bash arrays (`outputs`, `env`, `*Inputs`, `completeDeps`, …) are
        // declared with correct types. stdenv/setup keys its structuredAttrs
        // path on NIX_ATTRS_JSON_FILE being set and then iterates these as
        // arrays — exporting them as scalars (the env.sh path below) makes
        // `${!outputs[@]}` / `${!env[@]}` yield index `0` and the build aborts.
        let json_str = drv
            .env
            .get("__json")
            .ok_or("structuredAttrs drv missing __json")?;
        let mut rewritten_json = rewrite_structured_attrs_json(
            json_str,
            out_dir.to_str().unwrap(),
            lib_dir.to_str().unwrap(),
            rewriter,
            src_override.and_then(|ov| ov.src_path.as_deref()),
        );
        apply_rmeta_rewrites(&mut rewritten_json);
        let json_val: serde_json::Value =
            serde_json::from_str(&rewritten_json).map_err(|e| format!("parsing __json: {e}"))?;
        let attrs_sh = json_to_attrs_sh(&json_val);

        let json_path = tmp.join(".attrs.json");
        let sh_path = tmp.join(".attrs.sh");
        std::fs::write(&json_path, &rewritten_json)
            .map_err(|e| format!("writing attrs json: {e}"))?;
        std::fs::write(&sh_path, &attrs_sh).map_err(|e| format!("writing attrs sh: {e}"))?;

        script.push_str(&format!(
            "export NIX_ATTRS_JSON_FILE='{}'\n",
            json_path.display()
        ));
        script.push_str(&format!(
            "export NIX_ATTRS_SH_FILE='{}'\n",
            sh_path.display()
        ));
        script.push_str(&format!("source '{}'\n", sh_path.display()));
    } else {
        // Non-structured drv: export every env var verbatim. $'...' quoting
        // (3ms for ~80 vars) avoids the per-var heredoc fork (200ms).
        let mut env = rewriter.rewrite_env(&drv.env);
        for v in env.values_mut() {
            apply_rmeta_rewrites(v);
        }
        let env_file = tmp.join("env.sh");
        let mut ef = String::new();
        for (k, v) in &env {
            if !is_valid_bash_ident(k) {
                continue;
            }
            let escaped = escape_for_dollar_single_quote(v);
            ef.push_str(&format!("export {k}=$'{escaped}'\n"));
        }
        std::fs::write(&env_file, &ef).map_err(|e| format!("writing env file: {e}"))?;
        script.push_str(&format!("source '{}'\n", env_file.display()));
        if let Some(ov) = src_override {
            if let Some(ref p) = ov.src_path {
                script.push_str(&format!("export src='{}'\n", p.display()));
            }
        }
        script.push_str(&format!("export out='{}'\n", out_dir.display()));
        script.push_str(&format!("export lib='{}'\n", lib_dir.display()));
        let outputs = drv
            .env
            .get("outputs")
            .cloned()
            .unwrap_or_else(|| "out".into());
        script.push_str(&format!("export outputs='{outputs}'\n"));
    }

    let inc_dir = cache.incremental_dir(drv_path);
    std::fs::create_dir_all(&inc_dir).map_err(|e| format!("creating incremental dir: {e}"))?;
    script.push_str(&format!(
        "export EXTRA_RUSTC_FLAGS=\"-C incremental={} ${{EXTRA_RUSTC_FLAGS:-}}\"\n",
        inc_dir.display()
    ));

    // NIX_BUILD_TOP must be drv-path-stable, NOT effective-key-stable:
    // buildRustCrate passes `--remap-path-prefix=$NIX_BUILD_TOP=/`, and rustc
    // hashes remap-path-prefix into its [TRACKED] options. If the build dir
    // moves whenever source changes (tmp/<effective_key>/build), the remap
    // value moves with it and rustc invalidates the whole incremental session
    // — paying dep-graph serialisation for nothing. Keying on drv_path (same
    // key incremental_dir uses) keeps both stable across source edits.
    let work_dir = cache
        .root()
        .join("build")
        .join(ArtifactCache::cache_key(drv_path));
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("creating work dir: {e}"))?;
    script.push_str(&format!("export NIX_BUILD_TOP='{}'\n", work_dir.display()));
    script.push_str(&format!("export TMPDIR='{}'\n", work_dir.display()));
    script.push_str(&format!("export TEMPDIR='{}'\n", work_dir.display()));
    script.push_str(&format!("export TMP='{}'\n", work_dir.display()));
    script.push_str(&format!("export TEMP='{}'\n", work_dir.display()));
    script.push_str("export HOME='/homeless-shelter'\n");
    script.push_str("export dontFixup=1\n");
    script.push_str(&format!("cd '{}'\n", work_dir.display()));

    // Source $stdenv/setup with this crate's real *Inputs in scope so stdenv's
    // input-processing machinery runs setup-hooks (cc-wrapper, pkg-config,
    // python3, rust-bindgen-hook, protobuf, ...). The worker's pre-sourced
    // stdenv had empty inputs, so PKG_CONFIG_PATH/LIBCLANG_PATH/PYTHONPATH/
    // NIX_CFLAGS_COMPILE etc. were never set — the previous "PATH=$p/bin"
    // loop was insufficient and broke every build.rs that probes the system.
    // Costs ~40ms/crate (what the worker was meant to save), but it's the
    // only way to avoid a hardcoded env-var whitelist.
    script.push_str(
        r#"
export NIX_STORE=/nix/store
export NIX_ENFORCE_PURITY=0
source "$stdenv/setup"
"#,
    );

    // Pipelining: wrap rustc to emit a fat rmeta early and signal via fd 3.
    // The wrapper is the bob binary itself (`__rustc-wrap`, see
    // rustc_wrap.rs), so arg classification, JSON-stderr parsing, and
    // rmeta→rlib polling happen in Rust without forking jq/cp/mv per line.
    // This /bin/sh shim just bridges PATH lookup of `rustc` to the subcommand.
    {
        let wrapper_dir = tmp.join("rustc-wrap");
        std::fs::create_dir_all(&wrapper_dir).map_err(|e| format!("creating wrapper dir: {e}"))?;
        let wrapper = wrapper_dir.join("rustc");
        let self_exe = std::env::current_exe().map_err(|e| format!("resolving self exe: {e}"))?;
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec '{}' __rustc-wrap \"$@\"\n",
                self_exe.display()
            ),
        )
        .map_err(|e| format!("writing rustc wrapper: {e}"))?;
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&wrapper).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&wrapper, perms).ok();

        // Config for rustc_wrap::main(). BOB_REAL_RUSTC is resolved AFTER
        // `source $stdenv/setup` so PATH already has the toolchain.
        // completeDeps/completeBuildDeps are bash arrays under structuredAttrs
        // (declared `-a` by .attrs.sh) and bash can't export arrays — assigning
        // a scalar to the same name just sets element [0]. Flatten into
        // distinct BOB_* scalars so the rustc-wrap subprocess inherits them.
        script.push_str(&format!(
            concat!(
                "export BOB_REAL_RUSTC=$(command -v rustc)\n",
                "export BOB_COMPLETE_DEPS=\"${{completeDeps[*]-}}\"\n",
                "export BOB_COMPLETE_BUILD_DEPS=\"${{completeBuildDeps[*]-}}\"\n",
                "export BOB_WRAP_RMETA_DIR='{rmeta_dir}'\n",
                "export BOB_EXPECTED_RMETA='{expected}'\n",
                "export BOB_SKIP_LINK_PASS='{skip_link}'\n",
                "export PATH='{wrap}':$PATH\n",
            ),
            rmeta_dir = tmp.join("rmeta").display(),
            expected = pl.expected_rmeta,
            skip_link = if pl.skip_link_pass { "1" } else { "" },
            wrap = wrapper_dir.display(),
        ));
    }

    // Use genericBuild but skip the per-phase overhead (dumpVars,
    // showPhaseHeader/Footer, date calls) by overriding those to no-ops.
    //
    // genericBuild's exit code must be captured explicitly: stdenv/setup turns
    // on `set -eu`, but buildRustCrate's buildPhase string is run via `eval`
    // and intermediate `runHook` calls mask failures, so a failed rustc would
    // fall through to installPhase and exit 0. The timing echo would mask it
    // either way. `set +e` makes the rc capture deterministic regardless of
    // what stdenv does to errexit.
    script.push_str(
        r#"
dumpVars() { :; }
showPhaseHeader() { :; }
showPhaseFooter() { :; }

_ms() { read _s _ < /proc/uptime; echo "${_s/./}"; }
_t0=$(_ms)
set +e
genericBuild
rc=$?
_t1=$(_ms)
echo "__TIMING__ phases=$((_t1-_t0))0ms" >&2
exit $rc
"#,
    );

    let script_path = tmp.join("builder.sh");
    std::fs::write(&script_path, &script).map_err(|e| format!("writing build script: {e}"))?;

    let result = worker
        .execute_with_signal(&script_path, &tmp, on_meta_ready)
        .map_err(|e| format!("worker build {crate_name}: {e}"))?;

    // genericBuild's exit code is unreliable across stdenv versions (errexit
    // interactions with eval'd phases). Belt-and-braces: only accept the
    // build if installPhase actually populated $lib (rlib/so for lib crates)
    // or $out/bin (bin-only crates).
    let lib_populated = std::fs::read_dir(lib_dir.join("lib"))
        .map(|d| {
            d.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.ends_with(".rlib") || n.ends_with(".so") || n.ends_with(".a"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let bin_populated = std::fs::read_dir(out_dir.join("bin"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    let success = result.exit_code == 0 && (lib_populated || bin_populated);
    if !success {
        if std::env::var_os("BOB_KEEP_FAILED").is_none() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
    } else {
        // commit_key's rename(tmp→artifacts) leaves a window where tmp/<key>
        // doesn't exist; once pipelining lets dependents start mid-build with
        // tmp/<key> paths embedded in their crate metadata, a transitive lookup
        // hitting that window E0463s. Hardlink-copy lib/out/rmeta into
        // artifacts/<key> (same-fs, ~free) and keep those subdirs of tmp/<key>
        // for the rest of this run. is_cached_key only checks dest.exists(),
        // so a partial commit (ENOSPC, SIGKILL mid-copy) must not leave dest
        // behind or it poisons the cache.
        if let Err(e) = hardlink_tree(&tmp, &dest) {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(format!("committing {crate_name}: {e}"));
        }
        // Signal full completion for any wrapper polling on us (proc-macro/
        // bin/cdylib consumers waiting for the rlib to be fully written).
        let _ = std::fs::write(tmp.join("done"), b"");
        // tmp/<key> is reset by the scheduler only when the SAME key rebuilds;
        // workspace edits produce new keys and crates.io deps never rebuild,
        // so build/ (NIX_BUILD_TOP — unpacked source, .o files, cmake trees for
        // -sys crates) would otherwise leak permanently. lib/out/rmeta/done
        // stay so embedded metadata paths and the wrapper's done-poll keep
        // resolving for the rest of this run.
        let _ = std::fs::remove_dir_all(&work_dir);
        let _ = std::fs::remove_dir_all(tmp.join("rustc-wrap"));
        for f in [
            "builder.sh",
            "env.sh",
            ".attrs.sh",
            ".attrs.json",
            "worker-stdout",
            "worker-stderr",
        ] {
            let _ = std::fs::remove_file(tmp.join(f));
        }
    }

    Ok(BuildResult {
        success,
        duration: start.elapsed(),
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

/// Recursive hardlink-copy (same-fs `cp -al`, without the fork). Only the
/// `lib`, `out`, and `rmeta` subtrees are persisted; `build/` (NIX_BUILD_TOP,
/// often hundreds of MB of unpacked source + .o files) is intentionally
/// skipped — the bash `cp -al` was hardlinking it too, which is cheap on disk
/// but still walks every file.
fn hardlink_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for sub in ["lib", "out", "rmeta"] {
        let s = src.join(sub);
        if s.exists() {
            hardlink_dir(&s, &dest.join(sub))?;
        }
    }
    Ok(())
}

fn hardlink_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let ft = e.file_type()?;
        let to = dest.join(e.file_name());
        if ft.is_dir() {
            hardlink_dir(&e.path(), &to)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(e.path())?;
            let _ = std::os::unix::fs::symlink(target, &to);
        } else {
            // EEXIST is fine (idempotent re-commit); EXDEV would mean cross-fs
            // which can't happen here (tmp/ and artifacts/ share cache root).
            if let Err(err) = std::fs::hard_link(e.path(), &to) {
                if err.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(err);
                }
            }
        }
    }
    Ok(())
}

/// Rewrite output paths and dependency paths in the __structuredAttrs JSON
/// so the build-rust-crate binary sees our cache paths instead of /nix/store.
/// Also remaps `outputs` from `["out","lib"]` to `{out: <tmp/out>, lib: <tmp/lib>}`
/// (matching what Nix writes to .attrs.json) and optionally overrides `src`.
fn rewrite_structured_attrs_json(
    json_str: &str,
    out_path: &str,
    lib_path: &str,
    rewriter: &PathRewriter,
    src_override: Option<&Path>,
) -> String {
    let mut val: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };

    if let serde_json::Value::Object(ref mut map) = val {
        let mut outputs_map = serde_json::Map::new();
        outputs_map.insert(
            "out".into(),
            serde_json::Value::String(out_path.to_string()),
        );
        outputs_map.insert(
            "lib".into(),
            serde_json::Value::String(lib_path.to_string()),
        );
        map.insert("outputs".into(), serde_json::Value::Object(outputs_map));

        if let Some(src) = src_override {
            map.insert(
                "src".into(),
                serde_json::Value::String(src.to_string_lossy().into_owned()),
            );
        }

        rewrite_json_values(map, rewriter);
    }

    serde_json::to_string(&val).unwrap_or_else(|_| json_str.to_string())
}

/// Render a structured-attrs JSON object as bash declarations, mirroring what
/// Nix's `writeStructuredAttrs` emits to `.attrs.sh`:
///   - string/number/bool/null → `declare -- k='v'`
///   - array of strings        → `declare -a k=('v1' 'v2' …)`
///   - object of strings       → `declare -A k=([k1]='v1' …)`
///   - anything else (nested objects, mixed arrays) → skipped
///
/// stdenv/setup's structuredAttrs path iterates `outputs`/`env` as associative
/// arrays and `*Inputs` as indexed arrays; sourcing this before setup gives it
/// the shapes it expects without us having to enumerate which keys are arrays.
fn json_to_attrs_sh(val: &serde_json::Value) -> String {
    use serde_json::Value;
    let mut out = String::new();
    let Value::Object(map) = val else {
        return out;
    };
    for (k, v) in map {
        if !is_valid_bash_ident(k) {
            continue;
        }
        match v {
            Value::String(s) => {
                out.push_str(&format!("declare -- {k}={}\n", sh_escape(s)));
            }
            Value::Number(n) => {
                out.push_str(&format!("declare -- {k}={n}\n"));
            }
            Value::Bool(b) => {
                out.push_str(&format!("declare -- {k}={}\n", if *b { "1" } else { "" }));
            }
            Value::Null => {
                out.push_str(&format!("declare -- {k}=\n"));
            }
            Value::Array(a) if a.iter().all(|v| v.is_string()) => {
                let items: Vec<String> = a.iter().map(|v| sh_escape(v.as_str().unwrap())).collect();
                out.push_str(&format!("declare -a {k}=({})\n", items.join(" ")));
            }
            Value::Object(o) if o.values().all(|v| v.is_string()) => {
                let items: Vec<String> = o
                    .iter()
                    .filter(|(ik, _)| is_valid_bash_ident(ik))
                    .map(|(ik, iv)| format!("[{ik}]={}", sh_escape(iv.as_str().unwrap())))
                    .collect();
                out.push_str(&format!("declare -A {k}=({})\n", items.join(" ")));
            }
            _ => {} // not representable in bash; Nix skips these too
        }
    }
    out
}

/// POSIX-sh single-quote escaping: wrap in '…', replace embedded ' with '\''.
fn sh_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Bash identifier: [A-Za-z_][A-Za-z0-9_]*. Keys like `__json` pass; keys like
/// `foo-bar` or `0abc` are skipped (Nix's .attrs.sh generator does the same).
fn is_valid_bash_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn rewrite_json_values(
    map: &mut serde_json::Map<String, serde_json::Value>,
    rewriter: &PathRewriter,
) {
    for (_key, val) in map.iter_mut() {
        match val {
            serde_json::Value::String(s) => {
                let rewritten = rewriter.rewrite(s);
                if rewritten != *s {
                    *s = rewritten;
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr.iter_mut() {
                    if let serde_json::Value::String(s) = item {
                        let rewritten = rewriter.rewrite(s);
                        if rewritten != *s {
                            *s = rewritten;
                        }
                    } else if let serde_json::Value::Object(ref mut inner) = item {
                        rewrite_json_values(inner, rewriter);
                    }
                }
            }
            serde_json::Value::Object(ref mut inner) => {
                rewrite_json_values(inner, rewriter);
            }
            _ => {}
        }
    }
}

/// Escape a string for bash $'...' quoting.
/// Only \, ', newline, tab, carriage return need escaping.
fn escape_for_dollar_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Build a PathRewriter for a crate given its drv and the cache locations
/// of its dependencies.
pub fn make_rewriter(_drv: &Derivation, dep_cache_map: &BTreeMap<String, PathBuf>) -> PathRewriter {
    let mut rw = PathRewriter::new();
    for (store_path, cache_path) in dep_cache_map {
        rw.add(store_path.clone(), cache_path.to_str().unwrap().to_string());
    }
    rw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attrs_sh_shapes() {
        let json = serde_json::json!({
            "crateName": "foo",
            "release": true,
            "codegenUnits": 16,
            "nativeBuildInputs": ["/nix/store/a", "/nix/store/b"],
            "outputs": {"out": "/tmp/out", "lib": "/tmp/lib"},
            "meta": {"NIX_MAIN_PROGRAM": "foo"},
            "crateBin": [{"name": "x"}],   // nested → skipped
            "bad-key": "nope",              // invalid ident → skipped
            "quoted": "it's fine",
        });
        let sh = json_to_attrs_sh(&json);
        assert!(sh.contains("declare -- crateName='foo'\n"));
        assert!(sh.contains("declare -- release=1\n"));
        assert!(sh.contains("declare -- codegenUnits=16\n"));
        assert!(sh.contains("declare -a nativeBuildInputs=('/nix/store/a' '/nix/store/b')\n"));
        assert!(sh.contains("declare -A outputs="));
        assert!(sh.contains("[out]='/tmp/out'"));
        assert!(sh.contains("[lib]='/tmp/lib'"));
        assert!(sh.contains("declare -A meta=([NIX_MAIN_PROGRAM]='foo')\n"));
        assert!(!sh.contains("crateBin"));
        assert!(!sh.contains("bad-key"));
        assert!(sh.contains("declare -- quoted='it'\\''s fine'\n"));
    }

    #[test]
    fn bash_ident_validation() {
        assert!(is_valid_bash_ident("foo"));
        assert!(is_valid_bash_ident("_foo123"));
        assert!(is_valid_bash_ident("__json"));
        assert!(!is_valid_bash_ident("0foo"));
        assert!(!is_valid_bash_ident("foo-bar"));
        assert!(!is_valid_bash_ident(""));
    }
}
