//! Parallel build scheduler with mid-build pipelining and early cutoff.
//!
//! ### Edge classification
//!
//! Each dep→dependent edge is **early-signal** (dependent may start once the
//! dep emits `__META_READY__` on fd 3, e.g. Rust rmeta) or **done**
//! (dependent waits for full commit). Classification is the dep's backend's
//! `PipelinePolicy`, with one extra constraint from early cutoff below.
//!
//! ### Early cutoff
//!
//! Tracked units (see [`overrides::tracked_set`]) use a composite cache key
//! `eff(c) = H(own(c) ‖ propagated(tracked deps))`, where `propagated(dep)`
//! is the hash of dep's *committed output*. That hash is only known once dep
//! is done, so:
//!
//!   - tracked→tracked edges are forced to **done-gated** (workspace crates
//!     lose rmeta pipelining among themselves; registry-crate pipelining is
//!     unaffected since registry crates are never tracked deps),
//!   - a tracked unit's `eff(c)` is computed when it becomes **ready**
//!     (worker pulls it), not upfront,
//!   - if `eff(c)` cache-hits, the worker reads the artifact's `.out-hash`
//!     into `propagated[c]` and completes without building.
//!
//! When a rebuilt dep's output hash equals its previous `.out-hash`,
//! dependents' `eff` keys don't move and they cache-hit — that's the cutoff.
//!
//! Untracked units keep the upfront drv-path-key cache check and the full
//! pipelining behaviour exactly as before.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;

use crate::backend::{Backend, BuildContext};
use crate::cache::ArtifactCache;
use crate::drv::Derivation;
use crate::executor::{self, SourceOverride};
use crate::graph::{BuildGraph, UnitNode};
use crate::overrides::{eff_hash, OwnHash};
use crate::progress::Progress;

pub struct SchedulerResult {
    pub failed: usize,
    /// Resolved cache key per drv (eff-key for tracked, drv-path key for
    /// untracked). Exposed so the cli can place result symlinks / dump keys
    /// after early-cutoff resolution.
    pub keys: HashMap<String, String>,
}

struct SharedState {
    ready: Vec<String>,
    /// Count of deps whose rmeta we're waiting on (pipelineable deps).
    pending_early: HashMap<String, usize>,
    /// Count of deps whose full build we're waiting on (proc-macro/build.rs).
    pending_done: HashMap<String, usize>,
    /// dep → dependents waiting on its rmeta.
    early_dependents: HashMap<String, Vec<String>>,
    /// dep → dependents waiting on its full completion.
    done_dependents: HashMap<String, Vec<String>>,
    /// store-output-path → cache/tmp path. Populated for cached crates up
    /// front, and for in-flight crates as soon as they START (pointing at
    /// tmp/<key>/lib so downstream can read the early rmeta).
    output_map: BTreeMap<String, PathBuf>,
    /// drv_paths whose rmeta has been signalled. The fd-3 signal and the
    /// post-success catch-up both call `fire_early`; this set guarantees
    /// idempotence so dependents are decremented exactly once.
    early_fired: HashSet<String>,
    /// Resolved cache key per tracked unit, filled in at ready-time.
    /// Untracked units' keys are precomputed in `untracked_key` outside the
    /// lock (they never change).
    eff_key: HashMap<String, String>,
    /// Early-cutoff propagated hash per tracked unit: blake3 of its
    /// committed output, set on commit (executor writes `.out-hash`) or read
    /// from a cache-hit's artifact. Dependents read this to compute their
    /// own `eff_key`.
    propagated: HashMap<String, String>,
    succeeded: usize,
    /// Tracked units that resolved to a cache hit at ready-time. Reported
    /// separately from upfront-cached untracked units so the progress
    /// summary's "built" count reflects actual work.
    late_cached: usize,
    failed: usize,
    abort: bool,
    in_flight: usize,
}

impl SharedState {
    fn maybe_ready(&mut self, drv: &str) {
        if self.pending_early.get(drv).copied() == Some(0)
            && self.pending_done.get(drv).copied() == Some(0)
        {
            // Guard against double-push: remove from pending maps.
            self.pending_early.remove(drv);
            self.pending_done.remove(drv);
            self.ready.push(drv.to_string());
        }
    }

    fn fire_early(&mut self, dep: &str) {
        if !self.early_fired.insert(dep.to_string()) {
            return;
        }
        if let Some(ds) = self.early_dependents.remove(dep) {
            for d in ds {
                if let Some(c) = self.pending_early.get_mut(&d) {
                    *c -= 1;
                }
                self.maybe_ready(&d);
            }
        }
    }

    fn fire_done(&mut self, dep: &str) {
        if let Some(ds) = self.done_dependents.remove(dep) {
            for d in ds {
                if let Some(c) = self.pending_done.get_mut(&d) {
                    *c -= 1;
                }
                self.maybe_ready(&d);
            }
        }
    }

    fn all_done(&self) -> bool {
        self.abort || (self.ready.is_empty() && self.in_flight == 0)
    }
}

/// Per-node backend dispatch. Each unit was admitted to the graph by exactly
/// one backend's `is_unit` (the cli unions them); rediscover which one here so
/// `unit_name` / `build_script_hooks` / `output_populated` / `pipeline` come
/// from the right place. Precomputed once — `is_unit` is cheap but called
/// per-edge for `pipelineable` below.
fn backend_for<'a>(
    backends: &'a [&'a dyn Backend],
    drv_path: &str,
    drv: &Derivation,
    repo_root: &Path,
) -> &'a dyn Backend {
    backends
        .iter()
        .copied()
        .find(|b| b.is_unit(drv_path, drv, repo_root))
        // from_roots() only admits units some backend claimed, so this is
        // unreachable for graph nodes. Fall back to the first backend rather
        // than panic so a future caller passing a non-unit drv degrades.
        .unwrap_or(backends[0])
}

#[allow(clippy::too_many_arguments)]
pub fn run_parallel(
    graph: &BuildGraph,
    cache: &ArtifactCache,
    jobs: usize,
    backends: &[&dyn Backend],
    repo_root: &Path,
    own: &HashMap<String, OwnHash>,
    tracked: &HashSet<String>,
    roots: &[String],
) -> SchedulerResult {
    let roots: HashSet<&str> = roots.iter().map(String::as_str).collect();
    let start = std::time::Instant::now();
    let self_exe = std::env::current_exe().expect("resolving self exe");

    // drv_path → owning backend. See `backend_for`.
    let backend_of: HashMap<&str, &dyn Backend> = graph
        .nodes
        .iter()
        .map(|(k, n)| (k.as_str(), backend_for(backends, k, &n.drv, repo_root)))
        .collect();

    // Worker pool config from any unit's drv — they all share stdenv/builder.
    // Mixed-backend graphs share stdenv too (it's nixpkgs', not the
    // language's), so any node will do.
    let Some(first_drv) = graph.nodes.values().next() else {
        // from_roots() rejects missing/non-unit roots, so this only triggers
        // when called with no roots at all.
        Progress::new(0, 0).summary(0, 0, 0, start.elapsed());
        return SchedulerResult {
            failed: 0,
            keys: HashMap::new(),
        };
    };
    let bash = first_drv.drv.builder.clone();
    let stdenv_path = first_drv
        .drv
        .env
        .get("stdenv")
        .expect("drv missing stdenv")
        .clone();

    // Untracked units: plain drv-path key, checkable now. Tracked units'
    // keys are deferred to ready-time (see module doc).
    let untracked_key = |drv: &str| ArtifactCache::cache_key(drv);

    let cached_artifact_ok = |drv: &str, node: &UnitNode, key: &str| -> bool {
        if !cache.is_cached_key(key) {
            return false;
        }
        if roots.contains(drv) {
            return backend_of[drv].pipeline().is_none_or(|p| {
                p.cached_artifact_sufficient_as_root(&node.drv, &cache.artifact_dir_by_key(key))
            });
        }
        true
    };

    // Edge classification is decided by the *dep's* backend's policy. On top
    // of that, tracked→tracked edges are forced done-gated so the dependent
    // can read `propagated[dep]` (the committed-output hash) when it becomes
    // ready. Untracked deps don't contribute to the dependent's eff-key, so
    // they keep early-gating where the policy allows it.
    let pipelineable: HashMap<String, bool> = graph
        .nodes
        .iter()
        .map(|(k, n)| {
            let p = backend_of[k.as_str()]
                .pipeline()
                .is_some_and(|p| p.is_pipelineable(&n.drv));
            (k.clone(), p)
        })
        .collect();
    let early_ok = |dep: &str, dependent: &str| {
        *pipelineable.get(dep).unwrap_or(&false)
            && !(tracked.contains(dep) && tracked.contains(dependent))
    };

    let mut pending_early: HashMap<String, usize> = HashMap::new();
    let mut pending_done: HashMap<String, usize> = HashMap::new();
    let mut early_dependents: HashMap<String, Vec<String>> = HashMap::new();
    let mut done_dependents: HashMap<String, Vec<String>> = HashMap::new();
    let mut output_map: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut ready: Vec<String> = Vec::new();
    let mut cached = 0;
    let mut to_schedule = 0;

    for (drv_path, node) in &graph.nodes {
        let is_tracked = tracked.contains(drv_path);

        // Untracked units can be cache-checked now; tracked units always
        // enter the schedule (their cache check happens at ready-time and
        // may resolve to a hit there).
        if !is_tracked && cached_artifact_ok(drv_path, node, &untracked_key(drv_path)) {
            let artifact = cache.artifact_dir_by_key(&untracked_key(drv_path));
            for (name, out) in &node.drv.outputs {
                output_map.insert(out.path.clone(), artifact.join(name));
            }
            cached += 1;
            continue;
        }
        to_schedule += 1;

        // Pending deps. For an untracked dependent, "uncached dep" means an
        // untracked dep that wasn't filtered above OR any tracked dep
        // (whose cache status is unknown until ready-time). For a tracked
        // dependent, same — but tracked deps are additionally forced to
        // done-gated regardless of pipelineability.
        let mut n_early = 0usize;
        let mut n_done = 0usize;
        for dep in &node.unit_deps {
            let dep_tracked = tracked.contains(dep);
            let dep_pending = dep_tracked
                || !cached_artifact_ok(dep, &graph.nodes[dep.as_str()], &untracked_key(dep));
            if !dep_pending {
                continue;
            }
            if early_ok(dep, drv_path) {
                n_early += 1;
                early_dependents
                    .entry(dep.clone())
                    .or_default()
                    .push(drv_path.clone());
            } else {
                n_done += 1;
                done_dependents
                    .entry(dep.clone())
                    .or_default()
                    .push(drv_path.clone());
            }
        }
        pending_early.insert(drv_path.clone(), n_early);
        pending_done.insert(drv_path.clone(), n_done);

        if n_early == 0 && n_done == 0 {
            ready.push(drv_path.clone());
        }
    }
    let progress = Arc::new(Progress::new(to_schedule, cached));

    if to_schedule == 0 {
        progress.summary(0, cached, 0, start.elapsed());
        return SchedulerResult {
            failed: 0,
            keys: graph
                .nodes
                .keys()
                .map(|k| (k.clone(), untracked_key(k)))
                .collect(),
        };
    }

    let state = Arc::new((
        Mutex::new(SharedState {
            ready,
            pending_early,
            pending_done,
            early_dependents,
            done_dependents,
            output_map,
            early_fired: HashSet::new(),
            eff_key: HashMap::new(),
            propagated: HashMap::new(),
            succeeded: 0,
            late_cached: 0,
            failed: 0,
            abort: false,
            in_flight: 0,
        }),
        Condvar::new(),
    ));

    thread::scope(|s| {
        for _ in 0..jobs {
            let state = Arc::clone(&state);
            let progress = Arc::clone(&progress);
            let backend_of = &backend_of;
            let roots = &roots;
            let bash = &bash;
            let stdenv_path = &stdenv_path;
            let self_exe = &self_exe;
            let cached_artifact_ok = &cached_artifact_ok;
            s.spawn(move || {
                let mut worker =
                    crate::worker::Worker::spawn(bash, stdenv_path).expect("spawning worker");
                worker_loop(
                    &state,
                    &graph.nodes,
                    cache,
                    backend_of,
                    self_exe,
                    &mut worker,
                    &progress,
                    own,
                    tracked,
                    cached_artifact_ok,
                    roots,
                );
            });
        }
    });

    let s = state.0.lock().unwrap();

    progress.summary(
        s.succeeded,
        cached + s.late_cached,
        s.failed,
        start.elapsed(),
    );

    let mut keys: HashMap<String, String> = s.eff_key.clone();
    for k in graph.nodes.keys() {
        keys.entry(k.clone()).or_insert_with(|| untracked_key(k));
    }
    SchedulerResult {
        failed: s.failed,
        keys,
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    state: &(Mutex<SharedState>, Condvar),
    nodes: &BTreeMap<String, UnitNode>,
    cache: &ArtifactCache,
    backend_of: &HashMap<&str, &dyn Backend>,
    self_exe: &std::path::Path,
    worker: &mut crate::worker::Worker,
    progress: &Progress,
    own: &HashMap<String, OwnHash>,
    tracked: &HashSet<String>,
    cached_artifact_ok: &dyn Fn(&str, &UnitNode, &str) -> bool,
    roots: &HashSet<&str>,
) {
    let (lock, cvar) = state;

    loop {
        let (drv_path, eff_key, dep_map, src_path) = {
            let mut s = lock.lock().unwrap();

            // Pull the next ready unit. Tracked cache-hits resolve here
            // without leaving the lock (no build, just propagate + fire);
            // loop until we find one that needs building or the queue
            // drains.
            let (drv_path, eff_key) = loop {
                while s.ready.is_empty() && !s.all_done() {
                    s = cvar.wait(s).unwrap();
                }
                if s.all_done() {
                    return;
                }
                let drv_path = s.ready.pop().unwrap();
                let node = &nodes[&drv_path];

                let eff_key = if tracked.contains(&drv_path) {
                    // All tracked deps are done (forced done-gated), so
                    // `propagated[dep]` is populated. Untracked deps yield
                    // None and don't contribute. A tracked dep missing from
                    // `propagated` (pre-cutoff artifact without `.out-hash`)
                    // falls back to its eff-key, which is in `eff_key` by
                    // now since it completed before us.
                    let k = ArtifactCache::cache_key_with_source(
                        &drv_path,
                        &eff_hash(
                            own.get(&drv_path),
                            node.unit_deps.iter().map(String::as_str),
                            |d| {
                                if !tracked.contains(d) {
                                    return None;
                                }
                                s.propagated
                                    .get(d)
                                    .or_else(|| s.eff_key.get(d))
                                    .map(String::as_str)
                            },
                        ),
                    );
                    s.eff_key.insert(drv_path.clone(), k.clone());
                    k
                } else {
                    ArtifactCache::cache_key(&drv_path)
                };

                // Late cache check (tracked units, and untracked units that
                // weren't filterable upfront because a tracked dep was
                // pending — though for untracked the key is drv-path-only
                // so this is the same check as upfront would have been).
                if cached_artifact_ok(&drv_path, node, &eff_key) {
                    let artifact = cache.artifact_dir_by_key(&eff_key);
                    for (name, out) in &node.drv.outputs {
                        s.output_map.insert(out.path.clone(), artifact.join(name));
                    }
                    // Propagated hash for dependents: the `.out-hash`
                    // sidecar written when this artifact was committed.
                    // Missing (pre-cutoff cache) → leave unset; dependents
                    // fall back to our eff-key (input-cascade behaviour for
                    // this edge, fixed on the next rebuild).
                    if let Ok(h) = std::fs::read_to_string(cache.out_hash_path(&eff_key)) {
                        s.propagated.insert(drv_path.clone(), h);
                    }
                    s.late_cached += 1;
                    progress.late_cached();
                    s.fire_early(&drv_path);
                    s.fire_done(&drv_path);
                    cvar.notify_all();
                    continue;
                }

                break (drv_path, eff_key);
            };

            s.in_flight += 1;
            let node = &nodes[&drv_path];
            let my_tmp = cache.root().join("tmp").join(&eff_key);

            // Reset tmp/<key> under the lock BEFORE publishing it via
            // output_map. A previous run leaves tmp/<key> populated (commit is
            // a hardlink copy, not a move); if a dependent saw that path and
            // raced us to the executor's later cleanup it could read stale
            // bytes. remove_file handles a leftover symlink, remove_dir_all
            // a real dir.
            let _ = std::fs::remove_file(&my_tmp);
            let _ = std::fs::remove_dir_all(&my_tmp);
            let _ = std::fs::create_dir_all(&my_tmp);

            // Register OUR outputs in output_map NOW, pointing at tmp/<key>/…,
            // so dependents that start (via rmeta) before we commit can find
            // our early rmeta and (later) rlib.
            for (name, out) in &node.drv.outputs {
                s.output_map.insert(out.path.clone(), my_tmp.join(name));
            }

            // Build dep_map (store-path → cache-or-tmp path) for direct deps.
            let mut dep_map: BTreeMap<String, PathBuf> = BTreeMap::new();
            for dep_drv in &node.unit_deps {
                let Some(dep_node) = nodes.get(dep_drv) else {
                    continue;
                };
                for out in dep_node.drv.outputs.values() {
                    if let Some(cache_path) = s.output_map.get(&out.path) {
                        dep_map.insert(out.path.clone(), cache_path.clone());
                    }
                }
            }

            let src_path = own.get(&drv_path).map(|o| o.src_dir.clone());
            (drv_path, eff_key, dep_map, src_path)
        };

        let node = &nodes[&drv_path];
        let backend = backend_of[drv_path.as_str()];
        let unit_name = backend.unit_name(&node.drv).into_owned();
        progress.start(&unit_name);

        let rewriter = executor::make_rewriter(&node.drv, &dep_map);
        let tmp = cache.root().join("tmp").join(&eff_key);
        let src_ov = SourceOverride {
            src_path,
            eff_key: eff_key.clone(),
        };
        let result = executor::build_unit(
            BuildContext {
                drv_path: &drv_path,
                drv: &node.drv,
                tmp: &tmp,
                cache,
                is_root: roots.contains(drv_path.as_str()),
                self_exe,
            },
            backend,
            &rewriter,
            worker,
            Some(&src_ov),
            |_early_dir| {
                let mut s = lock.lock().unwrap();
                s.fire_early(&drv_path);
                cvar.notify_all();
            },
        );

        {
            let mut s = lock.lock().unwrap();
            s.in_flight -= 1;

            match result {
                Ok(ref r) if r.success => {
                    s.succeeded += 1;
                    progress.finish(&unit_name, r.duration);

                    // Executor wrote `.out-hash` on commit; read it back so
                    // tracked dependents (done-gated on us) key on it.
                    if tracked.contains(&drv_path) {
                        if let Ok(h) = std::fs::read_to_string(cache.out_hash_path(&eff_key)) {
                            s.propagated.insert(drv_path.clone(), h);
                        }
                    }

                    // Catch-up for crates that never signalled rmeta.
                    // `early_fired` makes this idempotent if fd-3 already fired.
                    s.fire_early(&drv_path);
                    s.fire_done(&drv_path);
                }
                Ok(ref r) => {
                    s.failed += 1;
                    s.abort = true;
                    progress.fail(&unit_name, &r.stdout, &r.stderr);
                }
                Err(e) => {
                    s.failed += 1;
                    s.abort = true;
                    progress.fail(&unit_name, "", &e);
                }
            }

            cvar.notify_all();
        }
    }
}
