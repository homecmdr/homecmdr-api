//! Plain data types shared across the scenes crate.

use std::path::PathBuf;

use homecmdr_lua_host::ExecutionMode;
use serde::Serialize;

/// Describes a file that failed to load during a catalog reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReloadError {
    /// Path (or directory) of the file that caused the error.
    pub file: String,
    /// Human-readable description of what went wrong.
    pub message: String,
}

/// Lightweight metadata about a scene — safe to send over the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneSummary {
    /// Unique scene identifier as declared in the Lua file.
    pub id: String,
    /// Display name shown in the UI.
    pub name: String,
    /// Optional longer description of what the scene does.
    pub description: Option<String>,
}

/// The outcome of a single device command issued by a scene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneExecutionResult {
    /// ID of the device that was targeted.
    pub target: String,
    /// `"ok"` or `"error"`.
    pub status: &'static str,
    /// Optional detail message (present on error or adapter feedback).
    pub message: Option<String>,
}

/// Full in-memory representation of a loaded scene.
#[derive(Debug, Clone)]
pub struct Scene {
    /// Metadata extracted from the Lua module.
    pub summary: SceneSummary,
    /// Controls how concurrent invocations of this scene are handled.
    pub mode: ExecutionMode,
    /// Absolute path to the `.lua` source file.
    pub path: PathBuf,
}

/// The outcome of a `SceneRunner::execute` call.
#[derive(Debug)]
pub enum SceneRunOutcome {
    /// Scene completed; results are attached.
    Completed(Vec<SceneExecutionResult>),
    /// No scene with the given id was found.
    NotFound,
    /// Scene is already running and its mode does not allow additional executions.
    Dropped,
    /// Scene was accepted for asynchronous execution (queued mode, background run).
    Queued,
}
