mod api;
mod tools;

use anyhow::Result;
use clap::Parser;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use api::ApiClient;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "mcp-server",
    about = "Smart home MCP server — JSON-RPC 2.0 over stdio"
)]
struct Args {
    /// Base URL of the smart home API.
    #[arg(long, default_value = "http://127.0.0.1:3001")]
    api_url: String,

    /// Bearer token for API authentication.
    #[arg(long, env = "SMART_HOME_TOKEN")]
    token: String,

    /// Workspace root directory (used for scaffold_adapter and cargo tools).
    #[arg(long, env = "SMART_HOME_WORKSPACE", default_value = ".")]
    workspace: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = ApiClient::new(&args.api_url, &args.token)?;

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        // Parse the incoming JSON-RPC message.
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let response = rpc_error(Value::Null, -32700, &format!("parse error: {e}"));
                write_response(&mut stdout, &response).await?;
                continue;
            }
        };

        // Notifications (requests without an `id`) are silently ignored per
        // JSON-RPC 2.0 / MCP spec.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };

        let method = request["method"].as_str().unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let response = match dispatch(method, &params, &client, &args.workspace).await {
            Ok(result) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }),
            Err((code, message)) => rpc_error(id, code, &message),
        };

        write_response(&mut stdout, &response).await?;
    }

    Ok(())
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

/// Returns `Ok(result_value)` on success or `Err((rpc_code, message))` on a
/// protocol-level failure.  Tool-level errors are returned as
/// `Ok({"content":[…],"isError":true})` so the MCP client can surface them.
async fn dispatch(
    method: &str,
    params: &Value,
    client: &ApiClient,
    workspace: &str,
) -> std::result::Result<Value, (i32, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "smart-home-mcp",
                "version": "0.1.0"
            }
        })),

        "tools/list" => Ok(json!({ "tools": tools::list() })),

        "tools/call" => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602i32, "missing 'name' in params".to_string()))?;

            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));

            let content = match tools::call(name, &args, client, workspace).await {
                Ok(text) => json!({
                    "content": [{ "type": "text", "text": text }]
                }),
                Err(e) => json!({
                    "content": [{ "type": "text", "text": e.to_string() }],
                    "isError": true
                }),
            };
            Ok(content)
        }

        other => Err((-32601, format!("method not found: {other}"))),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

async fn write_response(stdout: &mut tokio::io::Stdout, response: &Value) -> Result<()> {
    let mut out = serde_json::to_string(response)?;
    out.push('\n');
    stdout.write_all(out.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}
