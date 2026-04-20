//! cc-specific `builder.sh` injection: persistent out-of-tree build dir,
//! live-source `cmakeDir`/meson cwd, and warm-reconfigure handling.
//!
//! Runs after `source $stdenv/setup`, so the cmake/meson setup-hooks have
//! already registered themselves as `configurePhase` and `$CC`/`$CXX` are the
//! cc-wrapper store paths (stable across runs — important, since cmake treats
//! a compiler-path change as a full-rebuild trigger).

use std::fmt::Write;
use std::path::Path;

use bob_core::{BuildContext, Derivation};

pub fn build_script_hooks(ctx: &BuildContext<'_>) -> Result<String, String> {
    let mut s = String::new();

    // Persistent build dir, drv-path-keyed (NOT effective-key-keyed): source
    // edits keep the same dir so ninja's depfile graph survives, but any
    // change to flags/compiler/buildInputs moves drv_path → fresh dir → clean
    // reconfigure. Same lifecycle as Rust's `-C incremental` dir.
    let inc = ctx.cache.incremental_dir(ctx.drv_path);
    std::fs::create_dir_all(&inc).map_err(|e| format!("creating cc build dir: {e}"))?;
    let inc_s = inc.display();

    // Build directly against the live worktree. `$src` has already been
    // overridden to the `OwnHash::src_dir` by core (executor.rs / attrs.rs),
    // so cmake/meson record a stable absolute SOURCE_DIR and ninja's stored
    // header paths keep resolving across runs. unpack/patch would copy into
    // the (wiped-per-run) NIX_BUILD_TOP and break that stability.
    //
    // Note `$src` for a unit without an override is the original store path —
    // also stable, just never changes, so the persistent dir is a one-shot
    // cache. That's fine: only `bobCcSrc`-marked units reach this hook, and
    // those are exactly the ones with a live override.
    s.push_str("dontUnpack=1\n");
    s.push_str("dontPatch=1\n");

    // cmake hook reads these as overridable defaults (`: ${cmakeBuildDir:=…}`).
    // Absolute build dir → `cmakeDir` can't stay `..`, so point it at $src.
    // The hook then `mkdir -p && cd $cmakeBuildDir && cmake $cmakeDir …`.
    writeln!(s, "export cmakeBuildDir='{inc_s}'").unwrap();
    s.push_str("export cmakeDir=\"$src\"\n");

    // The cmake hook prepends `-DCMAKE_INSTALL_PREFIX`/`_BINDIR`/… each run
    // from `$out`/`$dev`/…, which DO move per effective-key. cmake handles a
    // changed install prefix without recompiling (only install rules touch
    // it). `-DCMAKE_C_COMPILER=$CC` is the dangerous one, but `$CC` is the
    // cc-wrapper store path and that's drv-path-stable.

    // meson hook runs `meson setup $mesonBuildDir …` from cwd = source. With
    // dontUnpack genericBuild never cds, so do it here. Re-setup on a warm
    // dir needs `--reconfigure` (otherwise "Directory already configured").
    // `mesonFlags` may be a bash array under structuredAttrs; appending via
    // `+=(…)` works for both array and unset-scalar.
    writeln!(s, "export mesonBuildDir='{inc_s}'").unwrap();
    writeln!(
        s,
        r#"preConfigureHooks+=(_bobCcPreConfigure)
_bobCcPreConfigure() {{
  cd "$src"
  if [[ -e '{inc_s}/meson-private' ]]; then
    mesonFlags+=(--reconfigure)
  fi
}}"#
    )
    .unwrap();

    // ninja install wants the build dir writable (it stamps `.ninja_log`); a
    // previous run may have left it owned by a different effective uid via
    // the worker's homeless-shelter dance. Belt-and-braces.
    writeln!(s, "chmod -R u+w '{inc_s}' 2>/dev/null || true").unwrap();

    Ok(s)
}

/// installPhase produced something usable? Any populated declared output's
/// `lib/` or `bin/` counts. cc units don't always have a `lib` *output* (vs
/// a `lib/` subdir of `$out`), so check every declared output.
pub fn output_populated(tmp: &Path, drv: &Derivation) -> bool {
    drv.outputs.keys().any(|o| {
        let base = tmp.join(o);
        dir_nonempty(&base.join("lib")) || dir_nonempty(&base.join("bin"))
    })
}

fn dir_nonempty(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bob_core::{ArtifactCache, Derivation};
    use std::collections::BTreeMap;

    fn fake_drv() -> Derivation {
        Derivation {
            outputs: {
                let mut m = BTreeMap::new();
                m.insert(
                    "out".into(),
                    bob_core::drv::Output {
                        path: "/nix/store/x-foo".into(),
                        hash_algo: String::new(),
                        hash: String::new(),
                    },
                );
                m
            },
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            platform: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            args: vec![],
            env: {
                let mut m = BTreeMap::new();
                m.insert("pname".into(), "libfoo".into());
                m
            },
        }
    }

    /// The injected shell fragment must be syntactically valid bash *and*
    /// must not leak the effective-key tmp path into compiler-facing
    /// variables (cmake treats CMAKE_C_COMPILER changes as full rebuilds,
    /// so anything keyed on the effective key would cold-start every edit).
    #[test]
    fn hooks_are_valid_bash_and_drv_keyed() {
        let cache_root = std::env::temp_dir().join(format!("bob-cc-hooks-{}", std::process::id()));
        let cache = ArtifactCache::from_path(cache_root.clone());
        let tmp = cache_root.join("tmp").join("effkey");
        std::fs::create_dir_all(&tmp).unwrap();
        let drv = fake_drv();
        let ctx = BuildContext {
            drv_path: "/nix/store/aaaa-libfoo.drv",
            drv: &drv,
            tmp: &tmp,
            cache: &cache,
            is_root: true,
            self_exe: Path::new("/bin/false"),
        };
        let s = build_script_hooks(&ctx).unwrap();

        // bash -n: parse without executing.
        let st = std::process::Command::new("bash")
            .args(["-n", "-c", &s])
            .status()
            .expect("running bash -n");
        assert!(st.success(), "hook output is not valid bash:\n{s}");

        // Build dir is the drv-keyed incremental dir, never the effective-
        // key tmp dir; nothing in the fragment should reference tmp/effkey.
        let inc = cache.incremental_dir("/nix/store/aaaa-libfoo.drv");
        assert!(s.contains(&inc.display().to_string()));
        assert!(
            !s.contains("effkey"),
            "hook leaked effective-key path into shell:\n{s}"
        );
        assert!(s.contains("dontUnpack=1"));
        assert!(s.contains("cmakeDir=\"$src\""));

        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn output_populated_checks_all_outputs() {
        let d = std::env::temp_dir().join(format!("bob-cc-out-{}", std::process::id()));
        let mut drv = fake_drv();
        drv.outputs.insert(
            "dev".into(),
            bob_core::drv::Output {
                path: "/nix/store/x-foo-dev".into(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );
        // Empty outputs → not populated.
        std::fs::create_dir_all(d.join("out")).unwrap();
        std::fs::create_dir_all(d.join("dev")).unwrap();
        assert!(!output_populated(&d, &drv));
        // A lib in `out` (not in a separate `lib` output) counts.
        std::fs::create_dir_all(d.join("out/lib")).unwrap();
        std::fs::write(d.join("out/lib/libfoo.so"), b"").unwrap();
        assert!(output_populated(&d, &drv));
        let _ = std::fs::remove_dir_all(&d);
    }
}
