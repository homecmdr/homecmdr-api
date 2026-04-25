// This module lets Lua scripts share code with each other via `require`.
//
// Without this, every script would have to copy-paste any helper functions
// it needs.  With it, you can write a shared library file and load it from
// multiple scenes or automations like:
//
//   local utils = require("shared.utils")
//
// The loader only looks in the configured scripts directory — it refuses to
// load files from outside that directory to prevent path-traversal attacks.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use mlua::{Lua, Table, Value};

// Holds the root directory that all `require` calls are resolved relative to.
#[derive(Clone)]
pub struct ScriptLoader {
    pub root: PathBuf,
}

impl ScriptLoader {
    // Registers this loader with the Lua VM by inserting it into the
    // `package.searchers` list (position 2, right after Lua's built-in
    // preload searcher).  Any `require("foo.bar")` call will then try
    // to find `<root>/foo/bar.lua` before falling back to the standard loaders.
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
                        // Wrap the loaded value in a zero-argument function so
                        // Lua's require machinery can call it in the normal way.
                        let module_loader = lua.create_function(move |_, ()| Ok(loaded.clone()))?;
                        Ok(Value::Function(module_loader))
                    }
                    // Return an error string instead of raising so Lua can
                    // keep trying other searchers in the list.
                    Err(error) => Ok(Value::String(lua.create_string(&error.to_string())?)),
                }
            })
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;

        // Shift all existing searchers after position 1 down by one slot to
        // make room for ours at position 2.
        let searchers: Table = package
            .get::<Table>("searchers")
            .or_else(|_| package.get::<Table>("loaders")) // Lua 5.1 compat name
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

    // Reads and evaluates a module file, returning whatever the file returns.
    // The `@` prefix on the chunk name is a Lua convention that tells the VM
    // to display the path in stack traces instead of the raw source code.
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

// Converts a dotted module name like `"vision.ollama"` into a file path like
// `<root>/vision/ollama.lua`.
//
// This also enforces a security boundary: if the resolved path ends up outside
// the root directory (e.g. via `../../etc/passwd`), the call is rejected.
pub(crate) fn resolve_script_module_path(root: &Path, module_name: &str) -> Result<PathBuf> {
    if module_name.trim().is_empty() {
        anyhow::bail!("script module name must not be empty");
    }

    let mut path = root.to_path_buf();
    for part in module_name.split('.') {
        if part.is_empty() {
            anyhow::bail!("script module name '{module_name}' is invalid");
        }

        // Reject any path component that tries to escape the root directory.
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

    // Canonicalize both paths (resolves symlinks) so the prefix check below
    // is reliable on all platforms.
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
