//! C/C++ language backend: stdenv `mkDerivation` units built with cmake or
//! meson (out-of-tree), replayed with a persistent build directory so ninja's
//! own `.d`-file dep tracking gives per-TU incrementality.
//!
//! ## Unit model
//!
//! Project-grain: one drv = one cmake/meson project. A drv opts in by carrying
//! `bobCcSrc = "<path relative to repo root>"` in its env (see `lib/cc.nix`).
//! That single attr is the unit marker, the display name's source-of-truth
//! lookup, *and* the live source dir for change detection — no separate
//! manifest.
//!
//! ## Incrementality
//!
//! `cache.incremental_dir(drv_path)` is repurposed as the persistent
//! out-of-tree build dir (cmake's `-B`, meson's builddir). It is keyed on
//! `drv_path`, not the effective key, so source edits land in the same dir and
//! ninja rebuilds only changed TUs. Any non-source input change (compiler,
//! flags, buildInputs) yields a new `drv_path` → fresh dir → full reconfigure,
//! which is the correct invalidation boundary.
//!
//! Unpack/patch are skipped: the build is pointed directly at the live
//! worktree (`$src`, already overridden by core to `OwnHash::src_dir`), so
//! cmake/meson record a stable absolute `SOURCE_DIR` and ninja's recorded
//! header paths stay valid across runs. This means **patched derivations are
//! not supported** — the canonical bob-cc unit is first-party source you're
//! editing in place.
//!
//! ## Pipelining (not yet)
//!
//! `pipeline()` returns `None`: every cc edge is done-gated. Unlike Rust's
//! rmeta, a cc unit has no cheap interface artifact that downstream
//! `find_package`/setup-hooks can consume before `installPhase` populates
//! `$out`/`$dev`. A correct early-signal needs (a) header staging into
//! `$dev/include` before `buildPhase`, (b) a `__cc-wrap` link-gate that polls
//! in-flight cc deps' `done` (mirroring `rustc_wrap`), and (c) an edge-aware
//! `PipelinePolicy` so cc→Rust edges stay done-gated (rustc-wrap doesn't know
//! to wait on non-`completeDeps` inputs). All three are mechanical once
//! there's a real cc→cc graph to test against; the per-TU incrementality here
//! is the order-of-magnitude win on its own.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use bob_core::{Backend, BuildContext, BuildGraph, Derivation, OwnHash};

mod hooks;
mod workspace;

pub struct CcBackend;

/// Env-var marker set by `lib/cc.nix`'s `unit`/`units`. Value is the source
/// dir relative to repo root.
pub(crate) const MARK: &str = "bobCcSrc";

impl Backend for CcBackend {
    fn id(&self) -> &'static str {
        "cc"
    }

    fn is_unit(&self, drv: &Derivation) -> bool {
        drv.env.contains_key(MARK)
    }

    fn unit_name<'a>(&self, drv: &'a Derivation) -> Cow<'a, str> {
        drv.env
            .get("pname")
            .or_else(|| drv.env.get("name"))
            .map(String::as_str)
            .unwrap_or("?")
            .into()
    }

    fn resolve_attr(&self, target: &str, _repo_root: &Path) -> Option<String> {
        // The bob.nix `cc.<name>` attr is the contract; CMakeLists
        // `project()` names often differ (e.g. attr `ndl` vs project
        // `neuron_kmdlib`), so don't gate on the discovered-project index.
        // This backend is tried last, after Rust's definitive Cargo.toml
        // lookup has declined, so claiming optimistically just turns a typo
        // into nix-instantiate's "attribute 'cc.<name>' missing" — clear
        // enough, and `list_targets` still offers project-name suggestions.
        // Reject obvious non-idents so paths/flags don't become attr paths.
        target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            .then(|| format!("cc.{target}"))
    }

    fn lock_hash(&self, _repo_root: &Path) -> Result<String, String> {
        // No lockfile analogue. The drv graph for a cc target is fully
        // determined by `bob.nix` and whatever it imports — both already
        // mixed into the eval-cache key by core's `resolve::eval_key` (via
        // `bob.nix` itself + `bob.toml` `eval-inputs`). Returning a fixed
        // string here means cc contributes nothing extra, which is correct.
        Ok(String::new())
    }

    fn detect_from_cwd(&self) -> Option<String> {
        workspace::detect_from_cwd()
    }

    fn list_targets(&self, repo_root: &Path) -> Vec<String> {
        workspace::cc_targets(repo_root).keys().cloned().collect()
    }

    fn workspace_unit_hashes(
        &self,
        repo_root: &Path,
        graph: &BuildGraph,
    ) -> HashMap<String, OwnHash> {
        workspace::unit_hashes(repo_root, graph)
    }

    fn build_script_hooks(&self, ctx: &BuildContext<'_>) -> Result<String, String> {
        hooks::build_script_hooks(ctx)
    }

    fn output_populated(&self, tmp: &Path, drv: &Derivation) -> bool {
        hooks::output_populated(tmp, drv)
    }

    // pipeline() defaults to None — see module doc.
}
