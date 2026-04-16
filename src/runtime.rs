use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::compact::{apply_micro_compact, auto_compact_if_needed};
use crate::llm::openai::OpenAiCompatClient;
use crate::tools::GlobalToolRegistry;

const TODO_REMINDER_THRESHOLD: usize = 3;
const TODO_REMINDER_MESSAGE: &str = "For multi-step work, update todo_write with the full task list, keep exactly one in_progress task, and mark completed tasks promptly.";
static SUBAGENT_AUDIT_COUNTER: AtomicU64 = AtomicU64::new(1);

pub struct AgentLoop {
    llm: Arc<OpenAiCompatClient>,
    max_steps: usize,
}

impl AgentLoop {
    pub fn new(llm: Arc<OpenAiCompatClient>, max_steps: usize) -> Self {
        Self { llm, max_steps }
    }

    pub fn run_turn(
        &self,
        user_prompt: &str,
        tool_registry: &GlobalToolRegistry,
    ) -> Result<String> {
        self.run_with_system_prompt(user_prompt, self.llm.system_prompt(), tool_registry, None)
    }

    pub fn run_subagent(
        &self,
        user_prompt: &str,
        tool_registry: &GlobalToolRegistry,
    ) -> Result<String> {
        let audit_path = if self.llm.write_model_audit_log_enabled() {
            Some(self.llm.subagent_audit_log_path(&next_subagent_audit_id()))
        } else {
            None
        };

        self.run_with_system_prompt(
            user_prompt,
            self.llm.subagent_system_prompt(),
            tool_registry,
            audit_path.as_deref(),
        )
    }

    fn run_with_system_prompt(
        &self,
        user_prompt: &str,
        system_prompt: &str,
        tool_registry: &GlobalToolRegistry,
        audit_log_path_override: Option<&str>,
    ) -> Result<String> {
        if self.max_steps == 0 {
            bail!("max_steps must be greater than 0");
        }

        let tool_definitions = tool_registry.definitions();
        let compact_cfg = self.llm.context_compact_config().clone();
        let mut rounds_without_todo_update = 0usize;
        let mut messages: Vec<Value> = vec![
            json!({"role": "system", "content": system_prompt}),
            json!({"role": "user", "content": user_prompt}),
        ];

        for _ in 0..self.max_steps {
            apply_micro_compact(&mut messages, &compact_cfg);
            if let Some(event) = auto_compact_if_needed(&mut messages, &compact_cfg)? {
                let transcript = event
                    .transcript_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<not saved>".to_string());
                eprintln!(
                    "info: auto compact triggered, removed {} messages (estimated tokens: {}), transcript: {}",
                    event.removed_messages, event.estimated_tokens_before, transcript
                );
            }

            let assistant = self
                .llm
                .create_chat_completion_with_audit_path(
                    &messages,
                    &tool_definitions,
                    audit_log_path_override,
                )?;
            messages.push(assistant.clone());

            let tool_calls = assistant
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            if tool_calls.is_empty() {
                let content = assistant
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();

                if content.is_empty() {
                    return Ok("(model returned empty response)".to_string());
                }
                return Ok(content);
            }

            let mut todo_updated_in_round = false;
            for call in tool_calls {
                let tool_id = call
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("tool call id missing")?;
                let function = call
                    .get("function")
                    .and_then(|v| v.as_object())
                    .context("tool function payload missing")?;
                let name = function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .context("tool function name missing")?;
                let arguments = function
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .context("tool function arguments missing")?;

                if name == "todo_write" {
                    todo_updated_in_round = true;
                }

                let result = match tool_registry.execute(name, arguments) {
                    Ok(output) => output,
                    Err(err) => format!("tool_error: {}", err),
                };

                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_id,
                    "content": result,
                }));
            }

            if todo_updated_in_round {
                rounds_without_todo_update = 0;
            } else {
                rounds_without_todo_update += 1;
                if rounds_without_todo_update >= TODO_REMINDER_THRESHOLD {
                    messages.push(json!({
                        "role": "user",
                        "content": TODO_REMINDER_MESSAGE,
                    }));
                    rounds_without_todo_update = 0;
                }
            }
        }

        bail!(
            "model/tool loop reached max steps ({}) without final answer",
            self.max_steps
        )
    }
}

fn next_subagent_audit_id() -> String {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SUBAGENT_AUDIT_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{ts_ms}_{seq}")
}

pub struct ConversationRuntime {
    agent_loop: Arc<AgentLoop>,
    tool_registry: Arc<GlobalToolRegistry>,
}

impl ConversationRuntime {
    pub fn new(
        llm: Arc<OpenAiCompatClient>,
        tool_registry: Arc<GlobalToolRegistry>,
        max_steps: usize,
    ) -> Self {
        Self {
            agent_loop: Arc::new(AgentLoop::new(llm, max_steps)),
            tool_registry,
        }
    }

    pub fn run_turn(&self, user_prompt: &str) -> Result<String> {
        self.agent_loop
            .run_turn(user_prompt, self.tool_registry.as_ref())
    }
}
