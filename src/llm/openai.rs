use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::ContextCompactConfig;
use crate::config::LlmConfig;
use crate::llm::cache::PromptCache;
use crate::llm::usage::ModelUsage;
use crate::tools::ToolDefinition;

const SUBAGENT_SYSTEM_PROMPT: &str = r#"You are a subagent for this repository.

# Mission
- Work from a fresh context and complete the delegated subtask.
- Return only the concise final answer needed by the parent agent.
- Be effective for single-repo and cross-repo architecture analysis tasks, including microservice interactions.

# Execution Rules
- Use tools when needed for correctness.
- For tool calls, provide strict JSON arguments only.
- Do not include internal tool traces unless they are necessary to support correctness.
- Never invent facts; if evidence is missing, state uncertainty briefly.
- Keep the static request prefix stable: do not rewrite the system prompt or tool set mid-session; put changing state into messages or tool results instead.
- If asked for architecture or flow outputs, provide precise, evidence-backed structure that can be rendered as diagrams.

# Planning
- If the delegated work is multi-step, use todo_write, keep exactly one task in_progress, and close completed tasks promptly.
- If blocked, report the blocker and what you already checked.
"#;

#[derive(Clone)]
pub struct OpenAiCompatClient {
    http: Client,
    cfg: LlmConfig,
    cache: Option<PromptCache>,
}

#[derive(Debug, Clone)]
pub struct ChatCompletionResult {
    pub message: Value,
    pub usage: ModelUsage,
    pub cached: bool,
}

impl OpenAiCompatClient {
    pub fn new(cfg: LlmConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("my-claw-blog-agent/0.1"),
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let http = Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .build()
            .context("failed to build llm http client")?;

        let cache = if cfg.enable_prompt_cache {
            Some(PromptCache::new(&cfg.prompt_cache_dir)?)
        } else {
            None
        };

        Ok(Self { http, cfg, cache })
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
    ) -> Result<ChatCompletionResult> {
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
        let mut body = json!({
            "model": self.cfg.model,
            "messages": messages,
            "stream": false
        });

        if !openai_tools.is_empty() {
            body["tools"] = json!(openai_tools);
            body["tool_choice"] = json!("auto");
        }

        let request_hash = request_hash_hex(&body);
        if let Some(cache) = &self.cache
            && let Some(cached) = cache.lookup(&request_hash)?
        {
            eprintln!("info: local prompt cache hit");
            return Ok(cached);
        }

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

        let message = payload
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|choice| choice.get("message"))
            .cloned()
            .context("model response missing choices[0].message")?;

        let usage = payload
            .get("usage")
            .cloned()
            .map(serde_json::from_value::<ModelUsage>)
            .transpose()
            .context("model response usage payload was invalid")?
            .unwrap_or_default();

        let result = ChatCompletionResult {
            message: message.clone(),
            usage: usage.clone(),
            cached: false,
        };

        if let Some(cache) = &self.cache
            && let Err(err) = cache.store(&request_hash, &result)
        {
            eprintln!("warn: failed to write prompt cache entry: {err}");
        }

        Ok(result)
    }
}

fn write_model_audit_log(path: &str, request: &Value, payload: &Value) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        create_dir_all(parent).with_context(|| {
            format!("failed to create audit log directory: {}", parent.display())
        })?;
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

fn request_hash_hex(body: &Value) -> String {
    let canonical = canonicalize_json(body);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let hash = hasher.finalize();
    hash.iter().map(|byte| format!("{:02x}", byte)).collect()
}

fn canonicalize_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s)),
        Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(canonicalize_json).collect();
            format!("[{}]", items.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap(),
                        canonicalize_json(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}
