use std::path::PathBuf;

use homecmdr_lua_host::ExecutionMode;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReloadError {
    pub file: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SceneExecutionResult {
    pub target: String,
    pub status: &'static str,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Scene {
    pub summary: SceneSummary,
    pub mode: ExecutionMode,
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
