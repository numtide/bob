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

## Commands

```bash
bob build <name>             # build a workspace member
bob build .                  # auto-detect from nearest Cargo.toml
bob build /nix/store/….drv   # raw drv path (skips resolve)
bob clean [--all|<name>]     # drop cached artifacts
bob status                   # cache stats
bob graph <drv>              # print dependency DAG
```

Options: `-j N` (jobs, default nproc), `--repo-root <path>` (default: walk up to `bob.nix`, or `$BOB_REPO_ROOT`).

## Cache

All state lives under `$XDG_CACHE_HOME/bob/`:

- `artifacts/<key>/{out,lib}` — build outputs
- `incremental/<key>/` — rustc `-C incremental` state, persists across rebuilds
- `eval/` — cached member → drv mappings
- `tmp/`, `rmeta/`, `build/` — in-flight state

## Limitations

- Outputs are not registered in the Nix store — downstream Nix consumers can't use them. Use `nix-build` for that.
- No file watcher; re-run `bob build` after edits.
- No `buildTests = true` support yet.

## License

MIT
