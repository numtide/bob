//! Store path rewriting for building outside the Nix sandbox.
//!
//! Three categories of store paths in a .drv:
//! 1. Output paths ($out, $lib) → rewrite to cache dir
//! 2. Dependency output paths → rewrite to cached dep outputs
//! 3. Toolchain paths (rustc, gcc, stdenv) → keep as-is

use std::collections::BTreeMap;

/// A map of original store paths → replacement paths.
/// Applied to all env vars before executing the build.
pub struct PathRewriter {
    rewrites: Vec<(String, String)>,
}

impl PathRewriter {
    pub fn new() -> Self {
        Self {
            rewrites: Vec::new(),
        }
    }

    /// Register a path substitution.
    pub fn add(&mut self, from: String, to: String) {
        self.rewrites.push((from, to));
    }

    /// Apply all substitutions to a string.
    /// Store paths have unique 32-char hashes so false positives are negligible.
    pub fn rewrite(&self, input: &str) -> String {
        let mut result = input.to_string();
        for (from, to) in &self.rewrites {
            result = result.replace(from.as_str(), to.as_str());
        }
        result
    }

    /// Apply rewrites to all env vars.
    pub fn rewrite_env(&self, env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        env.iter()
            .map(|(k, v)| (k.clone(), self.rewrite(v)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_store_paths() {
        let mut rw = PathRewriter::new();
        rw.add(
            "/nix/store/aaaa-serde-1.0.0-lib".into(),
            "/home/user/.cache/nib/artifacts/serde-1.0.0".into(),
        );
        rw.add(
            "/nix/store/bbbb-hello-0.1.0".into(),
            "/home/user/.cache/nib/out/hello-0.1.0".into(),
        );

        let input = "-L /nix/store/aaaa-serde-1.0.0-lib/lib";
        assert_eq!(
            rw.rewrite(input),
            "-L /home/user/.cache/nib/artifacts/serde-1.0.0/lib"
        );

        // Unmapped store paths pass through untouched
        let toolchain = "/nix/store/cccc-gcc-15.2.0/bin/cc";
        assert_eq!(rw.rewrite(toolchain), toolchain);
    }

    #[test]
    fn rewrite_env_map() {
        let mut rw = PathRewriter::new();
        rw.add("/nix/store/xxxx-out".into(), "/tmp/cache/out".into());

        let mut env = BTreeMap::new();
        env.insert("out".into(), "/nix/store/xxxx-out".into());
        env.insert("installPhase".into(), "cp -r target/lib $out/lib".into());

        // $out in installPhase is a shell variable, not the literal path.
        // But the literal path appears in `out` env var.
        let rewritten = rw.rewrite_env(&env);
        assert_eq!(rewritten["out"], "/tmp/cache/out");
        // installPhase references $out (shell var), not the literal — unchanged
        assert_eq!(rewritten["installPhase"], "cp -r target/lib $out/lib");
    }
}
