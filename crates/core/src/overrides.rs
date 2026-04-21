//! Source-change tracking + early-cutoff cache keying.
//!
//! A unit is **tracked** if it has a backend-supplied own-source hash
//! ([`OwnHash`]) or transitively depends on one via `unit_deps`. Tracked
//! units use a composite cache key
//!
//!   eff(c) = blake3( own(c)? ‖ sorted(propagated(dep) for dep ∈ tracked_deps(c)) )
//!
//! where `propagated(dep)` is the hash of dep's **committed output**
//! (`artifacts/<key>/.out-hash`), not dep's inputs. So a rebuild that
//! produces a byte-identical artifact leaves dependents' keys unchanged and
//! they cache-hit — Bazel/Shake-style early cutoff.
//!
//! Because `propagated(dep)` is only known once dep has committed (or
//! cache-hit), `eff(c)` can't be precomputed for the whole graph. The
//! scheduler computes it at the moment `c` becomes ready; see
//! `scheduler::run_parallel`. This module supplies:
//!   - [`tracked_set`]: which units are tracked (cheap topo walk),
//!   - [`eff_hash`]: the per-unit key derivation given resolved dep hashes.
//!
//! Units outside `tracked` keep the plain `blake3(drv_path)` key and are
//! cache-checked upfront exactly as before.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::graph::BuildGraph;

/// Per-unit own-source hash + live source directory, supplied by the backend.
pub struct OwnHash {
    pub hash: String,
    pub src_dir: PathBuf,
}

/// Units that need a composite (early-cutoff) cache key: those with an
/// own-hash, plus everything downstream of one along `unit_deps`. External
/// (registry) deps are leaves w.r.t. workspace units, so they stay untracked
/// and keep the plain drv-path key.
pub fn tracked_set(g: &BuildGraph, own: &HashMap<String, OwnHash>) -> HashSet<String> {
    let mut tracked: HashSet<String> = own.keys().cloned().collect();
    for drv in &g.topo_order {
        if tracked.contains(drv) {
            continue;
        }
        if g.nodes[drv].unit_deps.iter().any(|d| tracked.contains(d)) {
            tracked.insert(drv.clone());
        }
    }
    tracked
}

/// Compute `eff(c)` once every tracked dep's propagated hash is known.
///
/// `dep_propagated` yields `Some(hash)` for tracked deps whose output hash
/// has been resolved (built this run, or read from a cache-hit's
/// `.out-hash`); untracked deps return `None` and don't contribute (their
/// drv path is already baked into `c.drv_path`, so they're covered by the
/// plain key component).
///
/// A tracked dep with no recorded propagated hash (first-ever build, or a
/// pre-cutoff artifact missing `.out-hash`) falls back to its own eff-key —
/// callers pass that as the value, which degrades this run to the old
/// input-cascade behaviour for that edge and writes `.out-hash` on commit
/// so the next run cuts off.
pub fn eff_hash<'a>(
    own: Option<&OwnHash>,
    unit_deps: impl Iterator<Item = &'a str>,
    dep_propagated: impl Fn(&str) -> Option<&'a str>,
) -> String {
    let mut deps: Vec<&str> = unit_deps.filter_map(dep_propagated).collect();
    deps.sort_unstable();
    let mut h = blake3::Hasher::new();
    if let Some(o) = own {
        h.update(o.hash.as_bytes());
    }
    for d in deps {
        h.update(b"\0");
        h.update(d.as_bytes());
    }
    h.finalize().to_hex()[..32].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(h: &str) -> OwnHash {
        OwnHash {
            hash: h.into(),
            src_dir: PathBuf::new(),
        }
    }

    /// The cutoff property: a dependent's eff-hash is a function of its own
    /// source and its tracked deps' *propagated* (output) hashes only. If a
    /// dep rebuilds to an identical output, the dependent's key is unchanged.
    #[test]
    fn eff_hash_cutoff_semantics() {
        let deps = ["/nix/store/a.drv", "/nix/store/b.drv", "/nix/store/reg.drv"];
        let prop1: HashMap<&str, &str> = [
            ("/nix/store/a.drv", "out-a-v1"),
            ("/nix/store/b.drv", "out-b"),
        ]
        .into();
        let prop2: HashMap<&str, &str> = [
            ("/nix/store/a.drv", "out-a-v1"),
            ("/nix/store/b.drv", "out-b"),
        ]
        .into();
        let prop3: HashMap<&str, &str> = [
            ("/nix/store/a.drv", "out-a-v2"),
            ("/nix/store/b.drv", "out-b"),
        ]
        .into();

        let k1 = eff_hash(Some(&own("me")), deps.iter().copied(), |d| {
            prop1.get(d).copied()
        });
        // Same propagated hashes (dep `a` rebuilt but output unchanged) → same key.
        let k2 = eff_hash(Some(&own("me")), deps.iter().copied(), |d| {
            prop2.get(d).copied()
        });
        assert_eq!(k1, k2, "unchanged dep output must not move dependent's key");

        // Dep `a`'s output actually changed → key moves.
        let k3 = eff_hash(Some(&own("me")), deps.iter().copied(), |d| {
            prop3.get(d).copied()
        });
        assert_ne!(k1, k3);

        // Untracked dep (`reg.drv`, no propagated entry) doesn't contribute.
        let k4 = eff_hash(
            Some(&own("me")),
            ["/nix/store/a.drv", "/nix/store/b.drv"].iter().copied(),
            |d| prop1.get(d).copied(),
        );
        assert_eq!(k1, k4, "untracked deps must be transparent to eff_hash");

        // Own-source change moves the key independently.
        let k5 = eff_hash(Some(&own("me2")), deps.iter().copied(), |d| {
            prop1.get(d).copied()
        });
        assert_ne!(k1, k5);

        // Dep order doesn't matter (sorted internally).
        let k6 = eff_hash(
            Some(&own("me")),
            ["/nix/store/b.drv", "/nix/store/a.drv"].iter().copied(),
            |d| prop1.get(d).copied(),
        );
        assert_eq!(k1, k6);
    }
}
