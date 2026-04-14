mod cache;
mod drv;
mod executor;
mod graph;
mod progress;
mod resolve;
mod rewrite;
mod rustc_wrap;
mod scheduler;
mod worker;

use cache::ArtifactCache;
use std::path::Path;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        // Internal: rmeta-pipelining rustc shim. Dispatched first because it's
        // the hot path — invoked once per rustc call inside builds.
        "__rustc-wrap" => rustc_wrap::main(&args[2..]),
        "build" => cmd_build(&args[2..]),
        "clean" => cmd_clean(&args[2..]),
        "status" => cmd_status(),
        "parse-drv" => cmd_parse_drv(&args[2..]),
        "graph" => cmd_graph(&args[2..]),
        "help" | "--help" | "-h" => print_usage(),
        other => {
            eprintln!("unknown command: {other}");
            eprintln!();
            print_usage();
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!("bob — fast Rust builds via Nix drv replay + caching");
    eprintln!();
    eprintln!("usage: bob <command> [args...]");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  build [opts] <target>...   Build workspace members or drv paths");
    eprintln!("  clean [crate|--all]        Remove cached artifacts");
    eprintln!("  status                     Show cache statistics");
    eprintln!("  parse-drv <path>           Parse a .drv file and print contents");
    eprintln!("  graph <drv-path>...        Show dependency graph");
    eprintln!();
    eprintln!("build targets:");
    eprintln!("  <name>                     Workspace member (e.g., hello-rs)");
    eprintln!("  .                          Detect crate from current directory");
    eprintln!("  /nix/store/....drv         Raw drv path");
    eprintln!();
    eprintln!("build options:");
    eprintln!("  -j N                       Parallel jobs (default: nproc)");
    eprintln!("  --repo-root <path>         Repo root containing bob.nix (default: auto-detect)");
}

/// Find the repo root by walking up from cwd looking for `bob.nix`.
/// `bob.nix` is the per-repo glue that exposes
/// `workspaceMembers.<name>.build` for nix-instantiate resolution.
fn find_repo_root() -> Result<PathBuf, String> {
    if let Ok(r) = std::env::var("BOB_REPO_ROOT") {
        return Ok(PathBuf::from(r));
    }
    let mut dir = std::env::current_dir().map_err(|e| format!("getting cwd: {e}"))?;
    loop {
        if dir.join("bob.nix").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(
                "could not find repo root (no bob.nix found); pass --repo-root or set BOB_REPO_ROOT"
                    .into(),
            );
        }
    }
}

/// Detect workspace member name from cwd by reading Cargo.toml package name.
fn detect_member_from_cwd() -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("getting cwd: {e}"))?;

    // Walk up to find the nearest Cargo.toml with [package]
    let mut dir = cwd.as_path();
    loop {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists() {
            let contents = std::fs::read_to_string(&cargo_toml)
                .map_err(|e| format!("reading {}: {e}", cargo_toml.display()))?;

            // Look for `name = "..."` under [package]
            let mut in_package = false;
            for line in contents.lines() {
                let trimmed = line.trim();
                if trimmed == "[package]" {
                    in_package = true;
                    continue;
                }
                if trimmed.starts_with('[') {
                    in_package = false;
                    continue;
                }
                if in_package && trimmed.starts_with("name") {
                    if let Some(name) = extract_toml_string_value(trimmed) {
                        return Ok(name);
                    }
                }
            }
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return Err("no Cargo.toml with [package] name found".into()),
        }
    }
}

/// Extract string value from a TOML line like `name = "foo"`.
fn extract_toml_string_value(line: &str) -> Option<String> {
    let (_, rhs) = line.split_once('=')?;
    let rhs = rhs.trim();
    if rhs.starts_with('"') && rhs.ends_with('"') && rhs.len() >= 2 {
        Some(rhs[1..rhs.len() - 1].to_string())
    } else {
        None
    }
}

/// Resolve a build target to a drv path.
/// Accepts: "." (cwd detection), a member name, or a raw /nix/store/*.drv path.
fn resolve_target(
    target: &str,
    repo_root: &Path,
    cache: &ArtifactCache,
) -> Result<resolve::ResolveResult, String> {
    if target.starts_with("/nix/store/") && target.ends_with(".drv") {
        return Ok(resolve::ResolveResult {
            drv_path: target.to_string(),
        });
    }

    let member = if target == "." {
        detect_member_from_cwd()?
    } else {
        target.to_string()
    };

    let eval_cache = resolve::EvalCache::new(cache.root());
    eval_cache.resolve_one(repo_root, &member)
}

fn cmd_build(args: &[String]) {
    if args.is_empty() {
        eprintln!("usage: bob build [-j N] [--repo-root <path>] <target>...");
        std::process::exit(1);
    }

    let mut jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut repo_root: Option<PathBuf> = None;
    let mut targets: Vec<String> = Vec::new();
    let mut dump_keys = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-j" => {
                i += 1;
                jobs = args[i].parse().expect("invalid job count");
            }
            "--repo-root" => {
                i += 1;
                repo_root = Some(PathBuf::from(&args[i]));
            }
            // Print `<effective-cache-key> <crateName> <drv-path>` for every
            // crate in the graph and exit. Used by the bench harness to seed
            // workspace crates whose build scripts bob can't replay.
            "--dump-keys" => dump_keys = true,
            other => targets.push(other.to_string()),
        }
        i += 1;
    }

    let cache = ArtifactCache::new();
    let repo_root = repo_root
        .or_else(|| find_repo_root().ok())
        .expect("could not find repo root — pass --repo-root, set BOB_REPO_ROOT, or add a bob.nix");

    // Resolve all targets
    let mut resolve_results: Vec<resolve::ResolveResult> = Vec::new();
    for target in &targets {
        match resolve_target(target, &repo_root, &cache) {
            Ok(r) => resolve_results.push(r),
            Err(e) => {
                eprintln!("error resolving '{target}': {e}");
                std::process::exit(1);
            }
        }
    }

    let drv_paths: Vec<String> = resolve_results.iter().map(|r| r.drv_path.clone()).collect();
    let g = graph::BuildGraph::from_roots_cached(&drv_paths, cache.root()).expect("building graph");

    // Realize any missing source tarballs / build inputs
    g.realize_inputs().expect("realizing inputs");

    // Build per-crate source overrides with cascading invalidation.
    // See compute_workspace_overrides() for the algorithm.
    let overrides = compute_workspace_overrides(&repo_root, &g);

    if dump_keys {
        for (drv, node) in &g.nodes {
            let key = match overrides.get(drv) {
                Some(ov) => ArtifactCache::cache_key_with_source(drv, &ov.source_hash),
                None => ArtifactCache::cache_key(drv),
            };
            let name = node
                .drv
                .env
                .get("crateName")
                .map(String::as_str)
                .unwrap_or("?");
            println!("{key} {name} {drv}");
        }
        return;
    }

    // Find bash and stdenv from the first crate drv
    let first_drv = g.nodes.values().next().expect("empty graph");
    let bash = first_drv.drv.builder.clone();
    let stdenv_path = first_drv
        .drv
        .env
        .get("stdenv")
        .expect("drv missing stdenv")
        .clone();

    eprintln!(
        "\x1b[1m  Compiling\x1b[0m {} crates ({} jobs)",
        g.crate_count(),
        jobs
    );

    let result = scheduler::run_parallel(
        &g,
        &cache,
        jobs,
        &bash,
        &stdenv_path,
        &overrides,
        &drv_paths,
    );

    // Show output binaries for root crates
    for r in &resolve_results {
        let artifact = match overrides.get(&r.drv_path) {
            Some(ov) => cache.artifact_dir_by_key(&ArtifactCache::cache_key_with_source(
                &r.drv_path,
                &ov.source_hash,
            )),
            None => cache.artifact_dir(&r.drv_path),
        };
        let out_bin = artifact.join("out").join("bin");
        let mut bins = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&out_bin) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    bins.push(entry.path());
                }
            }
        }
        if !bins.is_empty() {
            for bin in &bins {
                eprintln!("   \x1b[1;32mOutput\x1b[0m {}", bin.display());
            }

            if resolve_results.len() == 1 {
                let out_dir = artifact.join("out");
                let link = std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("result");
                let _ = std::fs::remove_file(&link);
                let _ = std::os::unix::fs::symlink(&out_dir, &link);
            }
        }
    }

    if result.failed > 0 {
        std::process::exit(1);
    }
}

/// Compute SourceOverrides for every workspace crate in the graph, with
/// cascading invalidation through the dependency DAG.
///
/// The eval cache reuses a drv keyed only on `Cargo.lock`, so the drv's
/// baked-in `src` store paths are stale once any workspace source changes.
/// We fix this by:
///
///  1. Hashing each workspace crate's source dir (mtime fast-path, ~0.1ms each).
///  2. Computing an *effective hash* per crate in topo order:
///     `eff(c) = blake3( own_src_hash(c) ‖ sorted(eff(d) for d in crate_deps(c)) )`.
///     Only crates that are workspace members, or depend (transitively) on one,
///     get an entry. Crates.io deps are leaves w.r.t. workspace crates, so they
///     stay on the plain `blake3(drv_path)` key and remain seedable from the
///     nix store.
///  3. Emitting a SourceOverride for every crate with an effective hash,
///     pointing `src` at the live worktree dir. The scheduler then uses
///     `cache_key_with_source(drv, eff)` for these crates: editing one
///     workspace crate's source produces a new key for it *and* every
///     downstream workspace crate, while everything else stays cached.
fn compute_workspace_overrides(
    repo_root: &Path,
    g: &graph::BuildGraph,
) -> std::collections::HashMap<String, executor::SourceOverride> {
    use std::collections::HashMap;

    // crateName (== Cargo.toml [package].name) → drv_path. If the same name
    // appears in both the workspace and crates.io (it shouldn't), prefer the
    // 0.0.0 version which is how cargo-nix-plugin tags local crates.
    let is_local = |drv: &str| {
        g.nodes[drv]
            .drv
            .env
            .get("version")
            .map(|v| v == "0.0.0")
            .unwrap_or(false)
    };
    let mut name_to_drv: HashMap<String, String> = HashMap::new();
    for (drv, node) in &g.nodes {
        let Some(name) = node.drv.env.get("crateName") else {
            continue;
        };
        let local = is_local(drv);
        match name_to_drv.get(name) {
            Some(_) if !local => {}
            Some(prev) if is_local(prev) => {
                // Resolver v1 unifies workspace features so this shouldn't
                // happen today. If crate2nix ever emits two feature-variant
                // drvs for one workspace member, the loser would silently
                // miss its own_hash and serve stale source on edits.
                eprintln!(
                    "  warn: workspace crate '{name}' has multiple drvs ({prev} and {drv}); \
                     source-change tracking will only cover one"
                );
                name_to_drv.insert(name.clone(), drv.clone());
            }
            _ => {
                name_to_drv.insert(name.clone(), drv.clone());
            }
        }
    }

    // Per-workspace-crate own source hash + live src dir.
    let mut own_hash: HashMap<String, String> = HashMap::new();
    let mut src_dir: HashMap<String, PathBuf> = HashMap::new();
    if let Ok(members) = resolve::EvalCache::workspace_members(repo_root) {
        for (name, rel) in &members {
            if let Some(drv) = name_to_drv.get(name) {
                match resolve::EvalCache::source_hash(repo_root, rel) {
                    Ok(h) => {
                        own_hash.insert(drv.clone(), h);
                        src_dir.insert(drv.clone(), repo_root.join(rel));
                    }
                    Err(e) => eprintln!("  warn: hashing {}: {e}", rel.display()),
                }
            }
        }
    }

    // Effective hash in topo order (deps before dependents).
    let mut eff: HashMap<String, String> = HashMap::new();
    for drv in &g.topo_order {
        let node = &g.nodes[drv];
        let own = own_hash.get(drv);
        let mut dep_effs: Vec<&str> = node
            .crate_deps
            .iter()
            .filter_map(|d| eff.get(d).map(String::as_str))
            .collect();
        if own.is_none() && dep_effs.is_empty() {
            // Pure crates.io subgraph — stable, plain key.
            continue;
        }
        dep_effs.sort_unstable();
        let mut h = blake3::Hasher::new();
        if let Some(o) = own {
            h.update(o.as_bytes());
        }
        for d in dep_effs {
            h.update(b"\0");
            h.update(d.as_bytes());
        }
        eff.insert(drv.clone(), h.finalize().to_hex()[..32].to_string());
    }

    let count = eff.len();
    let overrides: HashMap<String, executor::SourceOverride> = eff
        .into_iter()
        .map(|(drv, hash)| {
            let ov = executor::SourceOverride {
                src_path: src_dir.get(&drv).cloned(),
                source_hash: hash,
            };
            (drv, ov)
        })
        .collect();

    eprintln!(
        "  \x1b[2mTracking {} workspace crate(s) for source changes\x1b[0m",
        count
    );
    overrides
}

fn cmd_clean(args: &[String]) {
    let cache = ArtifactCache::new();

    if args.is_empty() || args[0] == "--help" {
        eprintln!("usage: bob clean [--all | --incremental | <member-name>]");
        eprintln!();
        eprintln!("  --all           Remove all artifacts + incremental cache");
        eprintln!("  --incremental   Remove only incremental compilation cache");
        eprintln!("  <name>          Remove artifacts for a specific member (requires eval cache)");
        std::process::exit(1);
    }

    if args[0] == "--all" {
        let root = cache.root();
        for subdir in &["artifacts", "incremental", "tmp", "rmeta", "build"] {
            let path = root.join(subdir);
            if path.exists() {
                let size = dir_size(&path);
                std::fs::remove_dir_all(&path).expect("removing cache dir");
                eprintln!("removed: {} ({})", path.display(), format_size(size));
            }
        }
        eprintln!("done");
        return;
    }

    if args[0] == "--incremental" {
        let path = cache.root().join("incremental");
        if path.exists() {
            let size = dir_size(&path);
            std::fs::remove_dir_all(&path).expect("removing incremental cache");
            eprintln!("removed: {} ({})", path.display(), format_size(size));
        } else {
            eprintln!("no incremental cache");
        }
        return;
    }

    // Clean a specific member — need to find its drv path
    let member = &args[0];
    let repo_root = find_repo_root().expect("could not find repo root");
    let eval_cache = resolve::EvalCache::new(cache.root());

    match eval_cache.resolve_one(&repo_root, member) {
        Ok(r) => {
            let artifact = cache.artifact_dir(&r.drv_path);
            let inc = cache.incremental_dir(&r.drv_path);
            let mut cleaned = false;

            if artifact.exists() {
                let size = dir_size(&artifact);
                std::fs::remove_dir_all(&artifact).expect("removing artifact");
                eprintln!(
                    "removed artifact: {} ({})",
                    artifact.display(),
                    format_size(size)
                );
                cleaned = true;
            }
            if inc.exists() {
                let size = dir_size(&inc);
                std::fs::remove_dir_all(&inc).expect("removing incremental");
                eprintln!(
                    "removed incremental: {} ({})",
                    inc.display(),
                    format_size(size)
                );
                cleaned = true;
            }
            if !cleaned {
                eprintln!("nothing cached for '{member}'");
            }
        }
        Err(e) => {
            eprintln!("error resolving '{member}': {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_status() {
    let cache = ArtifactCache::new();
    let root = cache.root();

    eprintln!("cache: {}", root.display());
    eprintln!();

    for (label, subdir) in &[
        ("artifacts", "artifacts"),
        ("incremental", "incremental"),
        ("eval cache", "eval"),
        ("tmp (stale)", "tmp"),
    ] {
        let path = root.join(subdir);
        if path.exists() {
            let (count, size) = dir_stats(&path);
            eprintln!("  {label:14} {count:5} entries   {}", format_size(size));
        } else {
            eprintln!("  {label:14}     0 entries   0 B");
        }
    }

    let total = dir_size(root);
    eprintln!();
    eprintln!("  total: {}", format_size(total));
}

fn cmd_parse_drv(args: &[String]) {
    let path = args.first().expect("missing drv path");
    let contents = std::fs::read(path).expect("failed to read drv file");
    match drv::Derivation::parse(&contents) {
        Ok(d) => {
            println!("outputs:");
            for (name, out) in &d.outputs {
                println!("  {name} = {}", out.path);
            }
            println!("input_derivations: {}", d.input_derivations.len());
            println!("input_sources: {}", d.input_sources.len());
            println!("platform: {}", d.platform);
            println!("builder: {}", d.builder);
            println!("args: {:?}", d.args);
            println!("env ({} vars):", d.env.len());
            for (k, v) in &d.env {
                let display = if v.len() > 120 {
                    format!("{}...", &v[..120])
                } else {
                    v.clone()
                };
                println!("  {k} = {display}");
            }
        }
        Err(e) => {
            eprintln!("parse error: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_graph(args: &[String]) {
    if args.is_empty() {
        eprintln!("usage: bob graph <drv-path>...");
        std::process::exit(1);
    }

    let roots: Vec<String> = args.to_vec();
    match graph::BuildGraph::from_roots(&roots) {
        Ok(g) => {
            println!("crates in graph: {}", g.crate_count());
            println!("topological order:");
            for (i, drv_path) in g.topo_order.iter().enumerate() {
                let node = &g.nodes[drv_path];
                let name = node
                    .drv
                    .env
                    .get("crateName")
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                let version = node
                    .drv
                    .env
                    .get("crateVersion")
                    .unwrap_or(&String::new())
                    .clone();
                let ndeps = node.crate_deps.len();
                println!("  {i:3}. {name}-{version} ({ndeps} deps)");
            }
        }
        Err(e) => {
            eprintln!("graph error: {e}");
            std::process::exit(1);
        }
    }
}

/// Count entries (immediate children) and total size of a directory.
fn dir_stats(path: &Path) -> (usize, u64) {
    let count = std::fs::read_dir(path)
        .map(|entries| entries.count())
        .unwrap_or(0);
    (count, dir_size(path))
}

/// Recursively compute directory size.
fn dir_size(path: &Path) -> u64 {
    walkdir(path)
}

fn walkdir(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let ft = entry.file_type().unwrap_or_else(|_| {
                // fallback: treat as file
                std::fs::metadata(entry.path()).unwrap().file_type()
            });
            if ft.is_dir() {
                total += walkdir(&entry.path());
            } else {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.1} KB", bytes as f64 / (1u64 << 10) as f64)
    } else {
        format!("{bytes} B")
    }
}
