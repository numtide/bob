# Bob the Builder

Fast incremental builds on top of Nix. Replays fine-grained `buildRustCrate` derivations outside the Nix sandbox with a content-addressed artifact cache, persistent stdenv workers, and rustc incremental compilation.

**Status: experimental.** Currently targets Rust workspaces built via [cargo-nix-plugin] / `buildRustCrate`, and C/C++ projects built with cmake or meson. The core (drv parser, scheduler, cache, path rewriter) is language-agnostic; other backends (Go via go2nix) are planned.

[cargo-nix-plugin]: https://github.com/Mic92/cargo-nix-plugin

## How it works

1. **Resolve** — translate a workspace member name to a `.drv` path via `nix-instantiate` (cached on `Cargo.lock` hash)
2. **Graph** — parse `.drv` files directly (ATerm) to build the crate dependency DAG
3. **Build** — replay each crate's configure/build/install phases in parallel, in persistent bash workers with `$stdenv/setup` pre-sourced; non-crate inputs (toolchain, C libs, fetchers) are realised once via `nix-store --realise`
4. **Cache** — registry/untracked units key on `blake3(drv_path)` (Nix has already hashed all their inputs); workspace units key on `blake3(own_src ‖ dep_output_hashes)` so a rebuild that produces an identical artifact doesn't move dependents' keys (see [Early cutoff](#early-cutoff))
5. **Pipeline** — a `rustc` wrapper emits `metadata,link`, signals `__META_READY__` on fd 3 once the fat `.rmeta` exists, and the scheduler unblocks dependents before codegen finishes (cargo-style pipelining)

On repeat builds only changed crates rebuild; `-C incremental` makes each rebuild fast, and early cutoff stops the rebuild from cascading past the point where outputs actually differ.

## Early cutoff

Cargo's freshness check is *input-mtime*: edit a deep crate → its `.rmeta` mtime bumps → every reverse-dep's check fails → rustc runs on each → their mtimes bump → all transitive revdeps rebuild. `-C incremental` makes each call cheap, but you still pay one rustc spawn per revdep, plus the leaf relinks.

bob's tracked-unit cache key is *output-addressed*: `eff(c) = blake3(own_src(c) ‖ prop(d) for tracked d ∈ deps(c))`, where `prop(d)` is the hash of `d`'s **built output**, not its inputs. The scheduler computes `eff(c)` at the moment `c` becomes ready (once each `prop(d)` is known), and if `artifacts/<eff(c)>/` exists `c` is skipped entirely.

For an edit at the bottom of a 20-deep revdep chain:

1. The edited crate rebuilds.
2. Its rmeta is hashed. If the public interface didn't change (comment, private body, formatting), the rmeta is byte-identical → every lib dependent's `eff` key is unchanged → all 19 intermediate crates cache-hit without spawning rustc.
3. The leaf cdylib/bin re-links (its key folds in the edited crate's *rlib* bytes, which did change).

If the edit *does* change the interface, the cascade runs until rmeta stabilises — typically one or two layers, not the full reachable set.

### Two-tier propagation

`prop(d)` is per-edge:

- **lib→lib** uses `early_hash(d)` = `blake3(rmeta)`, taken at `__META_READY__`. rmeta is rustc's interface artifact and is byte-stable for unchanged inputs even under `-C incremental`, so cutoff fires for non-interface edits *and* the edge stays early-gated (pipelining preserved).
- **→link** (cdylib/staticlib/bin/proc-macro) uses `out_hash(d)` = `blake3(full output)`, taken at commit. rlibs are *not* byte-stable across `-C incremental` session states, so keying the link on rmeta would be unsound — a stale `.so` could be served against a changed rlib. These edges are done-gated.

cc units have no early signal yet, so cc→anything is done-gated on `out_hash`.

### Trade-offs

- **Hash on the critical path.** Each built unit's rmeta and full output are blake3'd before dependents can compute their key. ~3 GB/s; tens of ms on fat rlibs.
- **Relies on rmeta determinism.** rustc gives no stability guarantee for `.rmeta`. Today it's byte-stable for equal inputs; if a future rustc embeds a nonce, lib→lib cutoff stops firing. The result is *slow*, not *wrong* (dependents rebuild and `-C incremental` does the work).
- **Link targets always rebuild if any transitive rlib did.** rlibs aren't reproducible under `-C incremental`, so every leaf bin/cdylib re-links whenever anything upstream rebuilt. One fat `.so` is fine; many leaf binaries pay this per leaf.
- **Precise invalidation = precise input model.** Cargo's blanket rebuild masks build scripts that read untracked state. `eff(c)` covers own sources, dep outputs, and the drv env (which already hashes declared `buildInputs`/flags); it does **not** cover ambient env a `build.rs` reads via `cargo:rerun-if-env-changed` — see [When to invalidate](#when-to-invalidate-manually).
- **No sandbox, no remote.** Replay runs in your worktree with your env; out-hashes aren't portable across machines, and outputs aren't store-registered. This is a dev-loop accelerator; `nix build` stays the source of truth.

## Setup

bob needs two things from the target repo:

1. **Per-crate derivations** — a Cargo workspace wired through cargo-nix-plugin's `buildRustCrate`, so each crate is its own `.drv`.
2. **A `bob.nix` at the repo root** with one top-level attr per backend. The Rust backend reads `rust.workspaceMembers.<name>.build`:

   ```nix
   # bob.nix
   { pkgs ? import <nixpkgs> {} }:
   let
     cargoNix = pkgs.callPackage ./Cargo.nix {};  # or however your repo wires cargo-nix-plugin
   in {
     rust = { inherit (cargoNix) workspaceMembers; };
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

- `artifacts/<key>/{out,lib,.out-hash,.early-hash}` — committed outputs plus the propagated hashes dependents key on. `<key>` is `blake3(drv_path)` for untracked units, `eff(c)` for tracked ones (so a tracked unit accumulates one entry per distinct source state it's been built at).
- `incremental/<blake3(drv_path)>/` — rustc `-C incremental` session / cc build dir. Drv-path-keyed so source edits reuse it; toolchain/flag changes (which move the drv path) cold-start it.
- `tmp/<blake3(drv_path)>/` — in-flight `$out`. Drv-path-keyed (not eff-keyed) so `$out` is stable across source edits — cmake/pkg-config/rpaths embed it, and `-C incremental`'s session inputs include it.
- `eval/` — `nix-instantiate` results, keyed on `bob.nix` + lockfile + `eval-inputs`.
- `rmeta/`, `build/` — in-flight pipelining state.

### When to invalidate manually

In normal use, never: source edits change `own_src` → new `eff` key; dep edits change `prop(d)` → new `eff` key; toolchain/flag/override changes change the drv path → new key for both tracked and untracked units *and* a fresh `incremental/` dir.

The cases that need a manual `bob clean`:

- **`build.rs` reads ambient state.** `cargo:rerun-if-env-changed=FOO` where `FOO` comes from your shell, not the drv env. Change `FOO` → bob serves the old artifact. `bob clean <crate>` (drops its incremental dir; next build re-runs `build.rs`) or set `FOO` via a crate override so it lands in the drv env and keys correctly.
- **Non-hermetic cc unit.** A `CMakeLists.txt` that does `find_package` against a system path, or reads an env var the drv doesn't set. Same remedy.
- **`-C incremental` corruption.** Rare rustc bug where the session state produces bad codegen after certain edits; symptoms are link errors or wrong behaviour that `nix build` doesn't reproduce. `bob clean <crate>` or `bob clean --incremental`.
- **Disk pressure.** `artifacts/` grows by one entry per (tracked unit × distinct source state). `bob clean --all`.

What the subcommands actually remove:

| | `artifacts/` | `incremental/` | `eval/` |
|---|:---:|:---:|:---:|
| `bob clean <member>` | only the drv-keyed entry¹ | that member's | — |
| `bob clean --incremental` | — | all | — |
| `bob clean --all` | all | all | — |

¹ Tracked units' eff-keyed `artifacts/` entries aren't individually addressable (there's one per source-hash, and the name→key mapping needs the source). They're harmless to keep; use `--all` to reclaim disk. The `eval/` cache self-invalidates on `bob.nix`/lockfile/`eval-inputs` changes; `rm -rf ~/.cache/bob/eval` if you need to force a re-instantiate without touching those.

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
├── cc/     bob-cc    — C/C++ backend: cmake/meson stdenv drvs marked via
│                     lib/cc.nix, persistent out-of-tree build dir for
│                     ninja-level per-TU incrementality (no pipelining yet)
└── cli/    bob       — the binary; registers backends and wires the CLI
```

## Adding a backend

Implement `bob_core::Backend` in a new `crates/<lang>/` crate and append it
to `BACKENDS` in `crates/cli/src/main.rs`. The minimum is:

- `is_unit(drv)` — e.g. `drv.env.contains_key("goPackagePath")`
- `unit_name(drv)` — progress display
- `resolve_attr(target, root)` — attr path under `(import bob.nix {}).<id()>`
- `lock_hash(root)` — e.g. `blake3(go.sum)`
- `build_script_hooks(ctx)` — e.g. `export GOCACHE=…`
- `output_populated(tmp, drv)`

`pipeline()` and `dispatch_internal()` default to no-ops; backends without
an early-artifact analogue (Go) get correct done-gated scheduling for free.
A `core-leakage` flake check enforces that `bob-core` stays free of
backend-specific identifiers.

## C/C++ backend

A cc unit is a plain `stdenv.mkDerivation` (cmake or meson, out-of-tree)
declared in `bob.nix`:

```nix
# bob.nix
let bobCc = import "${bob}/lib/cc.nix"; in
{
  rust = { inherit (cargoNix) workspaceMembers; };
  cc = bobCc.units {
    libfoo = { drv = pkgs.libfoo; src = "path/to/libfoo"; };
  };
}
```

`bobCc.unit` attaches `bobCcSrc` as a Nix-level attribute (`drv // { … }`),
so `drvPath` is **unchanged** — if `pkgs.libfoo` also appears in some Rust
crate's `buildInputs`, bob's graph walk from a Rust root finds the same drv
as a unit and a C edit cascades through to the `.so`. The cc backend
evaluates `(import bob.nix {}).cc` once to get the drvPath→src map; nothing
is written into the drv env.

`bob build libfoo` keeps a drv-path-keyed build directory under
`~/.cache/bob/incremental/` so reconfigure is warm and `ninja` rebuilds only
the TUs whose `.d` depfiles changed. The drv still `nix build`s normally —
`dontUnpack`/`cmakeBuildDir` are injected only at replay time.

Caveats: unpack/patch are skipped (the build runs against the live worktree),
so patched derivations are not supported; cc edges are done-gated (no early
signal yet — see `crates/cc/src/lib.rs` for what's needed).

## Limitations

- Outputs are not registered in the Nix store — downstream Nix consumers can't use them. Use `nix-build` for that.
- No file watcher; re-run `bob build` after edits.
- No `buildTests = true` support yet.

## License

MIT
