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
/// change to `foo/src/lib.rs` produces a new key for foo and
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

    if let Some(ov) = src_override {
        if let Some(ref p) = ov.src_path {
            script.push_str(&format!("export src='{}'\n", p.display()));
        }
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
    pl: &PipelineConfig,
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

    // No is_cached short-circuit here: the scheduler already filtered cached
    // crates, and a root cdylib that was previously committed rlib-only must
    // be rebuilt even though is_cached_key() returns true. Clear any stale
    // artifact so the hardlink commit at the end gets a clean destination.
    let dest = cache.artifact_dir_by_key(&effective_key);
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&dest);
    }

    // tmp/<key> is reset by the scheduler under lock (before publishing it via
    // output_map) so dependents can't see stale bytes. Ensure lib/lib/ exists
    // too: dependents' configurePhase reaches our rmeta via
    // `$i/../rmeta/*.rmeta` where `$i = tmp/<key>/lib`, and bash glob can't
    // resolve `lib/..` if `lib/` itself doesn't exist yet.
    let tmp = cache.root().join("tmp").join(&effective_key);
    std::fs::create_dir_all(tmp.join("lib").join("lib"))
        .map_err(|e| format!("creating tmp dir: {e}"))?;
    let mut env = rewriter.rewrite_env(&drv.env);

    // Swap in-flight deps' .rlib paths for their early-written .rmeta. Applied
    // AFTER store-prefix rewrites so they match the already-rewritten cache/tmp
    // paths. Also patch configurePhase's `symlink_dependency` glob so
    // `-L dependency=target/deps` resolves transitive in-flight crates via
    // their early rmeta. The rmeta lives at `<dep>/../rmeta/` (not `<dep>/lib/`)
    // so installPhase's `cp target/lib/* $lib/lib` can't truncate it underneath
    // a reader.
    let apply_rmeta_rewrites = |s: &mut String| {
        for (from, to) in &pl.rmeta_rewrites {
            if s.contains(from) {
                *s = s.replace(from, to);
            }
        }
    };
    for v in env.values_mut() {
        apply_rmeta_rewrites(v);
    }
    if let Some(cp) = env.get_mut("configurePhase") {
        *cp = cp.replace(
            "ln -s -f $i/lib/*.rlib $2",
            "ln -s -f $i/lib/*.rlib $i/lib/*.rmeta $i/../rmeta/*.rmeta $2 2>/dev/null",
        );
    }

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

    // Override src with local worktree dir when reusing a cached drv across
    // source changes. unpackPhase will copy it into NIX_BUILD_TOP; the live
    // dir only ever contains a tiny target/.nix-inc-mtime-cache (cargo's real
    // target dir is workspace-level), so no filtered snapshot is needed.
    if let Some(ov) = src_override {
        if let Some(ref p) = ov.src_path {
            script.push_str(&format!("export src='{}'\n", p.display()));
        }
    }

    script.push_str(&format!("export out='{}'\n", out_dir.display()));
    script.push_str(&format!("export lib='{}'\n", lib_dir.display()));

    if drv.is_structured_attrs() {
        if let Some(json_str) = drv.env.get("__json") {
            let json_path = tmp.join(".attrs.json");
            let mut rewritten_json = rewrite_structured_attrs_json(
                json_str,
                out_dir.to_str().unwrap(),
                lib_dir.to_str().unwrap(),
                rewriter,
            );
            apply_rmeta_rewrites(&mut rewritten_json);
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
    // The wrapper is the nix-inc binary itself (`__rustc-wrap`, see
    // rustc_wrap.rs), so arg classification, JSON-stderr parsing, and
    // rmeta→rlib polling happen in Rust without forking jq/cp/mv per line.
    // This /bin/sh shim just bridges PATH lookup of `rustc` to the subcommand.
    {
        let wrapper_dir = tmp.join("rustc-wrap");
        std::fs::create_dir_all(&wrapper_dir)
            .map_err(|e| format!("creating wrapper dir: {e}"))?;
        let wrapper = wrapper_dir.join("rustc");
        let self_exe = std::env::current_exe()
            .map_err(|e| format!("resolving self exe: {e}"))?;
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

        // Config for rustc_wrap::main(). NIXINC_REAL_RUSTC is resolved AFTER
        // `source $stdenv/setup` so PATH already has the toolchain.
        // completeDeps/completeBuildDeps come from env.sh (already rewritten).
        // Intentionally NOT `NIX_INC_RMETA_DIR`: build-rust-crate keys its
        // post-build `--emit=metadata` pass + fd-3 signal off that var. The
        // wrapper already signalled mid-build, so that second rustc call would
        // be ~50ms of redundant work per crate.
        script.push_str(&format!(
            concat!(
                "export NIXINC_REAL_RUSTC=$(command -v rustc)\n",
                "export NIXINC_RMETA_DIR='{rmeta_dir}'\n",
                "export NIXINC_EXPECTED_RMETA='{expected}'\n",
                "export NIXINC_SKIP_LINK_PASS='{skip_link}'\n",
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
    std::fs::write(&script_path, &script)
        .map_err(|e| format!("writing build script: {e}"))?;

    let result = worker.execute_with_signal(&script_path, &tmp, on_meta_ready)
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
        let _ = std::fs::remove_dir_all(&tmp);
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
        for sub in ["build", "rustc-wrap"] {
            let _ = std::fs::remove_dir_all(tmp.join(sub));
        }
        for f in ["builder.sh", "env.sh", "worker-stdout", "worker-stderr"] {
            let _ = std::fs::remove_file(tmp.join(f));
        }
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
