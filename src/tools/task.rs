use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::runtime::AgentLoop;

use super::task_registry::{TaskRecord, TaskStatus};
use super::{GlobalToolRegistry, TaskRegistry, ToolDefinition, ToolExecutor, ToolHandler};

const MAX_TASK_RESULT_CHARS: usize = 50_000;
const TODO_COORDINATION_CONTRACT: &str = "Planning contract: if this subtask is multi-step, call todo_write early with the full list, keep exactly one item in_progress, and mark completed items promptly.";
const TASK_ANALYSIS_CONTRACT: &str = "Analysis contract: this assistant can analyze any codebase scope, including multi-repo and microservice architectures. When asked for learning output, prefer tutorial-style, step-by-step documentation. When architecture or execution flow is requested, provide evidence-backed diagram-ready structure (for example Mermaid) and clearly state missing repository/service context instead of guessing.";

#[derive(Debug, Deserialize)]
struct TaskInput {
    prompt: String,
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskGetInput {
    task_id: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TaskListStatusInput {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Deserialize)]
struct TaskListInput {
    #[serde(default)]
    status: Vec<TaskListStatusInput>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct TaskOutputInput {
    task_id: String,
}

pub fn task_handler(
    agent_loop: Arc<AgentLoop>,
    child_registry: Arc<GlobalToolRegistry>,
    task_registry: Arc<TaskRegistry>,
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
                },
                "task_id": {
                    "type": "string",
                    "description": "Optional stable task identifier. If omitted, one is generated."
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

        let task_id = task_registry
            .start_task(input.task_id.as_deref(), prompt)
            .context("failed to register task")?;

        let wrapped_prompt = format!(
            "{}\n{}\n\nSubtask:\n{}",
            TODO_COORDINATION_CONTRACT, TASK_ANALYSIS_CONTRACT, prompt
        );

        let summary = agent_loop
            .run_subagent(&wrapped_prompt, child_registry.as_ref())
            .map_err(|err| {
                let _ = task_registry.fail_task(&task_id, &err.to_string());
                anyhow!("subagent execution failed for task '{}': {}", task_id, err)
            })?;

        let truncated_summary = truncate_summary(&summary);
        task_registry
            .complete_task(&task_id, &truncated_summary)
            .context("failed to update task completion state")?;

        serde_json::to_string_pretty(&json!({
            "task_id": task_id,
            "status": "completed",
            "summary": truncated_summary,
        }))
        .context("failed to encode task output")
    });

    ToolHandler::new(definition, execute)
}

pub fn task_query_handlers(task_registry: Arc<TaskRegistry>) -> Vec<ToolHandler> {
    vec![
        task_get_handler(Arc::clone(&task_registry)),
        task_list_handler(Arc::clone(&task_registry)),
        task_output_handler(task_registry),
    ]
}

fn task_get_handler(task_registry: Arc<TaskRegistry>) -> ToolHandler {
    let definition = ToolDefinition {
        name: "task_get".to_string(),
        description: "Get one task by task_id with status, ownership, and output summary fields."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task id returned by the task tool."}
            },
            "required": ["task_id"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: TaskGetInput = serde_json::from_str(input_json)
            .context("invalid input JSON for task_get")?;
        let task_id = input.task_id.trim();
        if task_id.is_empty() {
            bail!("task_id cannot be empty");
        }

        let Some(record) = task_registry.get(task_id) else {
            return Ok(task_not_found_json(task_id)?);
        };

        serde_json::to_string_pretty(&json!({
            "ok": true,
            "task": task_record_json(&record, true),
        }))
        .context("failed to encode task_get output")
    });

    ToolHandler::new(definition, execute)
}

fn task_list_handler(task_registry: Arc<TaskRegistry>) -> ToolHandler {
    let definition = ToolDefinition {
        name: "task_list".to_string(),
        description:
            "List tasks with optional status filtering and limit. Returns counts by status for planning."
                .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "array",
                    "items": {"type": "string", "enum": ["running", "completed", "failed"]},
                    "description": "Optional status filter list."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "description": "Optional max tasks to return."
                }
            },
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: TaskListInput = serde_json::from_str(input_json)
            .context("invalid input JSON for task_list")?;

        if let Some(limit) = input.limit {
            if limit == 0 || limit > 200 {
                bail!("limit must be between 1 and 200");
            }
        }

        let status_filters = parse_status_filters(&input.status);
        let tasks = task_registry.list_tasks(
            if status_filters.is_empty() {
                None
            } else {
                Some(&status_filters)
            },
            input.limit,
        )?;

        let all_tasks = task_registry.all_tasks()?;
        let by_status = count_by_status(&all_tasks);
        let task_values = tasks
            .iter()
            .map(|task| task_record_json(task, false))
            .collect::<Vec<_>>();

        serde_json::to_string_pretty(&json!({
            "ok": true,
            "tasks": task_values,
            "count": task_values.len(),
            "by_status": by_status,
        }))
        .context("failed to encode task_list output")
    });

    ToolHandler::new(definition, execute)
}

fn task_output_handler(task_registry: Arc<TaskRegistry>) -> ToolHandler {
    let definition = ToolDefinition {
        name: "task_output".to_string(),
        description:
            "Get output-like view for one task, including preview, error, and availability flags."
                .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task id returned by the task tool."}
            },
            "required": ["task_id"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: TaskOutputInput = serde_json::from_str(input_json)
            .context("invalid input JSON for task_output")?;
        let task_id = input.task_id.trim();
        if task_id.is_empty() {
            bail!("task_id cannot be empty");
        }

        let Some(record) = task_registry.get(task_id) else {
            return Ok(task_not_found_json(task_id)?);
        };

        serde_json::to_string_pretty(&json!({
            "ok": true,
            "task_id": record.id,
            "status": task_status_str(record.status),
            "output": {
                "output": record.result_preview,
                "preview": record.result_preview,
                "error": record.error,
                "has_output": record.result_preview.is_some() || record.error.is_some(),
                "truncated": true,
                "output_truncated": true,
            }
        }))
        .context("failed to encode task_output output")
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

fn task_record_json(task: &TaskRecord, include_prompt: bool) -> Value {
    let mut value = json!({
        "task_id": task.id,
        "status": task_status_str(task.status),
        "assigned_teammate": task.assigned_teammate,
        "result_preview": task.result_preview,
        "error": task.error,
        "created_at_unix_ms": task.created_at_unix_ms,
        "updated_at_unix_ms": task.updated_at_unix_ms,
        "has_output": task.result_preview.is_some() || task.error.is_some(),
        "is_terminal": matches!(task.status, TaskStatus::Completed | TaskStatus::Failed),
    });

    if include_prompt {
        value["prompt"] = json!(task.prompt);
    }

    value
}

fn task_not_found_json(task_id: &str) -> Result<String> {
    serde_json::to_string_pretty(&json!({
        "ok": false,
        "error": {
            "code": "task_not_found",
            "message": format!("task '{}' not found", task_id),
            "task_id": task_id,
        }
    }))
    .context("failed to encode task not found output")
}

fn task_status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "running",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
    }
}

fn parse_status_filters(input: &[TaskListStatusInput]) -> Vec<TaskStatus> {
    input
        .iter()
        .map(|status| match status {
            TaskListStatusInput::Running => TaskStatus::Running,
            TaskListStatusInput::Completed => TaskStatus::Completed,
            TaskListStatusInput::Failed => TaskStatus::Failed,
        })
        .collect()
}

fn count_by_status(tasks: &[TaskRecord]) -> Value {
    let mut running = 0usize;
    let mut completed = 0usize;
    let mut failed = 0usize;

    for task in tasks {
        match task.status {
            TaskStatus::Running => running += 1,
            TaskStatus::Completed => completed += 1,
            TaskStatus::Failed => failed += 1,
        }
    }

    json!({
        "running": running,
        "completed": completed,
        "failed": failed,
    })
}
