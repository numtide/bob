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
use std::process::Command;

use crate::cache::ArtifactCache;

/// Override for a crate's source and cache key when reusing an old drv
/// with modified local source (skipping nix-instantiate).
#[derive(Clone, Debug)]
pub struct SourceOverride {
    /// Local source directory to use instead of the store path in the drv.
    pub src_path: PathBuf,
    /// Source content hash, mixed into the cache key.
    pub source_hash: String,
}
use crate::drv::Derivation;
use crate::rewrite::PathRewriter;

/// Result of executing a single crate build.
#[derive(Debug)]
pub struct BuildResult {
    pub drv_path: String,
    pub crate_name: String,
    pub success: bool,
    pub duration: std::time::Duration,
    pub stdout: String,
    pub stderr: String,
    /// Set if the build signaled __META_READY__ mid-build (pipelining).
    pub rmeta_dir: Option<PathBuf>,
}

/// Build a single crate derivation using its env vars and stdenv phases.
/// If `stdenv_dump` is provided, source that instead of stdenv/setup (faster).
pub fn build_crate(
    drv_path: &str,
    drv: &Derivation,
    cache: &ArtifactCache,
    rewriter: &PathRewriter,
    stdenv_dump: Option<&Path>,
) -> Result<BuildResult, String> {
    let crate_name = drv.env.get("crateName")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let start = std::time::Instant::now();

    // Skip if already cached
    if cache.is_cached(drv_path) {
        return Ok(BuildResult {
            drv_path: drv_path.into(),
            crate_name,
            success: true,
            duration: start.elapsed(),
            stdout: String::new(),
            stderr: String::new(),
            rmeta_dir: None,
        });
    }

    let tmp = cache.prepare_tmp(drv_path)
        .map_err(|e| format!("preparing tmp dir: {e}"))?;

    // Rewrite all env vars
    let env = rewriter.rewrite_env(&drv.env);

    // Build the shell script that sources stdenv and runs the build
    let stdenv_path = env.get("stdenv")
        .ok_or("drv missing 'stdenv' env var")?;
    let setup_path = format!("{stdenv_path}/setup");

    // Write the build script to a file, matching nix's invocation:
    // bash -e source-stdenv.sh default-builder.sh
    // where source-stdenv.sh does: source "$stdenv/setup"; source "$1"
    // and default-builder.sh does: genericBuild
    let script_path = tmp.join("builder.sh");
    let script = build_script(&env, &setup_path, &tmp, stdenv_dump);
    std::fs::write(&script_path, &script)
        .map_err(|e| format!("writing build script: {e}"))?;

    let bash = &drv.builder;

    let mut cmd = Command::new(bash);
    cmd.arg("-e")
        .arg(&script_path);

    // Set a clean environment, then add the drv's env vars
    cmd.env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }

    // Override outputs to point at our tmp dir
    cmd.env("out", tmp.join("out").to_str().unwrap());
    if env.contains_key("lib") {
        cmd.env("lib", tmp.join("lib").to_str().unwrap());
    }

    // Set NIX_BUILD_TOP to a working directory
    let work_dir = tmp.join("build");
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| format!("creating work dir: {e}"))?;
    cmd.env("NIX_BUILD_TOP", work_dir.to_str().unwrap());
    cmd.env("TMPDIR", work_dir.to_str().unwrap());
    cmd.env("TEMPDIR", work_dir.to_str().unwrap());
    cmd.env("TMP", work_dir.to_str().unwrap());
    cmd.env("TEMP", work_dir.to_str().unwrap());
    cmd.current_dir(&work_dir);

    // Provide outputs list
    let outputs = env.get("outputs")
        .cloned()
        .unwrap_or_else(|| "out".into());
    cmd.env("outputs", &outputs);

    // HOME is needed by some build scripts
    cmd.env("HOME", "/homeless-shelter");

    // NIX_STORE is set by the nix daemon; gcc-wrapper uses it under set -u
    cmd.env("NIX_STORE", "/nix/store");

    // Disable purity enforcement — gcc-wrapper rejects paths outside /nix/store
    // but our cached artifacts live in ~/.cache/
    cmd.env("NIX_ENFORCE_PURITY", "0");

    let output = cmd.output()
        .map_err(|e| format!("executing build for {crate_name}: {e}"))?;

    let success = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !success {
        let _ = std::fs::remove_dir_all(&tmp);
    } else {
        cache.commit(drv_path)
            .map_err(|e| format!("committing {crate_name} to cache: {e}"))?;
    }

    Ok(BuildResult {
        drv_path: drv_path.into(),
        crate_name,
        success,
        duration: start.elapsed(),
        stdout,
        stderr,
        rmeta_dir: None,
    })
}

/// Generate the build script that sources stdenv and runs the build phases.
fn build_script(
    _env: &BTreeMap<String, String>,
    setup_path: &str,
    tmp: &Path,
    stdenv_dump: Option<&Path>,
) -> String {
    let out_dir = tmp.join("out");
    let lib_dir = tmp.join("lib");

    let source_line = match stdenv_dump {
        Some(dump) => format!("source \"{}\"", dump.display()),
        None => format!("source \"{setup_path}\""),
    };

    format!(
        r#"
export out="{out}"
export lib="{lib}"

_ms() {{ read _s _ < /proc/uptime; echo "${{_s/./}}"; }}
_t0=$(_ms)
{source_line}
_t1=$(_ms)

export dontFixup=1
genericBuild
_t2=$(_ms)

echo "__TIMING__ setup=$((_t1-_t0))0ms phases=$((_t2-_t1))0ms" >&2
"#,
        out = out_dir.display(),
        lib = lib_dir.display(),
    )
}


/// Build a crate using a persistent worker (stdenv already sourced).
/// The worker forks a subshell per build, saving ~40ms of stdenv sourcing.
pub fn build_crate_with_worker(
    drv_path: &str,
    drv: &Derivation,
    cache: &ArtifactCache,
    rewriter: &PathRewriter,
    worker: &mut crate::worker::Worker,
    src_override: Option<&SourceOverride>,
) -> Result<BuildResult, String> {
    // When source is overridden, use a different cache key that
    // incorporates the source hash (the drv_path alone would match
    // the old, stale artifact).
    let effective_key = match src_override {
        Some(ov) => ArtifactCache::cache_key_with_source(drv_path, &ov.source_hash),
        None => ArtifactCache::cache_key(drv_path),
    };
    let crate_name = drv.env.get("crateName")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let start = std::time::Instant::now();

    if cache.is_cached_key(&effective_key) {
        return Ok(BuildResult {
            drv_path: drv_path.into(),
            crate_name,
            success: true,
            duration: start.elapsed(),
            stdout: String::new(),
            stderr: String::new(),
            rmeta_dir: None,
        });
    }

    let tmp_dir = cache.root().join("tmp").join(&effective_key);
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)
            .map_err(|e| format!("removing old tmp: {e}"))?;
    }
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| format!("creating tmp dir: {e}"))?;
    let tmp = tmp_dir;
    let env = rewriter.rewrite_env(&drv.env);

    let out_dir = tmp.join("out");
    let lib_dir = tmp.join("lib");

    // Write a crate-specific script that sets env vars and runs genericBuild.
    // The worker's parent bash already has stdenv sourced, so the subshell
    // inherits genericBuild, PATH, hooks, etc.
    let mut script = String::new();

    // Export all crate env vars using $'...' quoting (no subprocess forks).
    // Heredoc+cat was 200ms for 76 vars; $'...' is 3ms.
    let env_file = tmp.join("env.sh");
    {
        let mut ef = String::new();
        for (k, v) in &env {
            let escaped = escape_for_dollar_single_quote(v);
            ef.push_str(&format!("export {k}=$'{escaped}'\n"));
        }
        std::fs::write(&env_file, &ef)
            .map_err(|e| format!("writing env file: {e}"))?;
    }
    script.push_str(&format!("source '{}'\n", env_file.display()));

    // Override src with local snapshot when skipping nix-instantiate
    if let Some(ov) = src_override {
        script.push_str(&format!("export src='{}'\n", ov.src_path.display()));
    }

    script.push_str(&format!("export out='{}'\n", out_dir.display()));
    script.push_str(&format!("export lib='{}'\n", lib_dir.display()));

    // For __structuredAttrs drvs: write the JSON attrs file so
    // build-rust-crate can read it, with output paths rewritten.
    if drv.is_structured_attrs() {
        if let Some(json_str) = drv.env.get("__json") {
            let json_path = tmp.join(".attrs.json");
            let rewritten_json = rewrite_structured_attrs_json(
                json_str,
                out_dir.to_str().unwrap(),
                lib_dir.to_str().unwrap(),
                rewriter,
            );
            std::fs::write(&json_path, &rewritten_json)
                .map_err(|e| format!("writing attrs json: {e}"))?;
            script.push_str(&format!(
                "export NIX_ATTRS_JSON_FILE='{}'\n",
                json_path.display()
            ));
        }
    }

    // Enable incremental compilation: rustc reuses previous work
    // from this persistent dir across rebuilds of the same crate.
    let inc_dir = cache.incremental_dir(drv_path);
    std::fs::create_dir_all(&inc_dir)
        .map_err(|e| format!("creating incremental dir: {e}"))?;
    script.push_str(&format!(
        "export EXTRA_RUSTC_FLAGS=\"-C incremental={} ${{EXTRA_RUSTC_FLAGS:-}}\"\n",
        inc_dir.display()
    ));

    // Pipelining: tell build-rust-crate where to emit .rmeta and to
    // signal readiness via fd 3 (inherited from the worker).
    let rmeta_dir = cache.root().join("rmeta").join(&effective_key);
    std::fs::create_dir_all(&rmeta_dir)
        .map_err(|e| format!("creating rmeta dir: {e}"))?;
    script.push_str(&format!(
        "export NIX_INC_RMETA_DIR='{}'\n",
        rmeta_dir.display()
    ));

    let work_dir = tmp.join("build");
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| format!("creating work dir: {e}"))?;
    script.push_str(&format!("export NIX_BUILD_TOP='{}'\n", work_dir.display()));
    script.push_str(&format!("export TMPDIR='{}'\n", work_dir.display()));
    script.push_str(&format!("export TEMPDIR='{}'\n", work_dir.display()));
    script.push_str(&format!("export TMP='{}'\n", work_dir.display()));
    script.push_str(&format!("export TEMP='{}'\n", work_dir.display()));
    script.push_str(&format!("export HOME='/homeless-shelter'\n"));

    let outputs = env.get("outputs")
        .cloned()
        .unwrap_or_else(|| "out".into());
    script.push_str(&format!("export outputs='{outputs}'\n"));
    script.push_str("export dontFixup=1\n");
    script.push_str(&format!("cd '{}'\n", work_dir.display()));

    // Re-process nativeBuildInputs into PATH. The worker's parent sourced
    // stdenv with empty nativeBuildInputs; we need to add the crate's
    // toolchain (rustc, cargo, gcc-wrapper, mold, etc.) to PATH.
    script.push_str(r#"
for p in $nativeBuildInputs $depsBuildBuild; do
  if [ -d "$p/bin" ]; then
    export PATH="$p/bin:$PATH"
  fi
done
"#);

    // Use genericBuild but skip the per-phase overhead (dumpVars,
    // showPhaseHeader/Footer, date calls) by overriding those to no-ops.
    script.push_str(r#"
dumpVars() { :; }
showPhaseHeader() { :; }
showPhaseFooter() { :; }

_ms() { read _s _ < /proc/uptime; echo "${_s/./}"; }
_t0=$(_ms)
genericBuild
_t1=$(_ms)
echo "__TIMING__ phases=$((_t1-_t0))0ms" >&2
"#);

    let script_path = tmp.join("builder.sh");
    std::fs::write(&script_path, &script)
        .map_err(|e| format!("writing build script: {e}"))?;

    let result = worker.execute(&script_path, &tmp)
        .map_err(|e| format!("worker build {crate_name}: {e}"))?;

    let success = result.exit_code == 0;
    if !success {
        let _ = std::fs::remove_dir_all(&tmp);
    } else {
        cache.commit_key(&effective_key)
            .map_err(|e| format!("committing {crate_name} to cache: {e}"))?;
    }

    Ok(BuildResult {
        drv_path: drv_path.into(),
        crate_name,
        success,
        duration: start.elapsed(),
        stdout: result.stdout,
        stderr: result.stderr,
        rmeta_dir: None,
    })
}

/// Like `build_crate_with_worker`, but accepts a callback that fires
/// when the build signals `__META_READY__` mid-build (pipelining).
pub fn build_crate_with_worker_signaled(
    drv_path: &str,
    drv: &Derivation,
    cache: &ArtifactCache,
    rewriter: &PathRewriter,
    worker: &mut crate::worker::Worker,
    src_override: Option<&SourceOverride>,
    on_meta_ready: impl FnOnce(PathBuf),
) -> Result<BuildResult, String> {
    let effective_key = match src_override {
        Some(ov) => ArtifactCache::cache_key_with_source(drv_path, &ov.source_hash),
        None => ArtifactCache::cache_key(drv_path),
    };
    let crate_name = drv.env.get("crateName")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let start = std::time::Instant::now();

    if cache.is_cached_key(&effective_key) {
        return Ok(BuildResult {
            drv_path: drv_path.into(),
            crate_name,
            success: true,
            duration: start.elapsed(),
            stdout: String::new(),
            stderr: String::new(),
            rmeta_dir: None,
        });
    }

    // Reuse the same tmp/script setup as build_crate_with_worker
    let tmp_dir = cache.root().join("tmp").join(&effective_key);
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)
            .map_err(|e| format!("removing old tmp: {e}"))?;
    }
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| format!("creating tmp dir: {e}"))?;
    let tmp = tmp_dir;
    let env = rewriter.rewrite_env(&drv.env);

    let out_dir = tmp.join("out");
    let lib_dir = tmp.join("lib");

    let mut script = String::new();

    let env_file = tmp.join("env.sh");
    {
        let mut ef = String::new();
        for (k, v) in &env {
            let escaped = escape_for_dollar_single_quote(v);
            ef.push_str(&format!("export {k}=$'{escaped}'\n"));
        }
        std::fs::write(&env_file, &ef)
            .map_err(|e| format!("writing env file: {e}"))?;
    }
    script.push_str(&format!("source '{}'\n", env_file.display()));

    if let Some(ov) = src_override {
        script.push_str(&format!("export src='{}'\n", ov.src_path.display()));
    }

    script.push_str(&format!("export out='{}'\n", out_dir.display()));
    script.push_str(&format!("export lib='{}'\n", lib_dir.display()));

    if drv.is_structured_attrs() {
        if let Some(json_str) = drv.env.get("__json") {
            let json_path = tmp.join(".attrs.json");
            let rewritten_json = rewrite_structured_attrs_json(
                json_str,
                out_dir.to_str().unwrap(),
                lib_dir.to_str().unwrap(),
                rewriter,
            );
            std::fs::write(&json_path, &rewritten_json)
                .map_err(|e| format!("writing attrs json: {e}"))?;
            script.push_str(&format!(
                "export NIX_ATTRS_JSON_FILE='{}'\n",
                json_path.display()
            ));
        }
    }

    let inc_dir = cache.incremental_dir(drv_path);
    std::fs::create_dir_all(&inc_dir)
        .map_err(|e| format!("creating incremental dir: {e}"))?;
    script.push_str(&format!(
        "export EXTRA_RUSTC_FLAGS=\"-C incremental={} ${{EXTRA_RUSTC_FLAGS:-}}\"\n",
        inc_dir.display()
    ));

    // Pipelining: tell build-rust-crate where to emit .rmeta
    let rmeta_dir = cache.root().join("rmeta").join(&effective_key);
    std::fs::create_dir_all(&rmeta_dir)
        .map_err(|e| format!("creating rmeta dir: {e}"))?;
    script.push_str(&format!(
        "export NIX_INC_RMETA_DIR='{}'\n",
        rmeta_dir.display()
    ));

    let work_dir = tmp.join("build");
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| format!("creating work dir: {e}"))?;
    script.push_str(&format!("export NIX_BUILD_TOP='{}'\n", work_dir.display()));
    script.push_str(&format!("export TMPDIR='{}'\n", work_dir.display()));
    script.push_str(&format!("export TEMPDIR='{}'\n", work_dir.display()));
    script.push_str(&format!("export TMP='{}'\n", work_dir.display()));
    script.push_str(&format!("export TEMP='{}'\n", work_dir.display()));
    script.push_str("export HOME='/homeless-shelter'\n");

    let outputs = env.get("outputs")
        .cloned()
        .unwrap_or_else(|| "out".into());
    script.push_str(&format!("export outputs='{outputs}'\n"));
    script.push_str("export dontFixup=1\n");
    script.push_str(&format!("cd '{}'\n", work_dir.display()));

    script.push_str(r#"
for p in $nativeBuildInputs $depsBuildBuild; do
  if [ -d "$p/bin" ]; then
    export PATH="$p/bin:$PATH"
  fi
done
"#);

    script.push_str(r#"
dumpVars() { :; }
showPhaseHeader() { :; }
showPhaseFooter() { :; }

_ms() { read _s _ < /proc/uptime; echo "${_s/./}"; }
_t0=$(_ms)
genericBuild
_t1=$(_ms)
echo "__TIMING__ phases=$((_t1-_t0))0ms" >&2
"#);

    let script_path = tmp.join("builder.sh");
    std::fs::write(&script_path, &script)
        .map_err(|e| format!("writing build script: {e}"))?;

    let result = worker.execute_with_signal(&script_path, &tmp, on_meta_ready)
        .map_err(|e| format!("worker build {crate_name}: {e}"))?;

    let success = result.exit_code == 0;
    if !success {
        let _ = std::fs::remove_dir_all(&tmp);
    } else {
        cache.commit_key(&effective_key)
            .map_err(|e| format!("committing {crate_name} to cache: {e}"))?;
    }

    Ok(BuildResult {
        drv_path: drv_path.into(),
        crate_name,
        success,
        duration: start.elapsed(),
        stdout: result.stdout,
        stderr: result.stderr,
        rmeta_dir: result.rmeta_dir,
    })
}

/// Rewrite output paths and dependency paths in the __structuredAttrs JSON
/// so the build-rust-crate binary sees our cache paths instead of /nix/store.
fn rewrite_structured_attrs_json(
    json_str: &str,
    out_path: &str,
    lib_path: &str,
    rewriter: &PathRewriter,
) -> String {
    // Parse, rewrite string values, re-serialize
    let mut val: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };

    if let serde_json::Value::Object(ref mut map) = val {
        // Replace outputs with a proper {name: path} map.
        // In the __json blob, outputs is just ["out", "lib"] (names only).
        // The build-rust-crate binary expects {"out": "/path", "lib": "/path"}.
        let mut outputs_map = serde_json::Map::new();
        outputs_map.insert("out".into(), serde_json::Value::String(out_path.to_string()));
        outputs_map.insert("lib".into(), serde_json::Value::String(lib_path.to_string()));
        map.insert("outputs".into(), serde_json::Value::Object(outputs_map));

        // Rewrite all string values that contain /nix/store paths
        rewrite_json_values(map, rewriter);
    }

    serde_json::to_string(&val).unwrap_or_else(|_| json_str.to_string())
}

fn rewrite_json_values(map: &mut serde_json::Map<String, serde_json::Value>, rewriter: &PathRewriter) {
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
pub fn make_rewriter(
    _drv: &Derivation,
    dep_cache_map: &BTreeMap<String, PathBuf>,
) -> PathRewriter {
    let mut rw = PathRewriter::new();

    // Rewrite dependency output paths
    for (store_path, cache_path) in dep_cache_map {
        rw.add(store_path.clone(), cache_path.to_str().unwrap().to_string());
    }

    rw
}
