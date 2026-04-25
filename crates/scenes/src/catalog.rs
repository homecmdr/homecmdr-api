//! The scene catalog: loads and indexes every `.lua` scene file in a directory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use homecmdr_core::runtime::Runtime;
use homecmdr_lua_host::DEFAULT_MAX_INSTRUCTIONS;

use crate::loader::{execute_scene_inline, load_scene_file};
use crate::types::{ReloadError, Scene, SceneExecutionResult, SceneSummary};

/// An in-memory index of all scenes loaded from a directory.
///
/// Created once at startup (or on hot-reload) and then cheaply cloned
/// into `SceneRunner` via `Arc`.
#[derive(Debug, Clone, Default)]
pub struct SceneCatalog {
    pub(crate) scenes: HashMap<String, Scene>,
    /// Optional root directory for shared Lua `require()` modules.
    pub(crate) scripts_root: Option<PathBuf>,
}

impl SceneCatalog {
    /// Returns an empty catalog (no scenes loaded).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Loads all `.lua` files from `path` and returns a populated catalog.
    /// Fails fast on the first error (parse, missing fields, duplicate id, …).
    pub fn load_from_directory(
        path: impl AsRef<std::path::Path>,
        scripts_root: Option<PathBuf>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let entries = std::fs::read_dir(path)
            .with_context(|| format!("failed to read scenes directory {}", path.display()))?;
        let mut scenes = HashMap::new();

        for entry in entries {
            let entry = entry.context("failed to read scenes directory entry")?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            if !file_type.is_file() {
                continue;
            }

            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("lua") {
                continue;
            }

            let scene = load_scene_file(&entry.path(), scripts_root.as_deref())?;
            let scene_id = scene.summary.id.clone();
            if scenes.insert(scene_id.clone(), scene).is_some() {
                bail!("duplicate scene id '{scene_id}'");
            }
        }

        Ok(Self {
            scenes,
            scripts_root,
        })
    }

    /// Like `load_from_directory` but designed for hot-reload: collects *all*
    /// errors instead of stopping at the first one, so the caller can report
    /// every broken file at once.  Returns `Err(errors)` if anything failed.
    pub fn reload_from_directory(
        path: impl AsRef<std::path::Path>,
        scripts_root: Option<PathBuf>,
    ) -> std::result::Result<Self, Vec<ReloadError>> {
        let path = path.as_ref();
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                return Err(vec![ReloadError {
                    file: path.display().to_string(),
                    message: format!("failed to read scenes directory: {error}"),
                }]);
            }
        };

        let mut files = Vec::new();
        let mut errors = Vec::new();

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    errors.push(ReloadError {
                        file: path.display().to_string(),
                        message: format!("failed to read scenes directory entry: {error}"),
                    });
                    continue;
                }
            };

            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    errors.push(ReloadError {
                        file: entry.path().display().to_string(),
                        message: format!("failed to inspect file type: {error}"),
                    });
                    continue;
                }
            };

            if !file_type.is_file() {
                continue;
            }

            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("lua") {
                continue;
            }

            files.push(entry.path());
        }

        files.sort();

        let mut scenes = HashMap::new();
        let mut ids = HashMap::<String, std::path::PathBuf>::new();

        for file in files {
            match load_scene_file(&file, scripts_root.as_deref()) {
                Ok(scene) => {
                    let scene_id = scene.summary.id.clone();
                    if let Some(existing_path) = ids.insert(scene_id.clone(), file.clone()) {
                        errors.push(ReloadError {
                            file: file.display().to_string(),
                            message: format!(
                                "duplicate scene id '{scene_id}' (already defined in {})",
                                existing_path.display()
                            ),
                        });
                        continue;
                    }
                    scenes.insert(scene_id, scene);
                }
                Err(error) => {
                    errors.push(ReloadError {
                        file: file.display().to_string(),
                        message: error.to_string(),
                    });
                }
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(Self {
            scenes,
            scripts_root,
        })
    }

    /// Returns a sorted list of scene summaries — used by the HTTP list endpoint.
    pub fn summaries(&self) -> Vec<SceneSummary> {
        let mut scenes = self
            .scenes
            .values()
            .map(|scene| scene.summary.clone())
            .collect::<Vec<_>>();
        scenes.sort_by(|a, b| a.id.cmp(&b.id));
        scenes
    }

    /// Execute a scene by id directly (no mode enforcement).
    /// Returns `None` if the scene does not exist.
    pub fn execute(
        &self,
        id: &str,
        runtime: Arc<Runtime>,
    ) -> Result<Option<Vec<SceneExecutionResult>>> {
        let Some(scene) = self.scenes.get(id) else {
            return Ok(None);
        };

        let results = execute_scene_inline(
            scene,
            runtime,
            self.scripts_root.as_deref(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            DEFAULT_MAX_INSTRUCTIONS,
        )?;
        Ok(Some(results))
    }
}
