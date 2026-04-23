use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use mlua::{Lua, Table, Value};

#[derive(Clone)]
pub struct ScriptLoader {
    pub root: PathBuf,
}

impl ScriptLoader {
    pub fn install(&self, lua: &Lua) -> Result<()> {
        let package: Table = lua
            .globals()
            .get("package")
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let loader = self.clone();

        let searcher = lua
            .create_function(move |lua, module_name: String| {
                match loader.load_module(lua, &module_name) {
                    Ok(value) => {
                        let loaded = value;
                        let module_loader = lua.create_function(move |_, ()| Ok(loaded.clone()))?;
                        Ok(Value::Function(module_loader))
                    }
                    Err(error) => Ok(Value::String(lua.create_string(&error.to_string())?)),
                }
            })
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;

        let searchers: Table = package
            .get::<Table>("searchers")
            .or_else(|_| package.get::<Table>("loaders"))
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;

        let len = searchers.raw_len();
        for index in (2..=len).rev() {
            let value: Value = searchers
                .raw_get(index)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            searchers
                .raw_set(index + 1, value)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        }
        searchers
            .raw_set(2, searcher)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;

        Ok(())
    }

    fn load_module(&self, lua: &Lua, module_name: &str) -> mlua::Result<Value> {
        let module_path =
            resolve_script_module_path(&self.root, module_name).map_err(mlua::Error::external)?;
        let source = fs::read_to_string(&module_path).map_err(mlua::Error::external)?;
        let chunk_name = format!("@{}", module_path.display());

        lua.load(&source)
            .set_name(&chunk_name)
            .eval::<Value>()
            .map_err(|error| {
                mlua::Error::external(format!(
                    "failed to load script module '{}': {}",
                    module_name, error
                ))
            })
    }
}

pub(crate) fn resolve_script_module_path(root: &Path, module_name: &str) -> Result<PathBuf> {
    if module_name.trim().is_empty() {
        anyhow::bail!("script module name must not be empty");
    }

    let mut path = root.to_path_buf();
    for part in module_name.split('.') {
        if part.is_empty() {
            anyhow::bail!("script module name '{module_name}' is invalid");
        }

        let component_path = Path::new(part);
        if component_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            anyhow::bail!("script module name '{module_name}' is invalid");
        }

        path.push(part);
    }
    path.set_extension("lua");

    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to access scripts directory {}", root.display()))?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("script module '{}' was not found", module_name))?;

    if !canonical_path.starts_with(&canonical_root) {
        anyhow::bail!(
            "script module '{}' is outside the scripts directory",
            module_name
        );
    }

    Ok(canonical_path)
}
