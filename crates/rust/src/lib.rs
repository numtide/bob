//! Rust language backend: `buildRustCrate` drvs via cargo-nix-plugin.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use bob_core::{Backend, BuildContext, BuildGraph, Derivation, OwnHash, PipelinePolicy};

mod hooks;
mod pipeline;
mod rustc_wrap;
mod workspace;

pub struct RustBackend;

static PIPELINE: pipeline::RustPipeline = pipeline::RustPipeline;

impl Backend for RustBackend {
    fn id(&self) -> &'static str {
        "rust"
    }

    fn is_unit(&self, drv: &Derivation) -> bool {
        drv.env.contains_key("crateName")
    }

    fn unit_name<'a>(&self, drv: &'a Derivation) -> Cow<'a, str> {
        drv.env
            .get("crateName")
            .map(String::as_str)
            .unwrap_or("?")
            .into()
    }

    fn resolve_attr(&self, target: &str, repo_root: &Path) -> Option<String> {
        let members = workspace::workspace_members(repo_root).ok()?;
        members
            .contains_key(target)
            .then(|| format!("workspaceMembers.{target}.build"))
    }

    fn lock_hash(&self, repo_root: &Path) -> Result<String, String> {
        workspace::lock_hash(repo_root)
    }

    fn detect_from_cwd(&self) -> Option<String> {
        workspace::detect_from_cwd()
    }

    fn workspace_unit_hashes(
        &self,
        repo_root: &Path,
        graph: &BuildGraph,
    ) -> HashMap<String, OwnHash> {
        workspace::unit_hashes(repo_root, graph)
    }

    fn build_script_hooks(&self, ctx: &BuildContext<'_>) -> Result<String, String> {
        hooks::build_script_hooks(ctx)
    }

    fn output_populated(&self, tmp: &Path, drv: &Derivation) -> bool {
        hooks::output_populated(tmp, drv)
    }

    fn pipeline(&self) -> Option<&dyn PipelinePolicy> {
        Some(&PIPELINE)
    }

    fn dispatch_internal(&self, cmd: &str, args: &[String]) {
        if cmd == "__rustc-wrap" {
            rustc_wrap::main(args);
        }
    }
}
