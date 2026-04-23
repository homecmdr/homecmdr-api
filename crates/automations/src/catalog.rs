use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{bail, Context, Result};
use homecmdr_core::model::AttributeValue;
use homecmdr_core::runtime::Runtime;
use homecmdr_lua_host::{evaluate_module, parse_execution_mode, LuaRuntimeOptions, DEFAULT_MAX_INSTRUCTIONS};
use mlua::{Function, Lua};

use crate::concurrency::ConcurrencyMap;
use crate::conditions::{first_failed_condition, parse_conditions};
use crate::runner::execute_automation;
use crate::types::AutomationExecutionResult;
use crate::triggers::{parse_trigger, trigger_type_name};
use crate::types::{
    Automation, AutomationSummary, ReloadError, RuntimeStatePolicy, TriggerContext,
};

// ── AutomationControlState ────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct AutomationControlState {
    enabled: HashMap<String, bool>,
}

// ── AutomationCatalog ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct AutomationCatalog {
    pub(crate) automations: Vec<Automation>,
    pub(crate) scripts_root: Option<PathBuf>,
    control: Arc<RwLock<AutomationControlState>>,
    pub(crate) concurrency: ConcurrencyMap,
}

impl AutomationCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn load_from_directory(
        path: impl AsRef<Path>,
        scripts_root: Option<PathBuf>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let entries = fs::read_dir(path)
            .with_context(|| format!("failed to read automations directory {}", path.display()))?;
        let mut automations = Vec::new();
        let mut ids = HashMap::new();

        for entry in entries {
            let entry = entry.context("failed to read automations directory entry")?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            if !file_type.is_file() {
                continue;
            }

            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("lua") {
                continue;
            }

            let automation = load_automation_file(&entry.path(), scripts_root.as_deref())?;
            if ids
                .insert(automation.summary.id.clone(), automation.path.clone())
                .is_some()
            {
                bail!("duplicate automation id '{}'", automation.summary.id);
            }
            automations.push(automation);
        }

        automations.sort_by(|a, b| a.summary.id.cmp(&b.summary.id));
        Ok(Self {
            automations,
            scripts_root,
            control: Arc::new(RwLock::new(AutomationControlState::default())),
            concurrency: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    pub fn reload_from_directory(
        path: impl AsRef<Path>,
        scripts_root: Option<PathBuf>,
    ) -> std::result::Result<Self, Vec<ReloadError>> {
        let path = path.as_ref();
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                return Err(vec![ReloadError {
                    file: path.display().to_string(),
                    message: format!("failed to read automations directory: {error}"),
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
                        message: format!("failed to read automations directory entry: {error}"),
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

        let mut automations = Vec::new();
        let mut ids = HashMap::<String, PathBuf>::new();

        for file in files {
            match load_automation_file(&file, scripts_root.as_deref()) {
                Ok(automation) => {
                    let id = automation.summary.id.clone();
                    if let Some(existing_path) = ids.insert(id.clone(), file.clone()) {
                        errors.push(ReloadError {
                            file: file.display().to_string(),
                            message: format!(
                                "duplicate automation id '{id}' (already defined in {})",
                                existing_path.display()
                            ),
                        });
                        continue;
                    }
                    automations.push(automation);
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

        automations.sort_by(|a, b| a.summary.id.cmp(&b.summary.id));

        Ok(Self {
            automations,
            scripts_root,
            control: Arc::new(RwLock::new(AutomationControlState::default())),
            concurrency: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    pub fn summaries(&self) -> Vec<AutomationSummary> {
        self.automations
            .iter()
            .map(|automation| automation.summary.clone())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<AutomationSummary> {
        self.automations
            .iter()
            .find(|automation| automation.summary.id == id)
            .map(|automation| automation.summary.clone())
    }

    pub fn is_enabled(&self, id: &str) -> Option<bool> {
        self.automations
            .iter()
            .find(|automation| automation.summary.id == id)
            .map(|_| self.read_control().enabled.get(id).copied().unwrap_or(true))
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        if self
            .automations
            .iter()
            .all(|automation| automation.summary.id != id)
        {
            bail!("automation '{id}' not found");
        }

        self.write_control().enabled.insert(id.to_string(), enabled);
        Ok(enabled)
    }

    pub fn validate(&self, id: &str) -> Result<AutomationSummary> {
        let automation = self
            .automations
            .iter()
            .find(|automation| automation.summary.id == id)
            .with_context(|| format!("automation '{id}' not found"))?;
        let reloaded = load_automation_file(&automation.path, self.scripts_root.as_deref())?;
        Ok(reloaded.summary)
    }

    pub fn execute(
        &self,
        id: &str,
        runtime: Arc<Runtime>,
        trigger_payload: AttributeValue,
        trigger_context: TriggerContext,
    ) -> Result<AutomationExecutionResult> {
        let automation = self
            .automations
            .iter()
            .find(|automation| automation.summary.id == id)
            .with_context(|| format!("automation '{id}' not found"))?;

        if let Some(reason) = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(first_failed_condition(
                automation,
                runtime.as_ref(),
                &trigger_payload,
                chrono::Utc::now(),
                trigger_context,
            ))
        }) {
            return Ok(AutomationExecutionResult {
                status: "skipped".to_string(),
                error: Some(reason),
                results: Vec::new(),
                duration_ms: 0,
            });
        }

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let record = execute_automation(
            automation,
            runtime,
            trigger_payload,
            self.scripts_root.as_deref(),
            cancel,
            DEFAULT_MAX_INSTRUCTIONS,
        )?;

        Ok(AutomationExecutionResult {
            status: record.status,
            error: record.error,
            results: record.results,
            duration_ms: record.duration_ms,
        })
    }

    fn read_control(&self) -> std::sync::RwLockReadGuard<'_, AutomationControlState> {
        match self.control.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write_control(&self) -> std::sync::RwLockWriteGuard<'_, AutomationControlState> {
        match self.control.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

// ── load_automation_file ──────────────────────────────────────────────────────

pub(crate) fn load_automation_file(path: &Path, scripts_root: Option<&Path>) -> Result<Automation> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read automation file {}", path.display()))?;
    let lua = Lua::new();
    let opts = LuaRuntimeOptions {
        scripts_root: scripts_root.map(Path::to_path_buf),
        ..Default::default()
    };
    let module = evaluate_automation_module(&lua, &source, path, &opts)?;

    let id = module.get::<String>("id").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} is missing string field 'id': {error}",
            path.display()
        )
    })?;
    let name = module.get::<String>("name").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} is missing string field 'name': {error}",
            path.display()
        )
    })?;
    let trigger_value = module.get::<mlua::Value>("trigger").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} is missing field 'trigger': {error}",
            path.display()
        )
    })?;
    let trigger = parse_trigger(trigger_value, path)?;
    let conditions = parse_conditions(&module, path)?;

    if id.trim().is_empty() {
        bail!("automation file {} has empty id", path.display());
    }
    if name.trim().is_empty() {
        bail!("automation file {} has empty name", path.display());
    }

    let _: Function = module.get("execute").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} is missing function field 'execute': {error}",
            path.display()
        )
    })?;

    let description = module
        .get::<Option<String>>("description")
        .map_err(|error| {
            anyhow::anyhow!(
                "automation file {} has invalid optional field 'description': {error}",
                path.display()
            )
        })?;
    let runtime_state_policy = parse_runtime_state_policy(&module, path)?;

    let mode_value = module.get::<mlua::Value>("mode").map_err(|error| {
        anyhow::anyhow!(
            "automation file {} has invalid optional field 'mode': {error}",
            path.display()
        )
    })?;
    let mode = parse_execution_mode(mode_value, 8).map_err(|error| {
        anyhow::anyhow!(
            "automation file {} has invalid field 'mode': {error}",
            path.display()
        )
    })?;

    Ok(Automation {
        summary: AutomationSummary {
            id,
            name,
            description,
            trigger_type: trigger_type_name(&trigger),
            condition_count: conditions.len(),
        },
        mode,
        path: path.to_path_buf(),
        trigger,
        conditions,
        runtime_state_policy,
    })
}

// ── evaluate_automation_module ────────────────────────────────────────────────

pub(crate) fn evaluate_automation_module(
    lua: &Lua,
    source: &str,
    path: &Path,
    opts: &LuaRuntimeOptions,
) -> Result<mlua::Table> {
    evaluate_module(lua, source, path.to_string_lossy().as_ref(), opts).map_err(|error| {
        anyhow::anyhow!(
            "failed to evaluate automation file {}: {error}",
            path.display()
        )
    })
}

// ── parse_runtime_state_policy ────────────────────────────────────────────────

fn parse_runtime_state_policy(module: &mlua::Table, path: &Path) -> Result<RuntimeStatePolicy> {
    let state = module
        .get::<Option<mlua::Table>>("state")
        .map_err(|error| {
            anyhow::anyhow!(
                "automation file {} has invalid optional field 'state': {error}",
                path.display()
            )
        })?;
    let Some(state) = state else {
        return Ok(RuntimeStatePolicy::default());
    };

    Ok(RuntimeStatePolicy {
        cooldown_secs: state.get::<Option<u64>>("cooldown_secs").map_err(|error| {
            anyhow::anyhow!(
                "automation file {} has invalid optional state field 'cooldown_secs': {error}",
                path.display()
            )
        })?,
        dedupe_window_secs: state
            .get::<Option<u64>>("dedupe_window_secs")
            .map_err(|error| {
                anyhow::anyhow!(
                "automation file {} has invalid optional state field 'dedupe_window_secs': {error}",
                path.display()
            )
            })?,
        resumable_schedule: state
            .get::<Option<bool>>("resumable_schedule")
            .map_err(|error| {
                anyhow::anyhow!(
                "automation file {} has invalid optional state field 'resumable_schedule': {error}",
                path.display()
            )
            })?
            .unwrap_or(false),
    })
}
