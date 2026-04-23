//! Plugin catalog and Lua file management handlers.
//!
//! ## Plugin catalog
//!
//! The plugin catalog is built at startup by scanning `config/plugins/` for
//! `.plugin.toml` manifests.  It can be refreshed at runtime via
//! `POST /plugins/reload` (admin tier).
//!
//! - `GET /plugins` — list all installed plugins.
//! - `GET /plugins/{name}` — details for a single plugin.
//!
//! ## Lua file management
//!
//! These endpoints (automation tier — requires `automation` or `admin` role)
//! let an automation client read and write `.lua` files in the scenes,
//! automations, and scripts directories without needing filesystem access.
//!
//! - `GET /files` — list all `.lua` files across all three directories.
//! - `GET /files/{type}/{filename.lua}` — read a single file.
//! - `PUT /files/{type}/{filename.lua}` — write (create or replace) a file.
//!
//! Path traversal and non-`.lua` extensions are rejected with 400 Bad Request.

use std::path::PathBuf;

use crate::dto::{ApiError, FileEntry, PluginSummary, PutFileRequest};
use crate::helpers::read_lock;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

/// Returns a summary of every plugin in the in-memory catalog.
pub async fn list_plugins(State(state): State<AppState>) -> Json<Vec<PluginSummary>> {
    let catalog = read_lock(&state.plugin_catalog);
    Json(catalog.iter().map(PluginSummary::from).collect())
}

/// Returns the details of a single plugin by name, or 404 if it is not in
/// the catalog.
pub async fn get_plugin(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<PluginSummary>, ApiError> {
    let catalog = read_lock(&state.plugin_catalog);
    catalog
        .iter()
        .find(|m| m.plugin.name == name)
        .map(PluginSummary::from)
        .map(Json)
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("plugin '{name}' not found"),
        })
}

/// Validate `path` and resolve it to a concrete filesystem `PathBuf`.
///
/// `path` must be in the form `{type}/{filename}` where:
/// - `{type}` is one of `scenes`, `automations`, `scripts`
/// - `{filename}` has a `.lua` extension
/// - No component is `..`
fn resolve_lua_path(state: &AppState, path: &str) -> Result<(PathBuf, &'static str), ApiError> {
    // Guard against path traversal at the raw string level.
    for component in path.split('/') {
        if component == ".." || component == "." {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "path traversal is not allowed",
            ));
        }
    }

    let (type_prefix, rest) = path.split_once('/').ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "path must be in the form {type}/{filename} (e.g. scenes/my-scene.lua)",
        )
    })?;

    if rest.is_empty() || rest.contains('/') {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "path must be a single filename inside a type directory",
        ));
    }

    if !rest.ends_with(".lua") {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "only .lua files are allowed",
        ));
    }

    let (base_dir, file_type): (&str, &'static str) = match type_prefix {
        "scenes" => (&state.scenes_directory, "scenes"),
        "automations" => (&state.automations_directory, "automations"),
        "scripts" => (&state.scripts_directory, "scripts"),
        other => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown file type '{other}'; must be scenes, automations, or scripts"),
            ));
        }
    };

    Ok((PathBuf::from(base_dir).join(rest), file_type))
}

/// Collect all `.lua` files from a directory as `FileEntry` values.
fn collect_lua_entries(dir: &str, file_type: &'static str) -> Vec<FileEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut result: Vec<FileEntry> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.path().is_file()
                && entry.path().extension().and_then(|ext| ext.to_str()) == Some("lua")
        })
        .filter_map(|entry| {
            let filename = entry.file_name().to_string_lossy().to_string();
            Some(FileEntry {
                path: format!("{file_type}/{filename}"),
                file_type,
            })
        })
        .collect();
    result.sort_by(|a, b| a.path.cmp(&b.path));
    result
}

/// Returns all `.lua` files from the scenes, automations, and scripts
/// directories, sorted alphabetically within each type.
pub async fn list_files(State(state): State<AppState>) -> Json<Vec<FileEntry>> {
    let mut entries = Vec::new();
    if !state.scenes_directory.is_empty() {
        entries.extend(collect_lua_entries(&state.scenes_directory, "scenes"));
    }
    if !state.automations_directory.is_empty() {
        entries.extend(collect_lua_entries(
            &state.automations_directory,
            "automations",
        ));
    }
    if !state.scripts_directory.is_empty() {
        entries.extend(collect_lua_entries(&state.scripts_directory, "scripts"));
    }
    Json(entries)
}

/// Returns the raw text content of a Lua file.
/// Returns 404 if the file does not exist.
pub async fn get_file(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let (full_path, _file_type) = resolve_lua_path(&state, &path)?;

    let content = tokio::fs::read_to_string(&full_path).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found(format!("file '{path}' not found"))
        } else {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read file '{path}': {err}"),
            )
        }
    })?;

    Ok(content)
}

/// Writes (creates or replaces) a Lua file with the provided content.
///
/// The parent directory is created automatically if it does not exist.
/// Returns 204 No Content on success.
pub async fn put_file(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Json(body): Json<PutFileRequest>,
) -> Result<StatusCode, ApiError> {
    let (full_path, _file_type) = resolve_lua_path(&state, &path)?;

    // Ensure the parent directory exists.
    if let Some(parent) = full_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to create directory for '{path}': {err}"),
            )
        })?;
    }

    tokio::fs::write(&full_path, body.content.as_bytes())
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to write file '{path}': {err}"),
            )
        })?;

    Ok(StatusCode::NO_CONTENT)
}
