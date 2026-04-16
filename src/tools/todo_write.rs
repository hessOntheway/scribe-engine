use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ToolDefinition, ToolExecutor, ToolHandler};

const MAX_TODOS: usize = 20;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TodoItem {
    #[serde(default)]
    id: Option<String>,
    content: String,
    status: TodoStatus,
}

#[derive(Debug, Deserialize)]
struct TodoWriteInput {
    todos: Vec<TodoItem>,
}

#[derive(Debug, Default)]
struct TodoManager {
    todos: Vec<TodoItem>,
}

impl TodoManager {
    fn update(&mut self, todos: Vec<TodoItem>) -> Result<String> {
        if todos.len() > MAX_TODOS {
            bail!("too many todo items: {} (max: {})", todos.len(), MAX_TODOS);
        }

        let mut in_progress_count = 0usize;
        for item in &todos {
            if item.content.trim().is_empty() {
                bail!("todo content cannot be empty");
            }
            if item.status == TodoStatus::InProgress {
                in_progress_count += 1;
            }
        }

        if in_progress_count > 1 {
            bail!("at most one todo item can be in_progress");
        }

        self.todos = todos;
        Ok(self.render())
    }

    fn render(&self) -> String {
        let total = self.todos.len();
        let completed = self
            .todos
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .count();

        let mut lines = vec![format!(
            "Todo list updated: {}/{} completed",
            completed, total
        )];

        for item in &self.todos {
            let marker = match item.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[-]",
                TodoStatus::Completed => "[x]",
            };

            let id_prefix = item
                .id
                .as_ref()
                .map(|id| format!("{}: ", id))
                .unwrap_or_default();

            lines.push(format!("{} {}{}", marker, id_prefix, item.content));
        }

        lines.join("\n")
    }
}

pub fn todo_write_handler() -> ToolHandler {
    let definition = ToolDefinition {
        name: "todo_write".to_string(),
        description:
            "Update the task plan for multi-step work. Provide the full todos list each time."
                .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "Complete todo list state after this update.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Optional stable identifier for the task."
                            },
                            "content": {
                                "type": "string",
                                "description": "Task text. Must be non-empty."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Current task status."
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    },
                    "maxItems": MAX_TODOS
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        }),
    };

    let manager = Arc::new(Mutex::new(TodoManager::default()));

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: TodoWriteInput = serde_json::from_str(input_json)
            .context("invalid input JSON for todo_write; expected {\"todos\": [{\"content\": \"...\", \"status\": \"pending\"}]}")?;

        let mut guard = manager
            .lock()
            .map_err(|_| anyhow::anyhow!("todo_write state lock poisoned"))?;
        guard.update(input.todos)
    });

    ToolHandler::new(definition, execute)
}
