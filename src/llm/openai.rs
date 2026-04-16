use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};

use crate::config::ContextCompactConfig;
use crate::config::LlmConfig;
use crate::tools::ToolDefinition;

const SYSTEM_PROMPT: &str = "You are a code analysis assistant for this repository. Help users understand overall architecture and specific feature behavior using concrete evidence from the codebase. Use available tools when they improve accuracy or when the user requests diagrams or publishing actions. For multi-step tasks, maintain progress using todo_write: keep exactly one task in_progress and mark tasks completed promptly. If a subtask benefits from a clean context, use task to delegate it and return to the parent with the result. Never invent facts; if evidence is missing, state uncertainty and request the minimal missing context. Keep answers concise, structured, and implementation-focused. When calling tools, always provide strict JSON arguments only.";
const SUBAGENT_SYSTEM_PROMPT: &str = "You are a subagent for this repository. Work from a fresh context, use tools as needed, and return only the concise final answer that helps the parent agent. Do not mention internal tool traces unless they are necessary to support the answer. When calling tools, always provide strict JSON arguments only.";

#[derive(Clone)]
pub struct OpenAiCompatClient {
    http: Client,
    cfg: LlmConfig,
}

impl OpenAiCompatClient {
    pub fn new(cfg: LlmConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("my-claw-blog-agent/0.1"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let http = Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .context("failed to build llm http client")?;

        Ok(Self { http, cfg })
    }

    pub fn system_prompt(&self) -> &str {
        SYSTEM_PROMPT
    }

    pub fn subagent_system_prompt(&self) -> &str {
        SUBAGENT_SYSTEM_PROMPT
    }

    pub fn context_compact_config(&self) -> &ContextCompactConfig {
        &self.cfg.context_compact
    }

    pub fn write_model_audit_log_enabled(&self) -> bool {
        self.cfg.write_model_audit_log
    }

    pub fn subagent_audit_log_path(&self, subagent_id: &str) -> String {
        let configured = Path::new(&self.cfg.model_audit_log_path);
        let parent_dir = configured.parent().unwrap_or_else(|| Path::new("."));
        let subagent_dir = parent_dir.join("subagents");

        let stem = configured
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("llm_response_audit");

        let extension = configured
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("json");

        subagent_dir
            .join(format!("{}_subagent_{}.{}", stem, subagent_id, extension))
            .to_string_lossy()
            .to_string()
    }

    pub fn create_chat_completion_with_audit_path(
        &self,
        messages: &[Value],
        tools: &[ToolDefinition],
        audit_log_path_override: Option<&str>,
    ) -> Result<Value> {
        let openai_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema
                    }
                })
            })
            .collect();

        let url = format!("{}/chat/completions", self.cfg.base_url);
        let body = json!({
            "model": self.cfg.model,
            "messages": messages,
            "tools": openai_tools,
            "tool_choice": "auto",
            "stream": false
        });

        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .with_context(|| {
                format!(
                    "failed to call model api: url={}, model={} (check network/proxy/LLM_API_KEY)",
                    url, self.cfg.model
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().unwrap_or_else(|_| "<no body>".to_string());
            bail!("model api error ({status}): {text}");
        }

        let payload: Value = response
            .json()
            .context("failed to decode model api response")?;

        if self.cfg.write_model_audit_log {
            let audit_log_path = audit_log_path_override.unwrap_or(&self.cfg.model_audit_log_path);
            if let Err(err) = write_model_audit_log(audit_log_path, &body, &payload) {
                eprintln!("warn: failed to write llm audit log: {err}");
            }
        }

        payload
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|choice| choice.get("message"))
            .cloned()
            .context("model response missing choices[0].message")
    }
}

fn write_model_audit_log(path: &str, request: &Value, payload: &Value) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_all(parent).with_context(|| {
                format!("failed to create audit log directory: {}", parent.display())
            })?;
        }
    }

    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock error")?
        .as_millis();

    let record = json!({
        "event": "llm_exchange",
        "ts_unix_ms": ts_ms,
        "request": request,
        "response": payload
    });

    let mut records: Vec<Value> = match read_to_string(path) {
        Ok(contents) if !contents.trim().is_empty() => serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse existing audit log: {path}"))?,
        _ => Vec::new(),
    };

    records.push(record);
    let pretty = serde_json::to_string_pretty(&records).context("failed to encode audit log")?;
    write(path, pretty).with_context(|| format!("failed to write {path}"))?;
    Ok(())
}
