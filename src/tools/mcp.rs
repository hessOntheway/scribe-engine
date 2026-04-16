use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolDefinition, ToolExecutor, ToolHandler};

#[derive(Debug, Clone, Deserialize)]
struct McpServerConfig {
    name: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    input_schema: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpToolsListResult {
    #[serde(default)]
    tools: Vec<McpTool>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug)]
struct McpProcess {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    wire_protocol: WireProtocol,
    next_id: u64,
    initialized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireProtocol {
    ContentLength,
    JsonLine,
}

#[derive(Debug)]
struct McpClient {
    config: McpServerConfig,
    process: McpProcess,
}

pub fn mcp_plugin_tools_from_config() -> Result<Vec<ToolHandler>> {
    let servers = load_servers_from_config_file()?;

    let mut handlers = Vec::new();

    for server in servers {
        let mut client = McpClient::spawn(server.clone())
            .with_context(|| format!("failed to spawn MCP server: {}", server.name))?;
        let tools = client
            .discover_tools()
            .with_context(|| format!("failed to discover MCP tools for server: {}", server.name))?;

        let shared_client = Arc::new(Mutex::new(client));

        for tool in tools {
            let qualified_name = format!(
                "mcp__{}__{}",
                normalize_name(&server.name),
                normalize_name(&tool.name)
            );
            let server_name = server.name.clone();
            let tool_name = tool.name.clone();
            let shared = Arc::clone(&shared_client);

            let definition = ToolDefinition {
                name: qualified_name,
                description: if tool.description.trim().is_empty() {
                    format!("MCP tool '{}' from server '{}'", tool.name, server_name)
                } else {
                    format!("{} (from MCP server '{}')", tool.description, server_name)
                },
                input_schema: tool
                    .input_schema
                    .unwrap_or_else(|| json!({"type": "object", "additionalProperties": true})),
            };

            let execute: ToolExecutor = Arc::new(move |input_json: &str| {
                let arguments: Value = serde_json::from_str(input_json).with_context(|| {
                    format!(
                        "invalid input JSON for MCP tool '{}'; arguments must be strict JSON",
                        tool_name
                    )
                })?;
                let mut locked = shared
                    .lock()
                    .map_err(|_| anyhow!("failed to lock MCP client for tool call"))?;
                let mut result = locked
                    .call_tool(&tool_name, arguments)
                    .with_context(|| format!("MCP tool call failed: {}", tool_name))?;
                let saved_files = persist_graph_images(&tool_name, &mut result)?;

                if !saved_files.is_empty() {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert("saved_files".to_string(), json!(saved_files));
                    }
                }

                serde_json::to_string_pretty(&result).context("failed to serialize MCP tool output")
            });

            handlers.push(ToolHandler::new(definition, execute));
        }
    }

    Ok(handlers)
}

fn load_servers_from_config_file() -> Result<Vec<McpServerConfig>> {
    let config_path = Path::new("config/mcp_servers.json");
    if !config_path.exists() {
        return Ok(vec![default_mermaid_server()]);
    }

    let raw = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read MCP config file: {}", config_path.display()))?;

    if raw.trim().is_empty() {
        return Ok(vec![default_mermaid_server()]);
    }

    serde_json::from_str(&raw).with_context(|| {
        format!(
            "invalid MCP config in {}: expected JSON array of MCP server configs",
            config_path.display()
        )
    })
}

impl McpClient {
    fn spawn(config: McpServerConfig) -> Result<Self> {
        let (command, args) = if let Some(url) = config.url.as_deref() {
            let mut remote_args = vec!["-y".to_string(), "mcp-remote".to_string(), url.to_string()];

            if let Some(transport) = config.transport.as_deref() {
                remote_args.push("--transport".to_string());
                remote_args.push(normalize_remote_transport(transport).to_string());
            }

            for (header_name, header_value) in &config.headers {
                remote_args.push("--header".to_string());
                remote_args.push(format!("{}: {}", header_name, header_value));
            }

            remote_args.extend(config.args.clone());
            ("npx".to_string(), remote_args)
        } else {
            let command = config
                .command
                .clone()
                .context("missing MCP server command")?;
            (command, config.args.clone())
        };

        let mut cmd = Command::new(&command);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        if !config.env.is_empty() {
            cmd.envs(config.env.clone());
        }

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "failed to spawn MCP server '{}' with command '{}'",
                config.name, command
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .context("failed to capture MCP server stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to capture MCP server stdout")?;

        let process = McpProcess {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
            wire_protocol: if config.url.is_some() {
                WireProtocol::JsonLine
            } else {
                WireProtocol::ContentLength
            },
            next_id: 1,
            initialized: false,
        };

        Ok(Self { config, process })
    }

    fn discover_tools(&mut self) -> Result<Vec<McpTool>> {
        self.ensure_initialized()?;

        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let params = if let Some(cursor_value) = &cursor {
                json!({"cursor": cursor_value})
            } else {
                json!({})
            };

            let response = self
                .request("tools/list", params)
                .with_context(|| format!("tools/list failed for server '{}'", self.config.name))?;

            let page: McpToolsListResult = serde_json::from_value(response)
                .context("invalid MCP tools/list response payload")?;
            all_tools.extend(page.tools);

            if let Some(next) = page.next_cursor {
                if next.is_empty() {
                    break;
                }
                cursor = Some(next);
            } else {
                break;
            }
        }

        Ok(all_tools)
    }

    fn call_tool(&mut self, tool_name: &str, arguments: Value) -> Result<Value> {
        self.ensure_initialized()?;

        self.request(
            "tools/call",
            json!({
                "name": tool_name,
                "arguments": arguments,
            }),
        )
    }

    fn ensure_initialized(&mut self) -> Result<()> {
        if self.process.initialized {
            return Ok(());
        }

        let _initialize_result = self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "scribe-engine",
                    "version": "0.1.0"
                }
            }),
        )?;

        self.notify("notifications/initialized", json!({}))?;
        self.process.initialized = true;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.process.next_id;
        self.process.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        self.write_message(&request)?;

        loop {
            let message = self.read_message()?;
            let Some(resp_id) = message.get("id").and_then(|v| v.as_u64()) else {
                continue;
            };

            if resp_id != id {
                continue;
            }

            if let Some(error_obj) = message.get("error") {
                bail!("MCP {} returned error: {}", method, error_obj);
            }

            return message
                .get("result")
                .cloned()
                .context("MCP response missing result field");
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&notification)
    }

    fn write_message(&mut self, message: &Value) -> Result<()> {
        let payload =
            serde_json::to_vec(message).context("failed to encode MCP jsonrpc payload")?;
        match self.process.wire_protocol {
            WireProtocol::ContentLength => {
                let header = format!("Content-Length: {}\r\n\r\n", payload.len());

                self.process
                    .stdin
                    .write_all(header.as_bytes())
                    .context("failed to write MCP header")?;
                self.process
                    .stdin
                    .write_all(&payload)
                    .context("failed to write MCP payload")?;
            }
            WireProtocol::JsonLine => {
                self.process
                    .stdin
                    .write_all(&payload)
                    .context("failed to write MCP payload")?;
                self.process
                    .stdin
                    .write_all(b"\n")
                    .context("failed to write MCP newline")?;
            }
        }
        self.process
            .stdin
            .flush()
            .context("failed to flush MCP payload")?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value> {
        if self.process.wire_protocol == WireProtocol::JsonLine {
            loop {
                let mut line = String::new();
                let bytes = self
                    .process
                    .stdout
                    .read_line(&mut line)
                    .context("failed reading MCP response line")?;

                if bytes == 0 {
                    bail!("MCP server closed stdout unexpectedly");
                }

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                return serde_json::from_str(trimmed).context("invalid JSON in MCP response line");
            }
        }

        let mut content_length: Option<usize> = None;

        loop {
            let mut line = String::new();
            let bytes = self
                .process
                .stdout
                .read_line(&mut line)
                .context("failed reading MCP response header")?;

            if bytes == 0 {
                bail!("MCP server closed stdout unexpectedly");
            }

            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }

            if let Some((key, value)) = trimmed.split_once(':') {
                if key.eq_ignore_ascii_case("Content-Length") {
                    let parsed = value
                        .trim()
                        .parse::<usize>()
                        .context("invalid Content-Length value from MCP server")?;
                    content_length = Some(parsed);
                }
            }
        }

        let size = content_length.context("missing Content-Length in MCP response")?;
        let mut body = vec![0_u8; size];
        self.process
            .stdout
            .read_exact(&mut body)
            .context("failed reading MCP response body")?;

        serde_json::from_slice(&body).context("invalid JSON in MCP response body")
    }
}

fn normalize_remote_transport(raw: &str) -> &str {
    match raw {
        // Mermaid docs use generic transport names in client config.
        "http" => "http-only",
        "sse" => "sse-only",
        // Pass through mcp-remote native values.
        "http-only" | "http-first" | "sse-only" | "sse-first" => raw,
        // Fall back to mcp-remote default strategy when unknown.
        _ => "http-first",
    }
}

fn normalize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }

    out.trim_matches('_').to_string()
}

fn default_mermaid_server() -> McpServerConfig {
    McpServerConfig {
        name: "mermaid".to_string(),
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some("https://mcp.mermaid.ai/mcp".to_string()),
        transport: Some("http".to_string()),
        headers: HashMap::new(),
    }
}

fn persist_graph_images(tool_name: &str, result: &mut Value) -> Result<Vec<String>> {
    let Some(content_items) = result.get("content").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };

    let graph_dir = Path::new(".graph");
    fs::create_dir_all(graph_dir)
        .with_context(|| format!("failed to create graph directory: {}", graph_dir.display()))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock error")?
        .as_millis();

    let mut saved_files = Vec::new();
    let safe_tool_name = normalize_name(tool_name);

    let mut image_items: Vec<&Value> = content_items
        .iter()
        .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("image"))
        .collect();
    image_items.sort_by_key(|item| image_priority(item.get("mimeType").and_then(|v| v.as_str())));

    for (idx, item) in image_items.into_iter().enumerate() {
        let Some(data_b64) = item.get("data").and_then(|v| v.as_str()) else {
            continue;
        };

        let mime_type = item
            .get("mimeType")
            .and_then(|v| v.as_str())
            .unwrap_or("image/png");
        let ext = mime_extension(mime_type);

        let file_name = format!("{}_{}_{}.{}", safe_tool_name, now, idx, ext);
        let file_path = graph_dir.join(file_name);
        let bytes = decode_image_bytes(data_b64, mime_type, tool_name)?;

        fs::write(&file_path, bytes)
            .with_context(|| format!("failed to write graph image: {}", file_path.display()))?;

        saved_files.push(file_path.to_string_lossy().to_string());
    }

    Ok(saved_files)
}

fn image_priority(mime: Option<&str>) -> u8 {
    match mime {
        Some("image/svg+xml") => 0,
        Some("image/png") => 1,
        Some("image/webp") => 2,
        Some("image/jpeg") => 3,
        _ => 9,
    }
}

fn decode_image_bytes(data: &str, mime: &str, tool_name: &str) -> Result<Vec<u8>> {
    if mime == "image/svg+xml" && data.trim_start().starts_with("<svg") {
        return Ok(data.as_bytes().to_vec());
    }

    base64::engine::general_purpose::STANDARD
        .decode(data)
        .with_context(|| format!("failed to decode image data for tool {}", tool_name))
}

fn mime_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/svg+xml" => "svg",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "bin",
    }
}
