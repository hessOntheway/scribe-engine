use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::compact::{apply_micro_compact, auto_compact_if_needed};
use crate::llm::openai::OpenAiCompatClient;
use crate::llm::session::ConversationSession;
use crate::llm::usage::PromptCacheStats;
use crate::tools::GlobalToolRegistry;
static SUBAGENT_AUDIT_COUNTER: AtomicU64 = AtomicU64::new(1);

pub struct AgentLoop {
    llm: Arc<OpenAiCompatClient>,
    max_steps: usize,
}

impl AgentLoop {
    pub fn new(llm: Arc<OpenAiCompatClient>, max_steps: usize) -> Self {
        Self { llm, max_steps }
    }

    pub fn run_session_turn(
        &self,
        session: &mut ConversationSession,
        tool_registry: &GlobalToolRegistry,
    ) -> Result<String> {
        let (messages, prompt_cache_stats) = session.messages_and_prompt_cache_stats_mut();
        self.run_message_loop(messages, tool_registry, None, Some(prompt_cache_stats))
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

        let mut messages: Vec<Value> = vec![
            json!({"role": "system", "content": self.llm.subagent_system_prompt()}),
            json!({"role": "user", "content": user_prompt}),
        ];

        self.run_message_loop(&mut messages, tool_registry, audit_path.as_deref(), None)
    }

    fn run_message_loop(
        &self,
        messages: &mut Vec<Value>,
        tool_registry: &GlobalToolRegistry,
        audit_log_path_override: Option<&str>,
        prompt_cache_stats: Option<&mut PromptCacheStats>,
    ) -> Result<String> {
        if self.max_steps == 0 {
            bail!("max_steps must be greater than 0");
        }

        let tool_definitions = tool_registry.definitions();
        let compact_cfg = self.llm.context_compact_config().clone();
        let mut prompt_cache_stats = prompt_cache_stats;

        for _ in 0..self.max_steps {
            apply_micro_compact(messages, &compact_cfg);
            if let Some(event) = auto_compact_if_needed(
                messages,
                &compact_cfg,
                self.llm.as_ref(),
                audit_log_path_override,
                &tool_definitions,
                prompt_cache_stats.as_mut().map(|stats| &mut **stats),
            )? {
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

            if let Some(stats) = prompt_cache_stats.as_mut() {
                if assistant.cached {
                    (**stats).record_local_cache_hit();
                } else {
                    (**stats).record_usage(&assistant.usage);
                }
                eprintln!("{}", (**stats).summary_line());
            }

            messages.push(assistant.message.clone());

            let tool_calls = assistant
                .message
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            if tool_calls.is_empty() {
                let content = assistant
                    .message
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

    pub fn run_session_turn(&self, session: &mut ConversationSession) -> Result<String> {
        self.agent_loop
            .run_session_turn(session, self.tool_registry.as_ref())
    }
}
