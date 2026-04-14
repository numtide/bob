//! Cascading source-change invalidation through the build DAG.
//!
//! The eval cache reuses a drv keyed only on the lockfile, so the drv's
//! baked-in `src` store paths are stale once any workspace source changes.
//! Rather than re-evaluate, we compute an *effective hash* per unit and mix
//! it into the cache key:
//!
//! 1. The backend supplies an own-source hash + live source dir for every
//!    workspace unit it can locate in the graph (`OwnHash`).
//! 2. We walk the topo order: `eff(c) = blake3(own(c) ‖ sorted(eff(dep) …))`.
//!    Only units that have an own-hash, or depend (transitively) on one that
//!    does, get an entry. External (registry) deps are leaves w.r.t.
//!    workspace units, so they stay on the plain `blake3(drv_path)` key.
//! 3. Each entry becomes a `SourceOverride`: the scheduler uses
//!    `cache_key_with_source(drv, eff)` for these, so editing one workspace
//!    unit produces a new key for it and every downstream workspace unit
//!    while everything else stays cached.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::executor::SourceOverride;
use crate::graph::BuildGraph;

/// Per-unit own-source hash + live source directory, supplied by the backend.
pub struct OwnHash {
    pub hash: String,
    pub src_dir: PathBuf,
}

/// Cascade `own` hashes through the graph in topo order to produce
/// `SourceOverride`s. See module doc for the algorithm.
pub fn cascade(g: &BuildGraph, own: HashMap<String, OwnHash>) -> HashMap<String, SourceOverride> {
    let mut eff: HashMap<String, String> = HashMap::new();
    for drv in &g.topo_order {
        let node = &g.nodes[drv];
        let mine = own.get(drv);
        let mut dep_effs: Vec<&str> = node
            .unit_deps
            .iter()
            .filter_map(|d| eff.get(d).map(String::as_str))
            .collect();
        if mine.is_none() && dep_effs.is_empty() {
            // Pure external-dep subgraph — stable, plain key.
            continue;
        }
        dep_effs.sort_unstable();
        let mut h = blake3::Hasher::new();
        if let Some(o) = mine {
            h.update(o.hash.as_bytes());
        }
        for d in dep_effs {
            h.update(b"\0");
            h.update(d.as_bytes());
        }
        eff.insert(drv.clone(), h.finalize().to_hex()[..32].to_string());
    }

    let count = eff.len();
    let result: HashMap<String, SourceOverride> = eff
        .into_iter()
        .map(|(drv, hash)| {
            let ov = SourceOverride {
                src_path: own.get(&drv).map(|o| o.src_dir.clone()),
                source_hash: hash,
            };
            (drv, ov)
        })
        .collect();

    eprintln!("  \x1b[2mTracking {count} workspace unit(s) for source changes\x1b[0m");
    result
}
