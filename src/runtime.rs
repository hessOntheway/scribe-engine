use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::llm::openai::OpenAiCompatClient;
use crate::tools::GlobalToolRegistry;

pub struct ConversationRuntime<'a> {
    llm: &'a OpenAiCompatClient,
    tool_registry: &'a GlobalToolRegistry,
    max_steps: usize,
}

impl<'a> ConversationRuntime<'a> {
    pub fn new(
        llm: &'a OpenAiCompatClient,
        tool_registry: &'a GlobalToolRegistry,
        max_steps: usize,
    ) -> Self {
        Self {
            llm,
            tool_registry,
            max_steps,
        }
    }

    pub fn run_turn(&self, user_prompt: &str) -> Result<String> {
        if self.max_steps == 0 {
            bail!("max_steps must be greater than 0");
        }

        let tool_definitions = self.tool_registry.definitions();
        let mut messages: Vec<Value> = vec![
            json!({"role": "system", "content": self.llm.system_prompt()}),
            json!({"role": "user", "content": user_prompt}),
        ];

        for _ in 0..self.max_steps {
            let assistant = self.llm.create_chat_completion(&messages, &tool_definitions)?;
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

                let result = match self.tool_registry.execute(name, arguments) {
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
