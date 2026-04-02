# nix-inc

Fast incremental Rust builds for the monorepo. Replays `buildRustCrate` derivations outside the Nix sandbox with a content-addressed artifact cache and rustc incremental compilation.

## Quick start

```bash
# Build a service by name
nix-inc build my-crate

# Or cd into a crate and build from there
cd path/to/my-crate
nix-inc build .

# The binary is at ./result/bin/<name>
./result/bin/my-crate
```

## How it works

1. **Resolve**: translates a workspace member name to a Nix `.drv` path via `nix-instantiate` (~5s, cached on subsequent runs)
2. **Graph**: parses `.drv` files to build the full crate dependency DAG (170 crates for hello-rs, takes ~20ms)
3. **Build**: replays each crate's configure → build → install phases in parallel using persistent bash workers with stdenv pre-sourced
4. **Cache**: stores build artifacts keyed by `blake3(drv_path)` — the drv path already encodes all inputs (source, deps, flags) via Nix's own hashing, so cache invalidation is automatic and sound

On repeat builds, only changed crates rebuild. Unchanged crates are served from cache in ~0.1ms each. Rustc's `-C incremental` flag further speeds up within-crate recompilation.

## Commands

### `nix-inc build [options] <target>...`

Build one or more workspace members.

**Targets:**
- `<name>` — workspace member name (e.g., `my-crate`)
- `.` — auto-detect from nearest `Cargo.toml`
- `/nix/store/....drv` — raw derivation path

**Options:**
- `-j N` — parallel build jobs (default: number of CPUs)
- `--repo-root <path>` — monorepo root (default: auto-detected by walking up to find `bob.nix`)

After a successful build, prints the output binary path and creates a `./result` symlink (like `nix-build`).

### `nix-inc clean [target]`

Remove cached build artifacts.

- `--all` — remove everything (artifacts + incremental cache)
- `--incremental` — remove only the rustc incremental compilation cache
- `<name>` — remove artifacts for a specific workspace member

### `nix-inc status`

Show cache statistics: entry counts and disk usage per category.

```
cache: /root/.cache/nix-inc

  artifacts        170 entries   1.1 GB
  incremental      170 entries   1.7 GB
  eval cache         0 entries   0 B
  tmp (stale)        0 entries   0 B

  total: 2.8 GB
```

### `nix-inc graph <drv-path>...`

Print the crate dependency graph in topological order. Useful for debugging.

### `nix-inc parse-drv <drv-path>`

Parse and dump a `.drv` file's contents (outputs, env vars, deps). Useful for debugging.

## Performance

Benchmarked with [hyperfine](https://github.com/sharkdp/hyperfine) on `example` (201 crates / 971 bazel actions), 64-core machine:


| Scenario | nix-inc | bazel | Relative |
|----------|---------|-------|----------|
| Clean build (cold cache) | **42s** | 76s | **nix-inc 1.8× faster** |
| No-op rebuild (cached) | **15ms** | 265ms | **nix-inc 17× faster** |
| Incremental (1 crate changed) | **2.1s** | 2.6s | **nix-inc 1.2× faster** |

nix-inc wins all three scenarios. For incremental rebuilds, when deps haven't changed (Cargo.lock stable), nix-inc reuses the previous drv and overrides `src` with a local snapshot, skipping the 2s `nix-instantiate` eval entirely. The remaining 2.1s is pure rustc compilation. Bazel's sandbox overhead accounts for the 0.5s difference in the incremental case.


Run the benchmark yourself:

```bash
cd .
cargo build --release
./bench.sh
```

## Cache location

All state lives under `$XDG_CACHE_HOME/nix-inc/` (default `~/.cache/nix-inc/`):

- `artifacts/` — build outputs (rlibs, binaries), one directory per crate
- `incremental/` — rustc incremental compilation state, persists across rebuilds
- `eval/` — cached workspace member → drv path mappings
- `tmp/` — in-progress builds (cleaned up on completion)

Cache invalidation is automatic: when a crate's source, dependencies, or build flags change, its Nix drv path changes, which changes the cache key. No manual invalidation needed for correctness.

## Requirements

- The patched Nix with `builtins.resolveCargoWorkspace` (for name → drv resolution)
- Must be run from within the monorepo (or pass `--repo-root`)

## Limitations

- Outputs are not registered in the Nix store — `dockerTools.buildImage` and other Nix consumers can't use them directly. For container images, use regular `nix-build`.
- No file watcher yet — you need to re-run `nix-inc build` after changes.
- No test support (`buildTests = true`) — only builds libs and binaries.
