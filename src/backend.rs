//! Language-backend abstraction.
//!
// Wired up in the follow-up "route everything through BACKEND" commit.
#![allow(dead_code)]
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
    fn id(&self) -> &'static str;

    // ── graph ──────────────────────────────────────────────────────────────

    /// Is this drv a unit we replay? Everything else becomes a boundary input.
    fn is_unit(&self, drv: &Derivation) -> bool;

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

    /// Handle `bob __<x> …` re-entries from wrapper shims. Return `true` only
    /// if `cmd` is ours — the impl must `process::exit` itself in that case
    /// (the cli `unreachable!()`s on `true`). Return `false` to pass.
    fn dispatch_internal(&self, _cmd: &str, _args: &[String]) -> bool {
        false
    }
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
