//! C/C++ language backend: stdenv `mkDerivation` units built with cmake or
//! meson (out-of-tree), replayed with a persistent build directory so ninja's
//! own `.d`-file dep tracking gives per-TU incrementality.
//!
//! ## Unit model
//!
//! Project-grain: one drv = one cmake/meson project. Units are declared in
//! `bob.nix` under `cc.<name>` via `lib/cc.nix`, which attaches `bobCcSrc`
//! as a *Nix-level* attribute (`drv // { … }`) so `drvPath` is unchanged.
//! That's load-bearing: the same drv path appears in any Rust crate's
//! `buildInputs` closure, so `bob build <rust-root>` finds the cc unit in
//! its graph and a C edit cascades through to the `.so` without overlays.
//! [`workspace::cc_units`] evaluates the `cc` attrset once to get the
//! drvPath→src map; nothing is read from the drv env.
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

impl Backend for CcBackend {
    fn id(&self) -> &'static str {
        "cc"
    }

    fn is_unit(&self, drv_path: &str, _drv: &Derivation, repo_root: &Path) -> bool {
        workspace::cc_units(repo_root).contains_key(drv_path)
    }

    fn unit_name<'a>(&self, drv: &'a Derivation) -> Cow<'a, str> {
        drv.env
            .get("pname")
            .or_else(|| drv.env.get("name"))
            .map(String::as_str)
            .unwrap_or("?")
            .into()
    }

    fn resolve_attr(&self, target: &str, repo_root: &Path) -> Option<String> {
        // The bob.nix `cc.<name>` attr is the contract. The map is already
        // loaded (or loads now, cached), so gate on declared names — unlike
        // the earlier optimistic claim, this is exact, so a typo falls
        // through to the cli's "unknown target" suggestion path instead of
        // a nix-instantiate error.
        workspace::cc_target_names(repo_root)
            .contains_key(target)
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
        workspace::cc_target_names(repo_root)
            .keys()
            .cloned()
            .collect()
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
