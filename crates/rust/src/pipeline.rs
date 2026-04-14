//! Rust pipelining policy: which crates can dependents start on early
//! (rmeta), and what artifact filenames to expect.

use std::path::Path;

use bob_core::{Derivation, PipelinePolicy};

pub struct RustPipeline;

impl PipelinePolicy for RustPipeline {
    fn is_pipelineable(&self, drv: &Derivation) -> bool {
        is_pipelineable(drv)
    }

    fn cached_artifact_sufficient_as_root(&self, drv: &Derivation, dir: &Path) -> bool {
        !needs_link_output(drv) || artifact_has_link_output(dir)
    }
}

/// A dep is "pipelineable" if dependents can start once its rmeta is ready,
/// without waiting for the full build. That requires:
///   - crateType is `lib` (proc-macro emits a .so that must be loaded;
///     cdylib/bin link the world)
///   - no `links` key (→ `crateLinks` env): such crates' build.rs writes
///     `lib/link` and `lib/env` (DEP_<links>_* vars) that downstream's
///     configurePhase reads BEFORE rustc starts. build.rs without `links`
///     (proc-macro2, anyhow, ...) only sets cfg flags for the crate ITSELF,
///     so dependents don't need to wait.
///
/// build.rs presence itself can't be detected statically — buildRustCrate
/// auto-detects it after unpack — so `crateLinks` is the only reliable signal
/// for "dependents need my full output".
pub fn is_pipelineable(drv: &Derivation) -> bool {
    let ct = drv.env.get("crateType").map(String::as_str).unwrap_or("");
    let has_links = drv
        .env
        .get("crateLinks")
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    // crateType is space-separated; "lib cdylib" / "rlib" both produce a
    // usable rmeta (the cdylib half needs upstream rlibs, but that's handled
    // by the wrapper's poll-for-done on the IN side). proc-macro never does.
    let emits_lib = ct.split_whitespace().any(|t| t == "lib" || t == "rlib");
    emits_lib && !has_links
}

/// Predict the rlib/rmeta filename a crate will produce:
/// `lib{libName}-{metadata}.{ext}`. buildRustCrate normalizes libName so the
/// rlib uses underscores; the metadata hash is fixed at eval time.
///
/// Note: cargo-nix-plugin's `detect_cargo_toml_info` may rewrite `libName` at
/// build time from `[lib].name` (e.g. new_debug_unreachable→debug_unreachable).
/// We use the eval-time `libName` here, so a mismatch means the wrapper never
/// signals `__META_READY__` for that crate — dependents fall back to
/// done-gating. Degrades to no-pipelining for that crate, not a failure.
pub fn lib_filename(drv: &Derivation, ext: &str) -> Option<String> {
    let lib_name = drv.env.get("libName")?.replace('-', "_");
    let metadata = drv.env.get("metadata")?;
    Some(format!("lib{lib_name}-{metadata}.{ext}"))
}

/// A `lib cdylib` crate's cached artifact may have been committed without the
/// `.so` (link pass skipped when it was a transitive dep). When it's the root
/// target, the `.so` IS the product, so an rlib-only artifact is insufficient.
pub fn needs_link_output(drv: &Derivation) -> bool {
    drv.env
        .get("crateType")
        .map(|ct| {
            ct.split_whitespace()
                .any(|t| matches!(t, "cdylib" | "staticlib"))
        })
        .unwrap_or(false)
}

pub fn artifact_has_link_output(dir: &Path) -> bool {
    std::fs::read_dir(dir.join("lib").join("lib"))
        .map(|d| {
            d.flatten().any(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.ends_with(".so") || n.ends_with(".a") || n.ends_with(".dylib")
            })
        })
        .unwrap_or(false)
}
