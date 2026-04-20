//! Language-backend abstraction.
//!
//! bob's core (drv parser, graph, cache, path rewriter, worker pool,
//! scheduler, `.attrs.{json,sh}` emission, `genericBuild` replay) is
//! language-agnostic. A `Backend` supplies the per-language policy:
//!
//! - which drvs in the closure are "units" we replay (vs boundary inputs we
//!   `nix-store --realise`),
//! - how to map a user-supplied target name to a `bob.nix` attr path,
//! - which workspace units to track for source changes,
//! - what to inject into `builder.sh` after `source $stdenv/setup`
//!   (incremental-cache env vars, compiler wrappers, …),
//! - whether/how mid-build pipelining applies.
//!
//! The fd-3 `__META_READY__` signal itself is generic: any backend's wrapper
//! may emit it, and the scheduler will unblock dependents whose edge to the
//! emitter is classified as pipelineable. Backends without an early-artifact
//! analogue (Go) simply return `pipeline() == None` and every edge is
//! done-gated.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use crate::cache::ArtifactCache;
use crate::drv::Derivation;
use crate::graph::BuildGraph;
use crate::overrides::OwnHash;

/// Per-unit context handed to `Backend::build_script_hooks`. Everything the
/// backend needs to compute its injection (wrapper shims, incremental dirs,
/// pipelining config) without the core knowing what any of it means.
///
/// `Copy` so the executor can pass it through by value without the
/// borrow-juggling that `..ctx` struct-update on a moved value invites.
#[derive(Clone, Copy)]
pub struct BuildContext<'a> {
    pub drv_path: &'a str,
    pub drv: &'a Derivation,
    /// `~/.cache/bob/tmp/<key>/` — in-progress build root.
    pub tmp: &'a Path,
    pub cache: &'a ArtifactCache,
    /// True iff this unit was named on the command line (vs a transitive dep).
    /// The Rust backend uses this for `skip_link_pass`: only roots need the
    /// cdylib `.so`; transitive deps' rlib is all anyone reads.
    pub is_root: bool,
    /// Path to the running `bob` binary, for wrapper-shim shebangs that
    /// re-enter via `bob __<backend>-wrap …`.
    pub self_exe: &'a Path,
}

pub trait Backend: Send + Sync {
    /// Backend identifier. Mixed into the graph-cache key (the unit/boundary
    /// partition depends on which backends are registered) and intended to
    /// tag `UnitNode`s once multiple backends coexist in one graph.
    fn id(&self) -> &'static str;

    // ── graph ──────────────────────────────────────────────────────────────

    /// Is this drv a unit we replay? Everything else becomes a boundary input.
    ///
    /// `drv_path` and `repo_root` are provided for backends whose unit set is
    /// declared out-of-band (e.g. cc's drvPath→src map in `bob.nix`) rather
    /// than via a marker in the drv env. Backends that key purely on
    /// `drv.env` ignore both.
    fn is_unit(&self, drv_path: &str, drv: &Derivation, repo_root: &Path) -> bool;

    /// Human-readable name for progress output and error messages.
    fn unit_name<'a>(&self, drv: &'a Derivation) -> Cow<'a, str>;

    // ── resolve ────────────────────────────────────────────────────────────

    /// Attr path under `(import bob.nix {})` for `target`, or `None` if this
    /// backend doesn't recognise it. The cli tries each registered backend.
    fn resolve_attr(&self, target: &str, repo_root: &Path) -> Option<String>;

    /// Hash of the file that gates eval-cache validity (lockfile / sum file).
    fn lock_hash(&self, repo_root: &Path) -> Result<String, String>;

    /// Detect a target name from cwd by looking for the backend's manifest.
    fn detect_from_cwd(&self) -> Option<String>;

    /// Known target names under `repo_root`. Used by the cli to suggest
    /// candidates when no backend's `resolve_attr` matches.
    fn list_targets(&self, _repo_root: &Path) -> Vec<String> {
        Vec::new()
    }

    // ── source-change tracking ─────────────────────────────────────────────

    /// `drv_path → (own_source_hash, live_src_dir)` for every workspace unit
    /// the backend can locate in the graph. Core then cascades these through
    /// the DAG into `SourceOverride`s (see `overrides::cascade`).
    fn workspace_unit_hashes(
        &self,
        repo_root: &Path,
        graph: &BuildGraph,
    ) -> HashMap<String, OwnHash>;

    // ── build ──────────────────────────────────────────────────────────────

    /// Shell fragment injected into `builder.sh` after `source $stdenv/setup`
    /// and before `genericBuild`. May write files under `ctx.tmp` (wrapper
    /// shims, config) and must return only the lines to append.
    fn build_script_hooks(&self, ctx: &BuildContext<'_>) -> Result<String, String>;

    /// Belt-and-braces success check: did installPhase produce a usable
    /// artifact? `genericBuild`'s exit code is unreliable across stdenv
    /// versions (errexit vs `eval`'d phases).
    fn output_populated(&self, tmp: &Path, drv: &Derivation) -> bool;

    // ── pipelining (optional) ──────────────────────────────────────────────

    /// `None` → every edge is done-gated; the backend never emits a mid-build
    /// signal.
    fn pipeline(&self) -> Option<&dyn PipelinePolicy> {
        None
    }

    // ── internal subcommands ───────────────────────────────────────────────

    /// Handle `bob __<x> …` re-entries from wrapper shims. If `cmd` belongs
    /// to this backend, run it and `process::exit`; otherwise return so the
    /// cli can try the next backend. (No useful return value: a claimed
    /// command never returns.)
    fn dispatch_internal(&self, _cmd: &str, _args: &[String]) {}
}

/// Per-backend pipelining policy. The scheduler classifies each dep→dependent
/// edge as either "early-signal" (dependent may start once dep emits
/// `__META_READY__`) or "done" (dependent waits for full commit). The
/// classification is `pipeline().is_pipelineable(dep)`.
pub trait PipelinePolicy: Send + Sync {
    /// Can dependents of THIS unit start on its mid-build signal?
    fn is_pipelineable(&self, drv: &Derivation) -> bool;

    /// Is the cached artifact at `dir` sufficient when this unit is a ROOT
    /// target? The Rust backend's `skip_link_pass` means a `lib cdylib`
    /// crate may have been committed rlib-only when it was a transitive dep;
    /// as a root, the `.so` IS the product, so that cache entry must be
    /// treated as a miss.
    fn cached_artifact_sufficient_as_root(&self, _drv: &Derivation, _dir: &Path) -> bool {
        true
    }
}
