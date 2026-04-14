//! Parallel build scheduler with rmeta pipelining.
//!
//! Executes crate builds in parallel, respecting the dependency DAG.
//! For lib→lib edges, dependents start as soon as the upstream's fat rmeta
//! (from `--emit=metadata,link`, contains MIR) is written, not when the rlib
//! is done. proc-macro deps and deps with build.rs (which emit `link`/`env`
//! files that downstream's configurePhase reads) gate on full completion.
//!
//! Mechanism:
//! - Worker threads pull from `ready`, build, and on completion decrement
//!   `pending_done` counters of dependents.
//! - The rustc wrapper writes `__META_READY__ <dir>` to fd 3 (the worker's
//!   saved stdout) the moment rustc emits the fat rmeta. The worker's reader
//!   dispatches that to an in-process callback that decrements `pending_rmeta`.
//! - A crate becomes ready when both counters hit zero.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;

use crate::cache::ArtifactCache;
use crate::executor::SourceOverride;
use crate::executor::{self, PipelineConfig};
use crate::graph::BuildGraph;
use crate::progress::Progress;
use crate::rust::pipeline::{
    artifact_has_link_output, is_pipelineable, lib_filename, needs_link_output,
};

pub struct SchedulerResult {
    pub failed: usize,
}

struct SharedState {
    ready: Vec<String>,
    /// Count of deps whose rmeta we're waiting on (pipelineable deps).
    pending_rmeta: HashMap<String, usize>,
    /// Count of deps whose full build we're waiting on (proc-macro/build.rs).
    pending_done: HashMap<String, usize>,
    /// dep → dependents waiting on its rmeta.
    rmeta_dependents: HashMap<String, Vec<String>>,
    /// dep → dependents waiting on its full completion.
    done_dependents: HashMap<String, Vec<String>>,
    /// store-output-path → cache/tmp path. Populated for cached crates up
    /// front, and for in-flight crates as soon as they START (pointing at
    /// tmp/<key>/lib so downstream can read the early rmeta).
    output_map: BTreeMap<String, PathBuf>,
    /// drv_paths whose rmeta has been signalled. The fd-3 signal and the
    /// post-success catch-up both call `fire_rmeta`; this set guarantees
    /// idempotence so dependents are decremented exactly once.
    rmeta_fired: HashSet<String>,
    succeeded: usize,
    cached: usize,
    failed: usize,
    abort: bool,
    in_flight: usize,
}

impl SharedState {
    fn maybe_ready(&mut self, drv: &str) {
        if self.pending_rmeta.get(drv).copied() == Some(0)
            && self.pending_done.get(drv).copied() == Some(0)
        {
            // Guard against double-push: remove from pending maps.
            self.pending_rmeta.remove(drv);
            self.pending_done.remove(drv);
            self.ready.push(drv.to_string());
        }
    }

    fn fire_rmeta(&mut self, dep: &str) {
        if !self.rmeta_fired.insert(dep.to_string()) {
            return;
        }
        if let Some(ds) = self.rmeta_dependents.remove(dep) {
            for d in ds {
                if let Some(c) = self.pending_rmeta.get_mut(&d) {
                    *c -= 1;
                }
                self.maybe_ready(&d);
            }
        }
    }
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
    roots: &[String],
) -> SchedulerResult {
    let roots: HashSet<&str> = roots.iter().map(String::as_str).collect();
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
    let artifact_dir = |drv: &str| cache.artifact_dir_by_key(&key_for(drv));
    let is_cached = |drv: &str, node: &crate::graph::UnitNode| -> bool {
        if !cache.is_cached_key(&key_for(drv)) {
            return false;
        }
        // Root cdylib targets must have the .so; a prior non-root run may have
        // committed rlib-only. Absence isn't an error — just means "rebuild".
        if roots.contains(drv) && needs_link_output(&node.drv) {
            return artifact_has_link_output(&artifact_dir(drv));
        }
        true
    };
    let tmp_dir = |drv: &str| cache.root().join("tmp").join(key_for(drv));

    let pipelineable: HashMap<String, bool> = graph
        .nodes
        .iter()
        .map(|(k, n)| (k.clone(), is_pipelineable(&n.drv)))
        .collect();

    let mut pending_rmeta: HashMap<String, usize> = HashMap::new();
    let mut pending_done: HashMap<String, usize> = HashMap::new();
    let mut rmeta_dependents: HashMap<String, Vec<String>> = HashMap::new();
    let mut done_dependents: HashMap<String, Vec<String>> = HashMap::new();
    let mut output_map: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut ready: Vec<String> = Vec::new();
    let mut cached = 0;
    let mut to_build = 0;

    for (drv_path, node) in &graph.nodes {
        if is_cached(drv_path, node) {
            let artifact = artifact_dir(drv_path);
            for (name, out) in &node.drv.outputs {
                output_map.insert(out.path.clone(), artifact.join(name));
            }
            cached += 1;
            continue;
        }
        to_build += 1;

        let uncached_deps: Vec<&String> = node
            .unit_deps
            .iter()
            .filter(|dep| !is_cached(dep, &graph.nodes[dep.as_str()]))
            .collect();

        let mut n_rmeta = 0usize;
        let mut n_done = 0usize;
        for dep in &uncached_deps {
            if *pipelineable.get(dep.as_str()).unwrap_or(&false) {
                n_rmeta += 1;
                rmeta_dependents
                    .entry((*dep).clone())
                    .or_default()
                    .push(drv_path.clone());
            } else {
                n_done += 1;
                done_dependents
                    .entry((*dep).clone())
                    .or_default()
                    .push(drv_path.clone());
            }
        }
        pending_rmeta.insert(drv_path.clone(), n_rmeta);
        pending_done.insert(drv_path.clone(), n_done);

        if n_rmeta == 0 && n_done == 0 {
            ready.push(drv_path.clone());
        }
    }
    let progress = Arc::new(Progress::new(to_build, cached));

    if to_build == 0 {
        progress.summary(0, cached, 0, start.elapsed());
        return SchedulerResult { failed: 0 };
    }

    let state = Arc::new((
        Mutex::new(SharedState {
            ready,
            pending_rmeta,
            pending_done,
            rmeta_dependents,
            done_dependents,
            output_map,
            rmeta_fired: HashSet::new(),
            succeeded: 0,
            cached: 0,
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
            let tmp_dir = &tmp_dir;
            let roots = &roots;
            s.spawn(move || {
                let mut worker =
                    crate::worker::Worker::spawn(bash, stdenv_path).expect("spawning worker");
                worker_loop(
                    &state,
                    &graph.nodes,
                    cache,
                    &mut worker,
                    &progress,
                    overrides,
                    tmp_dir,
                    roots,
                );
            });
        }
    });

    let s = state.0.lock().unwrap();

    progress.summary(s.succeeded, cached + s.cached, s.failed, start.elapsed());

    SchedulerResult { failed: s.failed }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    state: &(Mutex<SharedState>, Condvar),
    nodes: &BTreeMap<String, crate::graph::UnitNode>,
    cache: &ArtifactCache,
    worker: &mut crate::worker::Worker,
    progress: &Progress,
    overrides: &HashMap<String, SourceOverride>,
    tmp_dir: &dyn Fn(&str) -> PathBuf,
    roots: &HashSet<&str>,
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
            let my_tmp = tmp_dir(&drv_path);

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
            // our early rmeta and (later) rlib. Transitive metadata baked into
            // downstream rmetas will reference these tmp/ paths, so commit
            // hardlink-copies tmp→artifacts and leaves tmp/ intact for the
            // remainder of the run.
            for (name, out) in &node.drv.outputs {
                s.output_map.insert(out.path.clone(), my_tmp.join(name));
            }

            // Build dep_map (store-path → cache-or-tmp path) for direct deps.
            // In-flight deps' --extern resolution and transitive lookup are
            // handled at rustc time by rustc_wrap::resolve_lib_deps, which
            // symlinks each in-flight dep's early rmeta into target/deps and
            // re-resolves missing externs by metadata hash.
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

            (drv_path, dep_map)
        };

        let node = &nodes[&drv_path];
        let crate_name = node
            .drv
            .env
            .get("crateName")
            .cloned()
            .unwrap_or_else(|| "unknown".into());

        progress.start(&crate_name);

        let rewriter = executor::make_rewriter(&node.drv, &dep_map);
        let src_ov = overrides.get(&drv_path);
        let pl = PipelineConfig {
            expected_rmeta: lib_filename(&node.drv, "rmeta").unwrap_or_default(),
            // Only the explicit build targets need their cdylib/staticlib
            // output; for transitive deps the rlib is all anyone reads.
            skip_link_pass: !roots.contains(drv_path.as_str()),
        };
        let result = executor::build_unit(
            &drv_path,
            &node.drv,
            cache,
            &rewriter,
            worker,
            src_ov,
            &pl,
            |_rmeta_dir| {
                let mut s = lock.lock().unwrap();
                s.fire_rmeta(&drv_path);
                cvar.notify_all();
            },
        );

        {
            let mut s = lock.lock().unwrap();
            s.in_flight -= 1;

            match result {
                Ok(ref r) if r.success => {
                    s.succeeded += 1;
                    progress.finish(&crate_name, r.duration);

                    // output_map already points at tmp/<key>/ (registered when
                    // we started); commit hardlink-copies tmp→artifacts but
                    // leaves tmp/ intact, so there's no need to repoint and
                    // doing so would change the path embedded in downstream
                    // metadata mid-run.

                    // Catch-up for crates that never signalled rmeta (proc-
                    // macros, bin-only, build.rs probe with no lib target,
                    // crates with `links`): unblock rmeta-waiters now.
                    // `rmeta_fired` makes this idempotent if fd-3 already fired.
                    s.fire_rmeta(&drv_path);

                    if let Some(ds) = s.done_dependents.remove(&drv_path) {
                        for d in ds {
                            if let Some(c) = s.pending_done.get_mut(&d) {
                                *c -= 1;
                            }
                            s.maybe_ready(&d);
                        }
                    }
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
