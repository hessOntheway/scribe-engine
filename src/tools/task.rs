use std::sync::Arc;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::json;

use crate::runtime::AgentLoop;

use super::{GlobalToolRegistry, ToolDefinition, ToolExecutor, ToolHandler};

const MAX_TASK_RESULT_CHARS: usize = 50_000;

#[derive(Debug, Deserialize)]
struct TaskInput {
    prompt: String,
}

pub fn task_handler(
    agent_loop: Arc<AgentLoop>,
    child_registry: Arc<GlobalToolRegistry>,
) -> ToolHandler {
    let definition = ToolDefinition {
        name: "task".to_string(),
        description: "Spawn a subagent with fresh context to work on an isolated subtask and return a concise summary only.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The isolated subtask prompt for the subagent."
                }
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: TaskInput = serde_json::from_str(input_json)
            .context("invalid input JSON for task; expected {\"prompt\": \"...\"}")?;

        let prompt = input.prompt.trim();
        if prompt.is_empty() {
            bail!("task prompt cannot be empty");
        }

        let summary = agent_loop
            .run_subagent(prompt, child_registry.as_ref())
            .context("subagent execution failed")?;

        Ok(truncate_summary(&summary))
    });

    ToolHandler::new(definition, execute)
}

fn truncate_summary(summary: &str) -> String {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return "(no summary)".to_string();
    }

    let char_count = trimmed.chars().count();
    if char_count <= MAX_TASK_RESULT_CHARS {
        return trimmed.to_string();
    }

    trimmed.chars().take(MAX_TASK_RESULT_CHARS).collect()
}
