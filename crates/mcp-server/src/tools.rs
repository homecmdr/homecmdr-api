use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::api::ApiClient;

// ── Tool schema ───────────────────────────────────────────────────────────────

pub fn list() -> Value {
    json!([
        {
            "name": "list_files",
            "description": "List all Lua files available in the scenes, automations, and scripts directories.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        },
        {
            "name": "read_file",
            "description": "Read the contents of a Lua file. Path must be in the form {type}/{filename}, e.g. scenes/wakeup.lua.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path, e.g. scenes/wakeup.lua" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "write_file",
            "description": "Write (create or overwrite) a Lua file. Path must be in the form {type}/{filename}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path, e.g. scenes/wakeup.lua" },
                    "content": { "type": "string", "description": "Full Lua source to write" }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "reload_scenes",
            "description": "Trigger a reload of all scene Lua files from disk.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "reload_automations",
            "description": "Trigger a reload of all automation Lua files from disk.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "reload_scripts",
            "description": "Trigger a reload of all script Lua files from disk.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "list_devices",
            "description": "List all devices currently known to the smart home runtime.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "list_rooms",
            "description": "List all rooms defined in the smart home runtime.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "list_capabilities",
            "description": "List all supported device capabilities and their schemas.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "list_adapters",
            "description": "List all adapter instances and their current runtime status.",
            "inputSchema": { "type": "object", "properties": {}, "required": [] }
        },
        {
            "name": "scaffold_adapter",
            "description": "Create a new adapter crate skeleton under crates/adapter-{name}/. Returns the boilerplate files written and the three manual registration steps required to wire it into the workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Adapter name in snake_case, e.g. my_sensor" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "run_cargo_check",
            "description": "Run `cargo check` against the workspace or a specific package.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "package": { "type": "string", "description": "Optional crate name to check. Omit to check the whole workspace." }
                },
                "required": []
            }
        },
        {
            "name": "run_cargo_test",
            "description": "Run `cargo test` against a specific package or the whole workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "package": { "type": "string", "description": "Optional crate name to test. Omit to test the whole workspace." }
                },
                "required": []
            }
        }
    ])
}

// ── Tool dispatch ─────────────────────────────────────────────────────────────

pub async fn call(
    name: &str,
    args: &Value,
    client: &ApiClient,
    workspace: &str,
) -> anyhow::Result<String> {
    match name {
        "list_files" => tool_list_files(client).await,
        "read_file" => tool_read_file(client, args).await,
        "write_file" => tool_write_file(client, args).await,
        "reload_scenes" => tool_reload(client, "scenes").await,
        "reload_automations" => tool_reload(client, "automations").await,
        "reload_scripts" => tool_reload(client, "scripts").await,
        "list_devices" => tool_get_json(client, "/devices").await,
        "list_rooms" => tool_get_json(client, "/rooms").await,
        "list_capabilities" => tool_get_json(client, "/capabilities").await,
        "list_adapters" => tool_get_json(client, "/adapters").await,
        "scaffold_adapter" => tool_scaffold_adapter(args, workspace),
        "run_cargo_check" => tool_cargo(args, workspace, "check").await,
        "run_cargo_test" => tool_cargo(args, workspace, "test").await,
        unknown => anyhow::bail!("unknown tool: {unknown}"),
    }
}

// ── Individual tool implementations ──────────────────────────────────────────

async fn tool_list_files(client: &ApiClient) -> anyhow::Result<String> {
    let files = client.get("/files").await?;
    Ok(serde_json::to_string_pretty(&files)?)
}

async fn tool_read_file(client: &ApiClient, args: &Value) -> anyhow::Result<String> {
    let path = require_str(args, "path")?;
    // URL-encode the path so slashes are preserved correctly.
    let encoded = path.replace('/', "%2F");
    client.get_text(&format!("/files/{encoded}")).await
}

async fn tool_write_file(client: &ApiClient, args: &Value) -> anyhow::Result<String> {
    let path = require_str(args, "path")?;
    let content = require_str(args, "content")?;
    let encoded = path.replace('/', "%2F");
    client
        .put_json(&format!("/files/{encoded}"), &json!({ "content": content }))
        .await?;
    Ok(format!("wrote {path}"))
}

async fn tool_reload(client: &ApiClient, target: &str) -> anyhow::Result<String> {
    let result = client
        .post(&format!("/{target}/reload"), &json!({}))
        .await?;
    Ok(serde_json::to_string_pretty(&result)?)
}

async fn tool_get_json(client: &ApiClient, path: &str) -> anyhow::Result<String> {
    let result = client.get(path).await?;
    Ok(serde_json::to_string_pretty(&result)?)
}

fn tool_scaffold_adapter(args: &Value, workspace: &str) -> anyhow::Result<String> {
    let name = require_str(args, "name")?;
    validate_adapter_name(name)?;

    let pascal = to_pascal_case(name);
    let crate_dir = PathBuf::from(workspace).join(format!("crates/adapter-{name}"));
    let src_dir = crate_dir.join("src");
    std::fs::create_dir_all(&src_dir)?;

    let cargo_toml = format!(
        r#"[package]
name = "adapter-{name}"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"
async-trait = "0.1"
inventory = "0.3"
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
smart-home-core = {{ path = "../core" }}
tokio = {{ version = "1", features = ["rt", "time"] }}
tracing = "0.1"
"#
    );

    let lib_rs = format!(
        r#"use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use smart_home_core::adapter::{{Adapter, AdapterConfig, AdapterFactory}};
use smart_home_core::bus::EventBus;
use smart_home_core::registry::DeviceRegistry;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct {pascal}Config {{
    // TODO: add adapter-specific configuration fields here
    enabled: bool,
}}

// ── Factory ───────────────────────────────────────────────────────────────────

struct {pascal}Factory;

inventory::submit! {{
    &{pascal}Factory as &dyn AdapterFactory
}}

impl AdapterFactory for {pascal}Factory {{
    fn name(&self) -> &'static str {{
        "{name}"
    }}

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {{
        let cfg: {pascal}Config = serde_json::from_value(config)?;
        if !cfg.enabled {{
            return Ok(None);
        }}
        Ok(Some(Box::new({pascal}Adapter {{ config: cfg }})))
    }}
}}

// ── Adapter ───────────────────────────────────────────────────────────────────

struct {pascal}Adapter {{
    #[allow(dead_code)]
    config: {pascal}Config,
}}

#[async_trait]
impl Adapter for {pascal}Adapter {{
    fn name(&self) -> &str {{
        "{name}"
    }}

    async fn run(&self, _registry: DeviceRegistry, _bus: EventBus) -> Result<()> {{
        // TODO: implement adapter main loop
        std::future::pending::<()>().await;
        Ok(())
    }}
}}
"#
    );

    write_file(&crate_dir.join("Cargo.toml"), &cargo_toml)?;
    write_file(&src_dir.join("lib.rs"), &lib_rs)?;

    Ok(format!(
        r#"Scaffolded adapter-{name} in crates/adapter-{name}/.

Manual registration steps required (the scaffold does NOT modify these files):

1. root Cargo.toml — add to [workspace] members:
       "crates/adapter-{name}",

2. crates/adapters/Cargo.toml — add to [dependencies]:
       adapter-{name} = {{ path = "../adapter-{name}" }}

3. crates/adapters/src/lib.rs — add:
       use adapter_{under} as _;

Then run: cargo check --workspace
"#,
        under = name.replace('-', "_")
    ))
}

async fn tool_cargo(args: &Value, workspace: &str, subcommand: &str) -> anyhow::Result<String> {
    let package = args.get("package").and_then(|v| v.as_str());

    let mut cmd = tokio::process::Command::new("cargo");
    cmd.arg(subcommand);
    cmd.current_dir(workspace);

    if let Some(pkg) = package {
        cmd.arg("-p").arg(pkg);
    } else {
        cmd.arg("--workspace");
    }

    let output = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    if output.status.success() {
        Ok(combined)
    } else {
        anyhow::bail!("cargo {subcommand} failed:\n{combined}")
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn require_str<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument '{key}'"))
}

fn validate_adapter_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("adapter name must not be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!(
            "adapter name must contain only ASCII alphanumerics, underscores, or hyphens"
        );
    }
    Ok(())
}

/// Convert a snake_case or kebab-case string to PascalCase.
pub fn to_pascal_case(s: &str) -> String {
    s.split(|c: char| c == '_' || c == '-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    upper + chars.as_str()
                }
            }
        })
        .collect()
}

fn write_file(path: &Path, content: &str) -> anyhow::Result<()> {
    std::fs::write(path, content)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_case_from_snake_case() {
        assert_eq!(to_pascal_case("my_sensor"), "MySensor");
        assert_eq!(to_pascal_case("open_meteo"), "OpenMeteo");
        assert_eq!(to_pascal_case("zigbee2mqtt"), "Zigbee2mqtt");
    }

    #[test]
    fn pascal_case_from_kebab_case() {
        assert_eq!(to_pascal_case("my-sensor"), "MySensor");
        assert_eq!(to_pascal_case("elgato-lights"), "ElgatoLights");
    }

    #[test]
    fn validate_adapter_name_rejects_bad_names() {
        assert!(validate_adapter_name("").is_err());
        assert!(validate_adapter_name("has space").is_err());
        assert!(validate_adapter_name("has/slash").is_err());
        assert!(validate_adapter_name("has..dots").is_err());
    }

    #[test]
    fn validate_adapter_name_accepts_good_names() {
        assert!(validate_adapter_name("my_sensor").is_ok());
        assert!(validate_adapter_name("my-sensor").is_ok());
        assert!(validate_adapter_name("zigbee2mqtt").is_ok());
    }
}
