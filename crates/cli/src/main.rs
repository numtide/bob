use std::path::Path;
use std::path::PathBuf;

use bob_core::{drv, graph, overrides, resolve, scheduler, ArtifactCache, Backend};

/// Registered language backends, tried in order for `resolve_attr` /
/// `detect_from_cwd` / `dispatch_internal`. `is_unit` and
/// `workspace_unit_hashes` are unioned across all of them.
///
/// `scheduler::run_parallel` still takes a single backend (the one that
/// resolved the root target); per-node backend dispatch in mixed-language
/// graphs is a follow-up once a second backend exists to test against.
static BACKENDS: &[&(dyn Backend + Sync)] = &[&bob_rust::RustBackend];

fn backend() -> &'static (dyn Backend + Sync) {
    BACKENDS[0]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        // Internal wrapper-shim re-entries (`bob __<backend>-wrap …`).
        // Dispatched first — hot path, invoked once per compiler call.
        cmd if cmd.starts_with("__") => {
            for b in BACKENDS {
                if b.dispatch_internal(cmd, &args[2..]) {
                    unreachable!("dispatch_internal must exit when it returns true");
                }
            }
            eprintln!("unknown internal command: {cmd}");
            std::process::exit(1);
        }
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
    eprintln!("Bob the Builder — fast incremental builds via Nix drv replay + caching");
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

    let name = if target == "." {
        BACKENDS
            .iter()
            .find_map(|b| b.detect_from_cwd())
            .ok_or("could not detect target from current directory")?
    } else {
        target.to_string()
    };

    let (b, attr) = BACKENDS
        .iter()
        .find_map(|b| b.resolve_attr(&name, repo_root).map(|a| (*b, a)))
        .ok_or_else(|| format!("unknown target '{name}'"))?;
    let lock_hash = b.lock_hash(repo_root)?;

    let eval_cache = resolve::EvalCache::new(cache.root());
    eval_cache.resolve_one(repo_root, &name, &attr, &lock_hash)
}

fn is_unit(d: &bob_core::Derivation) -> bool {
    BACKENDS.iter().any(|b| b.is_unit(d))
}

/// Stable identifier for the `is_unit` predicate, mixed into the graph-cache
/// key so adding/removing a backend invalidates cached graphs.
fn predicate_key() -> String {
    BACKENDS
        .iter()
        .map(|b| b.id())
        .collect::<Vec<_>>()
        .join(",")
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
    let g =
        graph::BuildGraph::from_roots_cached(&drv_paths, cache.root(), &predicate_key(), is_unit)
            .expect("building graph");

    // Realize any missing source tarballs / build inputs
    g.realize_inputs().expect("realizing inputs");

    // Per-unit source overrides with cascading invalidation; see
    // overrides::cascade for the algorithm. Each backend supplies own-source
    // hashes for the workspace units it recognises.
    let mut own = std::collections::HashMap::new();
    for b in BACKENDS {
        own.extend(b.workspace_unit_hashes(&repo_root, &g));
    }
    let overrides = overrides::cascade(&g, own);
    eprintln!(
        "  \x1b[2mTracking {} workspace unit(s) for source changes\x1b[0m",
        overrides.len()
    );

    if dump_keys {
        for (drv, node) in &g.nodes {
            let key = match overrides.get(drv) {
                Some(ov) => ArtifactCache::cache_key_with_source(drv, &ov.source_hash),
                None => ArtifactCache::cache_key(drv),
            };
            println!("{key} {} {drv}", backend().unit_name(&node.drv));
        }
        return;
    }

    eprintln!(
        "\x1b[1m  Compiling\x1b[0m {} units ({} jobs)",
        g.unit_count(),
        jobs
    );

    let result = scheduler::run_parallel(&g, &cache, jobs, backend(), &overrides, &drv_paths);

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

    match resolve_target(member, &repo_root, &cache) {
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
    match graph::BuildGraph::from_roots(&roots, is_unit) {
        Ok(g) => {
            println!("units in graph: {}", g.unit_count());
            println!("topological order:");
            for (i, drv_path) in g.topo_order.iter().enumerate() {
                let node = &g.nodes[drv_path];
                let name = backend().unit_name(&node.drv);
                let ndeps = node.unit_deps.len();
                println!("  {i:3}. {name} ({ndeps} deps)");
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
