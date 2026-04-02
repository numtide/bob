//! Parallel build scheduler with rmeta pipelining.
//!
//! Each crate build runs as a single worker task. Mid-build, when
//! `build-rust-crate` finishes the lib target's .rmeta, it signals
//! `__META_READY__` via fd 3. The scheduler immediately unlocks
//! dependents that only need type metadata (non-proc-macro deps).
//!
//! Proc-macro and build-dep consumers wait for the full build because
//! they need the compiled .so / .rlib to load or link against.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::cache::ArtifactCache;
use crate::executor::{self, SourceOverride};
use crate::graph::BuildGraph;
use crate::progress::Progress;

pub struct SchedulerResult {
    pub succeeded: usize,
    pub cached: usize,
    pub failed: usize,
    pub total_duration: std::time::Duration,
}

struct SharedState {
    ready: Vec<String>,
    /// Number of unsatisfied deps per crate. A dep is "satisfied" when:
    /// - its metadata is ready (for regular rlib deps), OR
    /// - its full build is done (for proc-macro / build deps)
    pending_deps: HashMap<String, usize>,
    dependents: HashMap<String, Vec<String>>,
    /// Which deps require full-build (proc-macro or build dep)?
    needs_full_build: HashMap<String, HashSet<String>>,
    output_map: BTreeMap<String, PathBuf>,
    succeeded: usize,
    cached: usize,
    failed: usize,
    abort: bool,
    in_flight: usize,
    total: usize,
}

impl SharedState {
    fn all_done(&self) -> bool {
        self.abort || (self.ready.is_empty() && self.in_flight == 0)
    }
}

pub fn run_parallel(
    graph: &BuildGraph,
    cache: &ArtifactCache,
    jobs: usize,
    bash: &str,
    stdenv_path: &str,
    overrides: &HashMap<String, SourceOverride>,
) -> SchedulerResult {
    let start = std::time::Instant::now();

    // Override-aware helpers. Any drv with a SourceOverride uses a composite
    // cache key (drv_path + effective source hash); plain `is_cached(drv)` would
    // wrongly hit the stale pre-override artifact.
    let key_for = |drv: &str| -> String {
        match overrides.get(drv) {
            Some(ov) => ArtifactCache::cache_key_with_source(drv, &ov.source_hash),
            None => ArtifactCache::cache_key(drv),
        }
    };
    let is_cached = |drv: &str| cache.is_cached_key(&key_for(drv));
    let artifact_dir = |drv: &str| cache.artifact_dir_by_key(&key_for(drv));

    let mut pending_deps: HashMap<String, usize> = HashMap::new();
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
    let mut needs_full_build: HashMap<String, HashSet<String>> = HashMap::new();
    let mut output_map: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut ready: Vec<String> = Vec::new();
    let mut cached = 0;

    // Build output_path → drv_path map for identifying build deps
    let mut output_to_drv: HashMap<String, String> = HashMap::new();
    for (drv_path, node) in &graph.nodes {
        for out in node.drv.outputs.values() {
            output_to_drv.insert(out.path.clone(), drv_path.clone());
        }
    }

    // Identify proc-macro crates from crateType (procMacro field is unreliable)
    let mut is_proc_macro: HashSet<String> = HashSet::new();
    for (drv_path, node) in &graph.nodes {
        let pm = node.drv.env.get("procMacro")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);
        let pm_crate_type = node.drv.env.get("crateType")
            .map(|v| v.contains("proc-macro"))
            .unwrap_or(false);
        if pm || pm_crate_type {
            is_proc_macro.insert(drv_path.clone());
        }
    }

    for (drv_path, node) in &graph.nodes {
        if is_cached(drv_path) {
            let artifact = artifact_dir(drv_path);
            for (name, out) in &node.drv.outputs {
                output_map.insert(out.path.clone(), artifact.join(name));
            }
            cached += 1;
            continue;
        }

        // Identify which deps need full build (proc-macro or build dep)
        let build_dep_paths: HashSet<String> = node.drv.env
            .get("completeBuildDeps")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        let mut full_set = HashSet::new();
        for dep_drv in &node.crate_deps {
            if is_proc_macro.contains(dep_drv) {
                full_set.insert(dep_drv.clone());
            }
        }
        // Build deps: match output paths back to drv paths
        for bdp in &build_dep_paths {
            if let Some(dep_drv) = output_to_drv.get(bdp) {
                if graph.nodes.contains_key(dep_drv) {
                    full_set.insert(dep_drv.clone());
                }
            }
        }

        let uncached_deps: Vec<&String> = node
            .crate_deps
            .iter()
            .filter(|dep| !is_cached(dep))
            .collect();

        pending_deps.insert(drv_path.clone(), uncached_deps.len());

        for dep in &uncached_deps {
            dependents
                .entry((*dep).clone())
                .or_default()
                .push(drv_path.clone());
        }

        if !full_set.is_empty() {
            needs_full_build.insert(drv_path.clone(), full_set);
        }

        if uncached_deps.is_empty() {
            ready.push(drv_path.clone());
        }
    }

    let to_build = pending_deps.len();
    let progress = Arc::new(Progress::new(to_build, cached));

    if to_build == 0 {
        progress.summary(0, cached, 0, start.elapsed());
        return SchedulerResult {
            succeeded: 0,
            cached,
            failed: 0,
            total_duration: start.elapsed(),
        };
    }

    let state = Arc::new((
        Mutex::new(SharedState {
            ready,
            pending_deps,
            dependents,
            needs_full_build,
            output_map,
            succeeded: 0,
            cached: 0,
            failed: 0,
            abort: false,
            in_flight: 0,
            total: to_build,
        }),
        Condvar::new(),
    ));

    thread::scope(|s| {
        for _ in 0..jobs {
            let state = Arc::clone(&state);
            let progress = Arc::clone(&progress);
            s.spawn(move || {
                let mut worker = crate::worker::Worker::spawn(bash, stdenv_path)
                    .expect("spawning worker");
                worker_loop(&state, &graph.nodes, cache, &mut worker, &progress, overrides);
            });
        }
    });

    let s = state.0.lock().unwrap();

    progress.summary(s.succeeded, cached + s.cached, s.failed, start.elapsed());

    SchedulerResult {
        succeeded: s.succeeded,
        cached: cached + s.cached,
        failed: s.failed,
        total_duration: start.elapsed(),
    }
}

fn worker_loop(
    state: &(Mutex<SharedState>, Condvar),
    nodes: &BTreeMap<String, crate::graph::CrateNode>,
    cache: &ArtifactCache,
    worker: &mut crate::worker::Worker,
    progress: &Progress,
    overrides: &HashMap<String, SourceOverride>,
) {
    let (lock, cvar) = state;

    loop {
        let (drv_path, dep_map) = {
            let mut s = lock.lock().unwrap();

            while s.ready.is_empty() && !s.all_done() {
                s = cvar.wait(s).unwrap();
            }

            if s.all_done() {
                return;
            }

            let drv_path = s.ready.pop().unwrap();
            s.in_flight += 1;

            let node = &nodes[&drv_path];
            let mut dep_map: BTreeMap<String, PathBuf> = BTreeMap::new();
            for dep_drv in &node.crate_deps {
                if let Some(dep_node) = nodes.get(dep_drv) {
                    for out in dep_node.drv.outputs.values() {
                        if let Some(cache_path) = s.output_map.get(&out.path) {
                            dep_map.insert(out.path.clone(), cache_path.clone());
                        }
                    }
                }
            }

            (drv_path, dep_map)
        };

        let node = &nodes[&drv_path];
        let crate_name = node.drv.env.get("crateName")
            .cloned()
            .unwrap_or_else(|| "unknown".into());

        progress.start(&crate_name);

        let rewriter = executor::make_rewriter(&node.drv, &dep_map);
        let src_ov = overrides.get(&drv_path);

        // Build with mid-build metadata signaling.
        // When __META_READY__ fires, unlock dependents that only need .rmeta.
        let drv_path_clone = drv_path.clone();
        let state_ref = state;

        let result = {
            let effective_key = match src_ov {
                Some(ov) => ArtifactCache::cache_key_with_source(&drv_path, &ov.source_hash),
                None => ArtifactCache::cache_key(&drv_path),
            };

            executor::build_crate_with_worker_signaled(
                &drv_path,
                &node.drv,
                cache,
                &rewriter,
                worker,
                src_ov,
                |_rmeta_dir| {
                    // Mid-build: metadata is ready. Unlock dependents
                    // that only need .rmeta (not proc-macro/build deps).
                    let mut s = lock.lock().unwrap();
                    unlock_dependents_meta(&mut s, &drv_path_clone, nodes);
                    cvar.notify_all();
                },
            )
        };

        {
            let mut s = lock.lock().unwrap();
            s.in_flight -= 1;

            match result {
                Ok(ref r) if r.success => {
                    s.succeeded += 1;
                    progress.finish(&crate_name, r.duration);

                    let key = match overrides.get(&drv_path) {
                        Some(ov) => ArtifactCache::cache_key_with_source(&drv_path, &ov.source_hash),
                        None => ArtifactCache::cache_key(&drv_path),
                    };
                    let artifact = cache.artifact_dir_by_key(&key);
                    for (name, out) in &node.drv.outputs {
                        s.output_map.insert(out.path.clone(), artifact.join(name));
                    }

                    // Full build done: unlock dependents that need full
                    // build (proc-macro consumers, build-dep consumers).
                    // Also unlock any that weren't unlocked by metadata
                    // (crates without lib targets skip __META_READY__).
                    let meta_was_signaled = r.rmeta_dir.is_some();
                    unlock_dependents_full(&mut s, &drv_path, meta_was_signaled, nodes);
                }
                Ok(ref r) => {
                    s.failed += 1;
                    s.abort = true;
                    progress.fail(&crate_name, &r.stdout, &r.stderr);
                }
                Err(e) => {
                    s.failed += 1;
                    s.abort = true;
                    progress.fail(&crate_name, "", &e);
                }
            }

            cvar.notify_all();
        }
    }
}

/// Unlock dependents that only need metadata (regular rlib deps).
/// Called mid-build when __META_READY__ fires.
fn unlock_dependents_meta(
    s: &mut SharedState,
    finished_drv: &str,
    nodes: &BTreeMap<String, crate::graph::CrateNode>,
) {
    let deps = match s.dependents.get(finished_drv).cloned() {
        Some(d) => d,
        None => return,
    };

    for dep_drv in &deps {
        // Skip dependents that need full build from this dep
        let needs_full = s.needs_full_build.get(dep_drv)
            .map(|set| set.contains(finished_drv))
            .unwrap_or(false);
        if needs_full {
            continue;
        }

        if let Some(count) = s.pending_deps.get_mut(dep_drv) {
            if *count == 0 { continue; }
            *count -= 1;
            if *count == 0 {
                s.ready.push(dep_drv.clone());
            }
        }
    }
}

/// Unlock dependents that need full build (proc-macro / build-dep consumers),
/// AND any dependents that were never unlocked by __META_READY__ (bin-only
/// crates, or crates where the signal was missed).
fn unlock_dependents_full(
    s: &mut SharedState,
    finished_drv: &str,
    meta_was_signaled: bool,
    _nodes: &BTreeMap<String, crate::graph::CrateNode>,
) {
    let deps = match s.dependents.get(finished_drv).cloned() {
        Some(d) => d,
        None => return,
    };

    for dep_drv in &deps {
        if let Some(count) = s.pending_deps.get_mut(dep_drv) {
            if *count == 0 { continue; }

            let needs_full = s.needs_full_build.get(dep_drv)
                .map(|set| set.contains(finished_drv))
                .unwrap_or(false);

            if needs_full {
                // Was waiting for full build — now satisfied
                *count -= 1;
            } else if !meta_was_signaled {
                // Regular dep, but __META_READY__ was never sent
                // (bin-only crate or no lib target). Decrement now.
                *count -= 1;
            }
            // If !needs_full && meta_was_signaled, already decremented
            // by unlock_dependents_meta — skip.

            if *count == 0 {
                s.ready.push(dep_drv.clone());
            }
        }
    }
}
