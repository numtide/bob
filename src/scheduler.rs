//! Parallel build scheduler.
//!
//! Executes crate builds in parallel, respecting the dependency DAG.
//! Uses std::thread with a work-stealing approach:
//! - Maintain a set of "ready" crates (all deps built)
//! - Spawn up to N workers that pull from the ready set
//! - When a crate finishes, check if any dependents become ready

use std::collections::{BTreeMap, HashMap};
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
    pending_deps: HashMap<String, usize>,
    dependents: HashMap<String, Vec<String>>,
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

    let mut pending_deps: HashMap<String, usize> = HashMap::new();
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
    let mut output_map: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut ready: Vec<String> = Vec::new();
    let mut cached = 0;

    for (drv_path, node) in &graph.nodes {
        let is_cached = if let Some(ov) = overrides.get(drv_path) {
            let key = ArtifactCache::cache_key_with_source(drv_path, &ov.source_hash);
            cache.is_cached_key(&key)
        } else {
            cache.is_cached(drv_path)
        };

        if is_cached {
            let artifact = if let Some(ov) = overrides.get(drv_path) {
                let key = ArtifactCache::cache_key_with_source(drv_path, &ov.source_hash);
                cache.artifact_dir_by_key(&key)
            } else {
                cache.artifact_dir(drv_path)
            };
            for (name, out) in &node.drv.outputs {
                output_map.insert(out.path.clone(), artifact.join(name));
            }
            cached += 1;
            continue;
        }

        let uncached_deps: Vec<&String> = node
            .crate_deps
            .iter()
            .filter(|dep| !cache.is_cached(dep))
            .collect();

        pending_deps.insert(drv_path.clone(), uncached_deps.len());

        for dep in &uncached_deps {
            dependents
                .entry((*dep).clone())
                .or_default()
                .push(drv_path.clone());
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
        let result = executor::build_crate_with_worker(&drv_path, &node.drv, cache, &rewriter, worker, src_ov);

        {
            let mut s = lock.lock().unwrap();
            s.in_flight -= 1;

            match result {
                Ok(ref r) if r.success => {
                    s.succeeded += 1;
                    progress.finish(&crate_name, r.duration);

                    for (name, out) in &node.drv.outputs {
                        s.output_map.insert(
                            out.path.clone(),
                            cache.artifact_dir(&drv_path).join(name),
                        );
                    }

                    if let Some(deps) = s.dependents.get(&drv_path).cloned() {
                        for dep in deps {
                            if let Some(count) = s.pending_deps.get_mut(&dep) {
                                *count -= 1;
                                if *count == 0 {
                                    s.ready.push(dep);
                                }
                            }
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
