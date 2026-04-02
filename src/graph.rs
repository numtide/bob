//! Build graph: walk .drv input_derivations to discover the full crate DAG,
//! topologically sort, and identify which crates need (re)building.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
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
    /// Non-crate input drv paths and their output paths, precomputed
    /// to avoid re-parsing drvs on every run.
    pub non_crate_input_map: BTreeMap<String, Vec<String>>,
}

impl BuildGraph {
    /// Build the graph, using a cached serialization when available.
    /// The cache key is blake3 of the sorted root drv paths — if the
    /// roots haven't changed (same nix eval), the graph is identical.
    pub fn from_roots_cached(root_drv_paths: &[String], cache_dir: &Path) -> Result<Self, String> {
        let mut hasher = blake3::Hasher::new();
        let mut sorted = root_drv_paths.to_vec();
        sorted.sort();
        for p in &sorted {
            hasher.update(p.as_bytes());
            hasher.update(b"\0");
        }
        let key = hasher.finalize().to_hex()[..32].to_string();
        let cache_path = cache_dir.join(format!("graph-{key}.bin"));

        if let Some(g) = Self::load_cached(&cache_path) {
            return Ok(g);
        }

        let g = Self::from_roots(root_drv_paths)?;
        g.save_cached(&cache_path);
        Ok(g)
    }

    /// Serialize graph to a compact binary format.
    /// Stores parsed Derivation fields directly to avoid re-parsing ATerm.
    fn save_cached(&self, path: &Path) {
        let mut buf = Vec::with_capacity(64 * 1024);
        buf.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());
        for (drv_path, node) in &self.nodes {
            write_str(&mut buf, drv_path);
            write_derivation(&mut buf, &node.drv);
            buf.extend_from_slice(&(node.crate_deps.len() as u32).to_le_bytes());
            for dep in &node.crate_deps {
                write_str(&mut buf, dep);
            }
        }
        buf.extend_from_slice(&(self.topo_order.len() as u32).to_le_bytes());
        for p in &self.topo_order {
            write_str(&mut buf, p);
        }
        // non_crate_input_map
        buf.extend_from_slice(&(self.non_crate_input_map.len() as u32).to_le_bytes());
        for (drv, outs) in &self.non_crate_input_map {
            write_str(&mut buf, drv);
            buf.extend_from_slice(&(outs.len() as u32).to_le_bytes());
            for o in outs {
                write_str(&mut buf, o);
            }
        }
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(path, &buf);
    }

    /// Load a cached graph. Nix store paths are content-addressed and
    /// immutable — if the drv path exists, its contents haven't changed.
    fn load_cached(path: &Path) -> Option<Self> {
        let buf = std::fs::read(path).ok()?;
        let mut pos = 0;

        let num_nodes = read_u32(&buf, &mut pos)?;
        let mut nodes = BTreeMap::new();
        for _ in 0..num_nodes {
            let drv_path = read_string(&buf, &mut pos)?;
            if !Path::new(&drv_path).exists() {
                return None;
            }
            let drv = read_derivation(&buf, &mut pos)?;
            let num_deps = read_u32(&buf, &mut pos)?;
            let mut crate_deps = Vec::with_capacity(num_deps as usize);
            for _ in 0..num_deps {
                crate_deps.push(read_string(&buf, &mut pos)?);
            }
            nodes.insert(drv_path.clone(), CrateNode { drv_path, drv, crate_deps });
        }

        let num_topo = read_u32(&buf, &mut pos)?;
        let mut topo_order = Vec::with_capacity(num_topo as usize);
        for _ in 0..num_topo {
            topo_order.push(read_string(&buf, &mut pos)?);
        }

        // non_crate_input_map
        let mut non_crate_input_map = BTreeMap::new();
        if let Some(num_nci) = read_u32(&buf, &mut pos) {
            for _ in 0..num_nci {
                let drv = read_string(&buf, &mut pos)?;
                let num_outs = read_u32(&buf, &mut pos)?;
                let mut outs = Vec::with_capacity(num_outs as usize);
                for _ in 0..num_outs {
                    outs.push(read_string(&buf, &mut pos)?);
                }
                non_crate_input_map.insert(drv, outs);
            }
        }

        Some(BuildGraph { nodes, topo_order, non_crate_input_map })
    }

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

        // Precompute non-crate input drv → output paths mapping
        let mut non_crate_input_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for node in nodes.values() {
            for (dep_drv, dep_outputs) in &node.drv.input_derivations {
                if crate_drv_paths.contains(dep_drv.as_str()) {
                    continue;
                }
                if non_crate_input_map.contains_key(dep_drv) {
                    continue;
                }
                let dep_drv_path = Path::new(dep_drv);
                if !dep_drv_path.exists() {
                    continue;
                }
                if let Ok(contents) = std::fs::read(dep_drv_path) {
                    if let Ok(dep) = Derivation::parse(&contents) {
                        let mut out_paths = Vec::new();
                        for out_name in dep_outputs {
                            if let Some(out) = dep.outputs.get(out_name) {
                                out_paths.push(out.path.clone());
                            }
                        }
                        non_crate_input_map.insert(dep_drv.clone(), out_paths);
                    }
                }
            }
        }

        Ok(BuildGraph { nodes, topo_order, non_crate_input_map })
    }

    pub fn crate_count(&self) -> usize {
        self.nodes.len()
    }

    /// Collect non-crate input drv paths that have unrealized outputs.
    /// Uses the precomputed map to avoid re-parsing drv files.
    pub fn non_crate_inputs(&self) -> Vec<String> {
        self.non_crate_input_map
            .iter()
            .filter(|(_, out_paths)| out_paths.iter().any(|p| !Path::new(p).exists()))
            .map(|(drv, _)| drv.clone())
            .collect()
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

fn write_derivation(buf: &mut Vec<u8>, drv: &Derivation) {
    // outputs
    buf.extend_from_slice(&(drv.outputs.len() as u32).to_le_bytes());
    for (name, out) in &drv.outputs {
        write_str(buf, name);
        write_str(buf, &out.path);
        write_str(buf, &out.hash_algo);
        write_str(buf, &out.hash);
    }
    // input_derivations
    buf.extend_from_slice(&(drv.input_derivations.len() as u32).to_le_bytes());
    for (path, outs) in &drv.input_derivations {
        write_str(buf, path);
        buf.extend_from_slice(&(outs.len() as u32).to_le_bytes());
        for o in outs {
            write_str(buf, o);
        }
    }
    // input_sources
    buf.extend_from_slice(&(drv.input_sources.len() as u32).to_le_bytes());
    for s in &drv.input_sources {
        write_str(buf, s);
    }
    write_str(buf, &drv.platform);
    write_str(buf, &drv.builder);
    // args
    buf.extend_from_slice(&(drv.args.len() as u32).to_le_bytes());
    for a in &drv.args {
        write_str(buf, a);
    }
    // env
    buf.extend_from_slice(&(drv.env.len() as u32).to_le_bytes());
    for (k, v) in &drv.env {
        write_str(buf, k);
        write_str(buf, v);
    }
}

fn read_derivation(buf: &[u8], pos: &mut usize) -> Option<Derivation> {
    use crate::drv::Output;
    let num_outputs = read_u32(buf, pos)?;
    let mut outputs = BTreeMap::new();
    for _ in 0..num_outputs {
        let name = read_string(buf, pos)?;
        let path = read_string(buf, pos)?;
        let hash_algo = read_string(buf, pos)?;
        let hash = read_string(buf, pos)?;
        outputs.insert(name, Output { path, hash_algo, hash });
    }
    let num_input_drvs = read_u32(buf, pos)?;
    let mut input_derivations = BTreeMap::new();
    for _ in 0..num_input_drvs {
        let path = read_string(buf, pos)?;
        let num_outs = read_u32(buf, pos)?;
        let mut outs = Vec::with_capacity(num_outs as usize);
        for _ in 0..num_outs {
            outs.push(read_string(buf, pos)?);
        }
        input_derivations.insert(path, outs);
    }
    let num_sources = read_u32(buf, pos)?;
    let mut input_sources = Vec::with_capacity(num_sources as usize);
    for _ in 0..num_sources {
        input_sources.push(read_string(buf, pos)?);
    }
    let platform = read_string(buf, pos)?;
    let builder = read_string(buf, pos)?;
    let num_args = read_u32(buf, pos)?;
    let mut args = Vec::with_capacity(num_args as usize);
    for _ in 0..num_args {
        args.push(read_string(buf, pos)?);
    }
    let num_env = read_u32(buf, pos)?;
    let mut env = BTreeMap::new();
    for _ in 0..num_env {
        let k = read_string(buf, pos)?;
        let v = read_string(buf, pos)?;
        env.insert(k, v);
    }
    Some(Derivation {
        outputs, input_derivations, input_sources,
        platform, builder, args, env,
    })
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn write_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
    buf.extend_from_slice(b);
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > buf.len() { return None; }
    let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().ok()?);
    *pos += 4;
    Some(v)
}

fn read_string(buf: &[u8], pos: &mut usize) -> Option<String> {
    let len = read_u32(buf, pos)? as usize;
    if *pos + len > buf.len() { return None; }
    let s = String::from_utf8(buf[*pos..*pos + len].to_vec()).ok()?;
    *pos += len;
    Some(s)
}

fn read_raw_bytes(buf: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    let len = read_u32(buf, pos)? as usize;
    if *pos + len > buf.len() { return None; }
    let b = buf[*pos..*pos + len].to_vec();
    *pos += len;
    Some(b)
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
