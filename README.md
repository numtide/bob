# Bob the Builder

Fast incremental builds on top of Nix. Replays fine-grained `buildRustCrate` derivations outside the Nix sandbox with a content-addressed artifact cache, persistent stdenv workers, and rustc incremental compilation.

**Status: experimental.** Currently targets Rust workspaces built via [cargo-nix-plugin] / `buildRustCrate`. The core (drv parser, scheduler, cache, path rewriter) is language-agnostic; other backends (Go via go2nix) are planned.

[cargo-nix-plugin]: https://github.com/Mic92/cargo-nix-plugin

## How it works

1. **Resolve** — translate a workspace member name to a `.drv` path via `nix-instantiate` (cached on `Cargo.lock` hash)
2. **Graph** — parse `.drv` files directly (ATerm) to build the crate dependency DAG
3. **Build** — replay each crate's configure/build/install phases in parallel, in persistent bash workers with `$stdenv/setup` pre-sourced; non-crate inputs (toolchain, C libs, fetchers) are realised once via `nix-store --realise`
4. **Cache** — artifacts keyed by `blake3(drv_path)`; the drv path already encodes all inputs via Nix's own hashing, so invalidation is automatic and sound
5. **Pipeline** — a `rustc` wrapper emits `metadata,link`, signals `__META_READY__` on fd 3 once the fat `.rmeta` exists, and the scheduler unblocks dependents before codegen finishes (cargo-style pipelining)

On repeat builds only changed crates rebuild; unchanged crates are served from cache in ~0.1ms each. `-C incremental` further speeds up within-crate recompilation.

## Setup

bob needs two things from the target repo:

1. **Per-crate derivations** — a Cargo workspace wired through cargo-nix-plugin's `buildRustCrate`, so each crate is its own `.drv`.
2. **A `bob.nix` at the repo root** that exposes `workspaceMembers.<name>.build`:

   ```nix
   # bob.nix
   { pkgs ? import <nixpkgs> {} }:
   let
     cargoNix = pkgs.callPackage ./Cargo.nix {};  # or however your repo wires cargo-nix-plugin
   in {
     inherit (cargoNix) workspaceMembers;
   }
   ```

If your `bob.nix` needs `builtins.resolveCargoWorkspace`, point bob at a patched `nix-instantiate`:

```bash
export BOB_NIX_INSTANTIATE=/path/to/patched/nix-instantiate
```

### Eval-cache invalidation

bob caches the `nix-instantiate` result so the ~1–2s eval is paid once, not per build. The cache key always covers `bob.nix` and `Cargo.lock`. If `bob.nix` imports other files (crate overrides, `flake.lock` for pins), declare them so edits invalidate the cache — either in `Cargo.toml`:

```toml
[workspace.metadata.bob]
eval-inputs = ["flake.lock", "nix/overrides/*.nix"]
```

or, if you can't put bob config into the upstream manifest, in a `bob.toml` next to `bob.nix`:

```toml
eval-inputs = ["flake.lock", "nix/overrides/*.nix"]
```

Both lists are additive. Globs use `*`/`?`/`[…]`/`**`; `*` matches within a single path component (so `nix/*.nix` matches `nix/a.nix` but not `nix/sub/b.nix`), `**` recurses (`nix/**/*.nix` matches both).

## Commands

```bash
bob build <name>             # build a workspace member
bob build .                  # auto-detect from nearest Cargo.toml
bob build /nix/store/….drv   # raw drv path (skips resolve)
bob clean [--all|<name>]     # drop cached artifacts
bob status                   # cache stats
bob graph <drv>              # print dependency DAG
```

Options: `-j N` (jobs, default nproc), `--repo-root <path>` (default: walk up to `bob.nix`, or `$BOB_REPO_ROOT`), `-o/--out-link <path>` (result symlink prefix, default `result`), `--no-out-link`, `--print-out-paths` (artifact paths on stdout).

Result symlinks follow nix-build: `result` → `$out`, `result-lib` → `$lib`; for multiple targets the second and onward get `-2`, `-3`, … suffixes.

## Cache

All state lives under `$XDG_CACHE_HOME/bob/`:

- `artifacts/<key>/{out,lib}` — build outputs
- `incremental/<key>/` — rustc `-C incremental` state, persists across rebuilds
- `eval/` — cached member → drv mappings
- `tmp/`, `rmeta/`, `build/` — in-flight state

## Crate layout

```
crates/
├── core/   bob-core  — language-agnostic .drv replay engine: ATerm parser,
│                     unit DAG, content-addressed cache, path rewriter,
│                     persistent stdenv workers, .attrs.{json,sh} emission,
│                     two-tier (early-signal/done) scheduler, Backend trait
├── rust/   bob-rust  — Rust backend: buildRustCrate/cargo-nix-plugin drvs,
│                     rmeta pipelining via the __rustc-wrap shim,
│                     -C incremental injection, Cargo workspace introspection
└── cli/    bob       — the binary; registers backends and wires the CLI
```

## Adding a backend

Implement `bob_core::Backend` in a new `crates/<lang>/` crate and append it
to `BACKENDS` in `crates/cli/src/main.rs`. The minimum is:

- `is_unit(drv)` — e.g. `drv.env.contains_key("goPackagePath")`
- `unit_name(drv)` — progress display
- `resolve_attr(target, root)` — attr path under `(import bob.nix {})`
- `lock_hash(root)` — e.g. `blake3(go.sum)`
- `build_script_hooks(ctx)` — e.g. `export GOCACHE=…`
- `output_populated(tmp, drv)`

`pipeline()` and `dispatch_internal()` default to no-ops; backends without
an early-artifact analogue (Go) get correct done-gated scheduling for free.
A `core-leakage` flake check enforces that `bob-core` stays free of
backend-specific identifiers.

## Limitations

- Outputs are not registered in the Nix store — downstream Nix consumers can't use them. Use `nix-build` for that.
- No file watcher; re-run `bob build` after edits.
- No `buildTests = true` support yet.

## License

MIT
