//! bob-core: language-agnostic Nix `.drv` replay engine.
//!
//! Parses `.drv` files, builds the unit DAG, replays `genericBuild` outside
//! the sandbox in persistent workers with a content-addressed artifact cache,
//! and (optionally) pipelines builds via a generic mid-build fd-3 signal.
//!
//! Language-specific behaviour comes through the [`Backend`] trait.

pub mod attrs;
pub mod backend;
pub mod cache;
pub mod drv;
pub mod executor;
pub mod graph;
pub mod overrides;
pub mod progress;
pub mod resolve;
pub mod rewrite;
pub mod scheduler;
pub mod worker;

pub use backend::{Backend, BuildContext, PipelinePolicy};
pub use cache::ArtifactCache;
pub use drv::Derivation;
pub use executor::SourceOverride;
pub use graph::{BuildGraph, UnitNode};
pub use overrides::{tracked_set, OwnHash};
