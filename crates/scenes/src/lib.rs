//! Scene management for HomeCmdr.
//!
//! A *scene* is a short Lua script that sends one or more commands to devices.
//! This crate handles loading scene files from disk, enforcing per-scene
//! execution modes (parallel, single, queued, restart), and running them
//! against a live `Runtime`.

pub mod catalog;
pub mod loader;
pub mod runner;
pub mod types;

#[cfg(test)]
mod tests;

pub use catalog::SceneCatalog;
pub use runner::SceneRunner;
pub use types::{ReloadError, Scene, SceneExecutionResult, SceneRunOutcome, SceneSummary};
