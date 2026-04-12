use std::fs::{create_dir_all, read_to_string, write};
use std::time::{SystemTime, UNIX_EPOCH};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};

use crate::config::LlmConfig;
use crate::tools::ToolDefinition;

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
        &self.cfg.system_prompt
    }

    pub fn create_chat_completion(&self, messages: &[Value], tools: &[ToolDefinition]) -> Result<Value> {
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
            if let Err(err) = write_model_audit_log(
                &self.cfg.model_audit_log_path,
                &body,
                &payload,
            ) {
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

fn write_model_audit_log(
    path: &str,
    request: &Value,
    payload: &Value,
) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_all(parent)
                .with_context(|| format!("failed to create audit log directory: {}", parent.display()))?;
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
