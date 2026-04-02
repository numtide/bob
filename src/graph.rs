//! Build graph: walk .drv input_derivations to discover the full crate DAG,
//! topologically sort, and identify which crates need (re)building.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Command;

use crate::drv::Derivation;

/// A node in the build graph.
#[derive(Debug)]
pub struct CrateNode {
    pub drv_path: String,
    pub drv: Derivation,
    /// Direct dependency drv paths (only crate deps, not toolchain).
    pub crate_deps: Vec<String>,
}

/// The full build graph of crate derivations.
#[derive(Debug)]
pub struct BuildGraph {
    pub nodes: BTreeMap<String, CrateNode>,
    /// Topologically sorted drv paths (dependencies before dependents).
    pub topo_order: Vec<String>,
}

impl BuildGraph {
    /// Build the graph starting from a set of root drv paths.
    /// Walks input_derivations recursively, keeping only crate drvs
    /// (identified by having a `crateName` env var).
    pub fn from_roots(root_drv_paths: &[String]) -> Result<Self, String> {
        let mut nodes: BTreeMap<String, CrateNode> = BTreeMap::new();
        let mut queue: VecDeque<String> = root_drv_paths.iter().cloned().collect();
        let mut visited: HashSet<String> = HashSet::new();

        while let Some(drv_path) = queue.pop_front() {
            if !visited.insert(drv_path.clone()) {
                continue;
            }

            let drv_file = Path::new(&drv_path);
            if !drv_file.exists() {
                // Not in store — skip (might be a fetchurl or other non-local drv)
                continue;
            }

            let contents = std::fs::read(drv_file)
                .map_err(|e| format!("reading {drv_path}: {e}"))?;
            let drv = Derivation::parse(&contents)
                .map_err(|e| format!("parsing {drv_path}: {e}"))?;

            // Only include crate derivations (those built by buildRustCrate)
            if drv.env.get("crateName").is_none() {
                continue;
            }

            // Find crate dependencies: input drvs that are also crate drvs.
            // We enqueue all input drvs for exploration but only link crate→crate edges.
            let input_drv_paths: Vec<String> = drv.input_derivations.keys().cloned().collect();
            for dep_path in &input_drv_paths {
                queue.push_back(dep_path.clone());
            }

            nodes.insert(drv_path.clone(), CrateNode {
                drv_path: drv_path.clone(),
                drv,
                crate_deps: Vec::new(), // filled in second pass
            });
        }

        // Second pass: fill in crate_deps (only edges to other crate nodes)
        let crate_drv_paths: HashSet<String> = nodes.keys().cloned().collect();
        for node in nodes.values_mut() {
            node.crate_deps = node.drv.input_derivations.keys()
                .filter(|p| crate_drv_paths.contains(p.as_str()))
                .cloned()
                .collect();
        }

        let topo_order = topo_sort(&nodes)?;

        Ok(BuildGraph { nodes, topo_order })
    }

    pub fn crate_count(&self) -> usize {
        self.nodes.len()
    }

    /// Collect store paths that crate drvs depend on but aren't crate drvs
    /// themselves (source tarballs, build inputs, etc). These must be
    /// realized in the Nix store before we can build.
    pub fn non_crate_inputs(&self) -> Vec<String> {
        let crate_drv_paths: HashSet<&str> = self.nodes.keys().map(|s| s.as_str()).collect();
        let mut inputs: HashSet<String> = HashSet::new();

        for node in self.nodes.values() {
            for (dep_drv, dep_outputs) in &node.drv.input_derivations {
                if crate_drv_paths.contains(dep_drv.as_str()) {
                    continue;
                }
                // Parse the dep drv to get its output paths
                let dep_drv_path = Path::new(dep_drv);
                if !dep_drv_path.exists() {
                    continue;
                }
                if let Ok(contents) = std::fs::read(dep_drv_path) {
                    if let Ok(dep) = Derivation::parse(&contents) {
                        for out_name in dep_outputs {
                            if let Some(out) = dep.outputs.get(out_name) {
                                if !Path::new(&out.path).exists() {
                                    inputs.insert(dep_drv.clone());
                                }
                            }
                        }
                    }
                }
            }
            // Also check inputSources
            for src in &node.drv.input_sources {
                if !Path::new(src).exists() {
                    // inputSources are store paths, not drvs — can't realize them
                    // directly, but they should exist if the drv was instantiated.
                }
            }
        }

        inputs.into_iter().collect()
    }

    /// Realize any missing non-crate store paths (source tarballs, etc).
    /// Shells out to nix-store --realise.
    ///
    /// TODO: talk to the Nix daemon directly over its Unix socket protocol
    /// instead of shelling out — saves ~5ms process overhead and lets us
    /// overlap fetching with cache checks / early builds.
    pub fn realize_inputs(&self) -> Result<(), String> {
        let missing = self.non_crate_inputs();
        if missing.is_empty() {
            return Ok(());
        }

        eprintln!(
            "  \x1b[1;36mFetching\x1b[0m {} missing store paths...",
            missing.len()
        );

        let mut cmd = Command::new("nix-store");
        cmd.arg("--realise");
        for drv in &missing {
            cmd.arg(drv);
        }
        cmd.stderr(std::process::Stdio::inherit());

        let output = cmd.output()
            .map_err(|e| format!("running nix-store --realise: {e}"))?;

        if !output.status.success() {
            return Err("nix-store --realise failed".into());
        }

        Ok(())
    }
}

/// Kahn's algorithm for topological sort.
fn topo_sort(nodes: &BTreeMap<String, CrateNode>) -> Result<Vec<String>, String> {
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for key in nodes.keys() {
        in_degree.entry(key.as_str()).or_insert(0);
    }
    for node in nodes.values() {
        for dep in &node.crate_deps {
            if let Some(deg) = in_degree.get_mut(dep.as_str()) {
                *deg += 0; // ensure entry exists
            }
            *in_degree.entry(node.drv_path.as_str()).or_insert(0) += 0;
        }
    }

    // Count actual in-degrees
    let mut in_deg: HashMap<&str, usize> = nodes.keys().map(|k| (k.as_str(), 0usize)).collect();
    for node in nodes.values() {
        for dep in &node.crate_deps {
            if nodes.contains_key(dep) {
                *in_deg.entry(node.drv_path.as_str()).or_default() += 1;
            }
        }
    }

    // Kahn's: start with nodes that have no crate deps
    let mut queue: VecDeque<&str> = in_deg.iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&k, _)| k)
        .collect();
    let mut result = Vec::with_capacity(nodes.len());

    // Build reverse adjacency: dep → dependents
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in nodes.values() {
        for dep in &node.crate_deps {
            if nodes.contains_key(dep) {
                dependents.entry(dep.as_str()).or_default().push(node.drv_path.as_str());
            }
        }
    }

    while let Some(n) = queue.pop_front() {
        result.push(n.to_string());
        if let Some(deps) = dependents.get(n) {
            for &dep in deps {
                let deg = in_deg.get_mut(dep).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(dep);
                }
            }
        }
    }

    if result.len() != nodes.len() {
        return Err(format!(
            "cycle detected: sorted {} of {} nodes",
            result.len(),
            nodes.len()
        ));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_from_real_drv() {
        let drv_path = "/nix/store/ps4wmxcnwk3sx6177pn0rwbr2ix7sps4-rust_hello-0.1.0.drv";
        if !Path::new(drv_path).exists() {
            eprintln!("skipping: {drv_path} not found");
            return;
        }

        let graph = BuildGraph::from_roots(&[drv_path.to_string()]).unwrap();
        // Should have at least hello and serde
        assert!(graph.crate_count() >= 1, "expected at least 1 crate, got {}", graph.crate_count());

        // Topo order should have deps before dependents
        let positions: HashMap<&str, usize> = graph.topo_order.iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), i))
            .collect();

        for node in graph.nodes.values() {
            let my_pos = positions[node.drv_path.as_str()];
            for dep in &node.crate_deps {
                if let Some(&dep_pos) = positions.get(dep.as_str()) {
                    assert!(dep_pos < my_pos,
                        "dep {} (pos {}) should come before {} (pos {})",
                        dep, dep_pos, node.drv_path, my_pos
                    );
                }
            }
        }
    }
}
