use std::path::Path;
use std::path::PathBuf;

use bob_core::{drv, graph, overrides, resolve, scheduler, ArtifactCache, Backend};
use clap::{Args, Parser, Subcommand};

/// Registered language backends, tried in order for `resolve_attr` /
/// `detect_from_cwd` / `dispatch_internal`. `is_unit` and
/// `workspace_unit_hashes` are unioned across all of them, and the
/// scheduler dispatches `build_script_hooks` / `output_populated` /
/// `pipeline` per-node by re-running `is_unit` to find each unit's owner.
///
/// Order matters for `resolve_attr`: Rust consults `Cargo.toml` (definitive
/// member list) so it goes first; cc's `project()`-name walk is a heuristic
/// and only claims targets Rust declined.
static BACKENDS: &[&dyn Backend] = &[&bob_rust::RustBackend, &bob_cc::CcBackend];

#[derive(Parser)]
#[command(
    name = "bob",
    about = "Bob the Builder — fast incremental builds via Nix drv replay + caching"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build workspace members or .drv paths
    Build(BuildArgs),
    /// Remove cached artifacts
    Clean(CleanArgs),
    /// Show cache statistics
    Status,
    /// Parse a .drv file and print its contents
    ParseDrv {
        /// Path to the .drv file
        path: PathBuf,
    },
    /// Show the unit dependency graph for one or more .drv paths
    Graph {
        /// Root .drv paths
        #[arg(required = true)]
        drv_paths: Vec<String>,
    },
}

#[derive(Args)]
struct BuildArgs {
    /// Workspace member name, `.` (cwd detection), or raw /nix/store/….drv
    #[arg(required = true)]
    targets: Vec<String>,

    /// Parallel jobs (default: nproc)
    #[arg(short = 'j', long, value_name = "N")]
    jobs: Option<usize>,

    /// Repo root containing bob.nix (default: walk up from cwd, or $BOB_REPO_ROOT)
    #[arg(long, value_name = "PATH")]
    repo_root: Option<PathBuf>,

    /// Result symlink prefix (nix-build style: `<prefix>[-N][-<output>]`)
    #[arg(
        short = 'o',
        long,
        value_name = "PATH",
        default_value = "result",
        conflicts_with = "no_out_link"
    )]
    out_link: PathBuf,

    /// Do not create result symlinks
    #[arg(long, visible_alias = "no-link")]
    no_out_link: bool,

    /// Print built artifact paths on stdout (one per output, `out` first)
    #[arg(long)]
    print_out_paths: bool,

    /// Print `<effective-cache-key> <crateName> <drv-path>` for every unit in
    /// the graph and exit. Used by the bench harness to seed workspace crates
    /// whose build scripts bob can't replay.
    #[arg(long, hide = true)]
    dump_keys: bool,
}

#[derive(Args)]
#[command(group = clap::ArgGroup::new("what").required(true))]
struct CleanArgs {
    /// Remove all artifacts + incremental cache
    #[arg(long, group = "what")]
    all: bool,

    /// Remove only the incremental compilation cache
    #[arg(long, group = "what")]
    incremental: bool,

    /// Remove artifacts for a specific workspace member (requires eval cache)
    #[arg(group = "what")]
    member: Option<String>,
}

fn main() {
    // Internal wrapper-shim re-entries (`bob __<backend>-wrap …`). Dispatched
    // before clap — hot path, invoked once per compiler call, and the trailing
    // args are arbitrary compiler argv that clap mustn't try to interpret. A
    // backend that claims `cmd` diverges via process::exit; if we fall through
    // the loop, nobody claimed it.
    let args: Vec<String> = std::env::args().collect();
    if let Some(cmd) = args.get(1).filter(|a| a.starts_with("__")) {
        for b in BACKENDS {
            b.dispatch_internal(cmd, &args[2..]);
        }
        eprintln!("unknown internal command: {cmd}");
        std::process::exit(1);
    }

    match Cli::parse().cmd {
        Cmd::Build(a) => cmd_build(a),
        Cmd::Clean(a) => cmd_clean(a),
        Cmd::Status => cmd_status(),
        Cmd::ParseDrv { path } => cmd_parse_drv(&path),
        Cmd::Graph { drv_paths } => cmd_graph(&drv_paths),
    }
}

/// Find the repo root by walking up from cwd looking for `bob.nix`.
/// `bob.nix` is the per-repo glue that exposes one top-level attr per
/// backend (`rust`, `cc`, …) for nix-instantiate resolution.
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
        .ok_or_else(|| {
            let mut avail: Vec<String> = BACKENDS
                .iter()
                .flat_map(|b| b.list_targets(repo_root))
                .collect();
            avail.sort();
            avail.truncate(10);
            if avail.is_empty() {
                format!("unknown target '{name}'")
            } else {
                format!(
                    "unknown target '{name}'. some available: {}",
                    avail.join(", ")
                )
            }
        })?;
    let lock_hash = b.lock_hash(repo_root)?;

    let eval_cache = resolve::EvalCache::new(cache.root());
    eval_cache.resolve_one(repo_root, &name, &attr, &lock_hash)
}

fn is_unit(repo_root: &Path) -> impl Fn(&str, &bob_core::Derivation) -> bool + '_ {
    move |path, d| BACKENDS.iter().any(|b| b.is_unit(path, d, repo_root))
}

/// Stable identifier for the `is_unit` predicate, mixed into the graph-cache
/// key so adding/removing a backend invalidates cached graphs. Also folds in
/// `bob.nix` content: cc's `is_unit` keys on the drvPath→src map declared
/// there, and adding a cc unit doesn't move any root drv path, so without
/// this a cached graph from before the addition would be served and the new
/// unit would silently stay a boundary input.
fn predicate_key(repo_root: &Path) -> String {
    let mut h = blake3::Hasher::new();
    for b in BACKENDS {
        h.update(b.id().as_bytes());
        h.update(b"\0");
    }
    if let Ok(b) = std::fs::read(repo_root.join("bob.nix")) {
        h.update(&b);
    }
    h.finalize().to_hex()[..16].to_string()
}

fn cmd_build(args: BuildArgs) {
    let BuildArgs {
        targets,
        jobs,
        repo_root,
        out_link,
        no_out_link,
        print_out_paths,
        dump_keys,
    } = args;
    let out_link = (!no_out_link).then_some(out_link);
    let jobs = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });

    let cache = ArtifactCache::new();
    let _lock = cache.lock_exclusive().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });
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
    let g = graph::BuildGraph::from_roots_cached(
        &drv_paths,
        cache.root(),
        &predicate_key(&repo_root),
        is_unit(&repo_root),
    )
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
            let name = BACKENDS
                .iter()
                .find(|b| b.is_unit(drv, &node.drv, &repo_root))
                .map(|b| b.unit_name(&node.drv))
                .unwrap_or("?".into());
            println!("{key} {name} {drv}");
        }
        return;
    }

    eprintln!(
        "\x1b[1m  Compiling\x1b[0m {} units ({} jobs)",
        g.unit_count(),
        jobs
    );

    let result = scheduler::run_parallel(
        &g, &cache, jobs, BACKENDS, &repo_root, &overrides, &drv_paths,
    );

    // Result symlinks + --print-out-paths, one per (root, output) following
    // nix-build's naming: <prefix>[-<n>][-<output>], with `-<n>` omitted for
    // the first root and `-<output>` omitted for `out`. Unlike before, lib-only
    // roots get a link too (`result-lib`), so callers can locate the artifact
    // without a second `--dump-keys` round-trip.
    for (idx, r) in resolve_results.iter().enumerate() {
        let artifact = match overrides.get(&r.drv_path) {
            Some(ov) => cache.artifact_dir_by_key(&ArtifactCache::cache_key_with_source(
                &r.drv_path,
                &ov.source_hash,
            )),
            None => cache.artifact_dir(&r.drv_path),
        };
        if !artifact.exists() {
            // Build failed or aborted before commit; skip silently, the
            // failure summary already reported it.
            continue;
        }

        // Cosmetic: keep listing produced binaries on stderr.
        if let Ok(entries) = std::fs::read_dir(artifact.join("out").join("bin")) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    eprintln!("   \x1b[1;32mOutput\x1b[0m {}", entry.path().display());
                }
            }
        }

        let n_suffix = if idx == 0 {
            String::new()
        } else {
            format!("-{}", idx + 1)
        };
        // `outputs` is a BTreeMap (alphabetical); list `out` first so
        // `--print-out-paths | head -1` yields the primary output.
        let outputs = g.nodes[&r.drv_path].drv.outputs.keys();
        for output in
            std::iter::once("out").chain(outputs.map(String::as_str).filter(|o| *o != "out"))
        {
            let path = artifact.join(output);
            if !path.exists() {
                continue;
            }
            if print_out_paths {
                println!("{}", path.display());
            }
            if let Some(prefix) = &out_link {
                let out_suffix = if output == "out" {
                    String::new()
                } else {
                    format!("-{output}")
                };
                let link = PathBuf::from(format!("{}{n_suffix}{out_suffix}", prefix.display()));
                let _ = std::fs::remove_file(&link);
                if let Err(e) = std::os::unix::fs::symlink(&path, &link) {
                    eprintln!("warning: creating symlink {}: {e}", link.display());
                }
            }
        }
    }

    if result.failed > 0 {
        std::process::exit(1);
    }
}

fn cmd_clean(args: CleanArgs) {
    let cache = ArtifactCache::new();
    let _lock = cache.lock_exclusive().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    if args.all {
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

    if args.incremental {
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

    // Clean a specific member — need to find its drv path. clap's ArgGroup
    // guarantees exactly one of all/incremental/member is set.
    let member = args.member.as_deref().unwrap();
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

fn cmd_parse_drv(path: &Path) {
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

fn cmd_graph(roots: &[String]) {
    // `bob graph` is a debugging aid; if there's no bob.nix above cwd, fall
    // back to an empty repo_root so backends that don't need it (rust) still
    // classify, and cc units simply won't be recognised.
    let repo_root = find_repo_root().unwrap_or_default();
    match graph::BuildGraph::from_roots(roots, is_unit(&repo_root)) {
        Ok(g) => {
            println!("units in graph: {}", g.unit_count());
            println!("topological order:");
            for (i, drv_path) in g.topo_order.iter().enumerate() {
                let node = &g.nodes[drv_path];
                let name = BACKENDS
                    .iter()
                    .find(|b| b.is_unit(drv_path, &node.drv, &repo_root))
                    .map(|b| b.unit_name(&node.drv))
                    .unwrap_or("?".into());
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
