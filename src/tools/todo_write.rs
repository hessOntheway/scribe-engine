use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ToolDefinition, ToolExecutor, ToolHandler};

const MAX_TODOS: usize = 20;
const TODO_STORE_ENV: &str = "TODO_WRITE_PATH";
const DEFAULT_TODO_STORE_PATH: &str = ".scribe-todos.json";

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
    store_path: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct TodoStoreFile {
    todos: Vec<TodoItem>,
}

impl TodoManager {
    fn new(store_path: PathBuf) -> Self {
        let todos = load_todos_from_path(&store_path).unwrap_or_default();
        Self { todos, store_path }
    }

    fn update(&mut self, todos: Vec<TodoItem>) -> Result<String> {
        validate_todos(&todos)?;

        let old_todos = self.todos.clone();
        let verification_nudge_needed = verify_nudge_needed(&todos);
        let all_completed = !todos.is_empty()
            && todos
                .iter()
                .all(|item| item.status == TodoStatus::Completed);

        if all_completed {
            clear_store_file(&self.store_path)?;
        } else {
            save_todos_to_path(&self.store_path, &todos)?;
        }

        self.todos = todos.clone();
        self.render_update_json(old_todos, todos, verification_nudge_needed)
    }

    fn render_update_json(
        &self,
        old_todos: Vec<TodoItem>,
        new_todos: Vec<TodoItem>,
        verification_nudge_needed: bool,
    ) -> Result<String> {
        let total = new_todos.len();
        let completed = new_todos
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .count();
        let active = new_todos
            .iter()
            .filter(|item| item.status != TodoStatus::Completed)
            .count();

        serde_json::to_string_pretty(&json!({
            "summary": format!("Todo list updated: {}/{} completed", completed, total),
            "stats": {
                "total": total,
                "completed": completed,
                "active": active,
            },
            "old_todos": old_todos,
            "new_todos": new_todos,
            "verification_nudge_needed": verification_nudge_needed,
            "store_path": self.store_path,
        }))
        .context("failed to encode todo_write result")
    }
}

fn validate_todos(todos: &[TodoItem]) -> Result<()> {
    if todos.len() > MAX_TODOS {
        bail!("too many todo items: {} (max: {})", todos.len(), MAX_TODOS);
    }

    let mut in_progress_count = 0usize;
    for item in todos {
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

    Ok(())
}

fn verify_nudge_needed(todos: &[TodoItem]) -> bool {
    let all_completed = !todos.is_empty()
        && todos
            .iter()
            .all(|item| item.status == TodoStatus::Completed);
    if !all_completed {
        return false;
    }

    let verification_keywords = ["verify", "validation", "test", "check", "review"];
    let has_verification_task = todos.iter().any(|item| {
        let content = item.content.to_ascii_lowercase();
        verification_keywords.iter().any(|kw| content.contains(kw))
    });

    !has_verification_task
}

fn resolve_store_path() -> PathBuf {
    let raw = std::env::var(TODO_STORE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TODO_STORE_PATH.to_string());
    PathBuf::from(raw)
}

fn load_todos_from_path(path: &PathBuf) -> Result<Vec<TodoItem>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read todo store: {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }

    let store: TodoStoreFile = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse todo store: {}", path.display()))?;
    validate_todos(&store.todos)?;
    Ok(store.todos)
}

fn save_todos_to_path(path: &PathBuf, todos: &[TodoItem]) -> Result<()> {
    let payload = TodoStoreFile {
        todos: todos.to_vec(),
    };
    let text = serde_json::to_string_pretty(&payload).context("failed to encode todo store")?;
    fs::write(path, text)
        .with_context(|| format!("failed to write todo store: {}", path.display()))
}

fn clear_store_file(path: &PathBuf) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("failed to clear todo store file: {}", path.display()))?;
    }
    Ok(())
}

pub fn has_persisted_active_todos() -> bool {
    let path = resolve_store_path();
    let todos = match load_todos_from_path(&path) {
        Ok(items) => items,
        Err(_) => return false,
    };

    todos
        .iter()
        .any(|item| item.status == TodoStatus::Pending || item.status == TodoStatus::InProgress)
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

    let manager = Arc::new(Mutex::new(TodoManager::new(resolve_store_path())));

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
