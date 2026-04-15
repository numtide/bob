//! Rust-specific `builder.sh` injection: `-C incremental`, the PATH-shadowing
//! `rustc` wrapper shim, and the `BOB_*` env that configures it.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use bob_core::{BuildContext, Derivation};

use super::pipeline::lib_filename;

/// Shell fragment appended to `builder.sh` after `source $stdenv/setup` and
/// before `genericBuild`. Installs the `__rustc-wrap` shim, exports its
/// config, and points `EXTRA_RUSTC_FLAGS` at the persistent incremental dir.
pub fn build_script_hooks(ctx: &BuildContext<'_>) -> Result<String, String> {
    let mut s = String::new();

    // Precreate $lib/lib so dependents that start on our rmeta (before our
    // installPhase runs) don't ENOENT on configurePhase's symlink walk over
    // `$dep/lib`.
    std::fs::create_dir_all(ctx.tmp.join("lib").join("lib"))
        .map_err(|e| format!("creating lib/lib: {e}"))?;

    // -C incremental: rustc reuses frontend/codegen state across rebuilds of
    // the same drv. Keyed on drv_path (not effective key) so source edits
    // don't cold-start the session.
    let inc_dir = ctx.cache.incremental_dir(ctx.drv_path);
    std::fs::create_dir_all(&inc_dir).map_err(|e| format!("creating incremental dir: {e}"))?;
    s.push_str(&format!(
        "export EXTRA_RUSTC_FLAGS=\"-C incremental={} ${{EXTRA_RUSTC_FLAGS:-}}\"\n",
        inc_dir.display()
    ));

    // Pipelining: wrap rustc to emit a fat rmeta early and signal via fd 3.
    // The wrapper is the bob binary itself (`__rustc-wrap`, see rustc_wrap.rs),
    // so arg classification, JSON-stderr parsing, and rmeta→rlib polling
    // happen in Rust without forking jq/cp/mv per line. This /bin/sh shim just
    // bridges PATH lookup of `rustc` to the subcommand.
    let wrapper_dir = ctx.tmp.join("rustc-wrap");
    std::fs::create_dir_all(&wrapper_dir).map_err(|e| format!("creating wrapper dir: {e}"))?;
    let wrapper = wrapper_dir.join("rustc");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec '{}' __rustc-wrap \"$@\"\n",
            ctx.self_exe.display()
        ),
    )
    .map_err(|e| format!("writing rustc wrapper: {e}"))?;
    let mut perms = std::fs::metadata(&wrapper)
        .map_err(|e| format!("stat rustc wrapper: {e}"))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper, perms).ok();

    // Config for rustc_wrap::main(). BOB_REAL_RUSTC is resolved AFTER
    // `source $stdenv/setup` so PATH already has the toolchain.
    // completeDeps/completeBuildDeps are bash arrays under structuredAttrs
    // (declared `-a` by .attrs.sh) and bash can't export arrays — assigning a
    // scalar to the same name just sets element [0]. Flatten into distinct
    // BOB_* scalars so the rustc-wrap subprocess inherits them.
    //
    // expected_rmeta and skip_link_pass are computed HERE from (drv, is_root),
    // not by the scheduler — they're Rust-only concerns.
    s.push_str(&format!(
        concat!(
            "export BOB_REAL_RUSTC=$(command -v rustc)\n",
            "export BOB_COMPLETE_DEPS=\"${{completeDeps[*]-}}\"\n",
            "export BOB_COMPLETE_BUILD_DEPS=\"${{completeBuildDeps[*]-}}\"\n",
            "export BOB_WRAP_RMETA_DIR='{rmeta_dir}'\n",
            "export BOB_EXPECTED_RMETA='{expected}'\n",
            "export BOB_SKIP_LINK_PASS='{skip_link}'\n",
            "export PATH='{wrap}':$PATH\n",
        ),
        rmeta_dir = ctx.tmp.join("rmeta").display(),
        expected = lib_filename(ctx.drv, "rmeta").unwrap_or_default(),
        skip_link = if ctx.is_root { "" } else { "1" },
        wrap = wrapper_dir.display(),
    ));

    Ok(s)
}

/// Did installPhase produce something usable? lib/lib/*.{rlib,so,a} for lib
/// crates, out/bin/* for bin-only. genericBuild's exit code can't be trusted
/// (errexit vs eval'd phases), so this is the actual success criterion.
pub fn output_populated(tmp: &Path, _drv: &Derivation) -> bool {
    let lib_populated = std::fs::read_dir(tmp.join("lib").join("lib"))
        .map(|d| {
            d.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.ends_with(".rlib") || n.ends_with(".so") || n.ends_with(".a"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let bin_populated = std::fs::read_dir(tmp.join("out").join("bin"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    lib_populated || bin_populated
}
