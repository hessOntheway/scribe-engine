use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRecord {
    pub id: String,
    pub prompt: String,
    pub status: TaskStatus,
    pub assigned_teammate: Option<String>,
    pub result_preview: Option<String>,
    pub error: Option<String>,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
}

#[derive(Debug)]
pub struct TaskRegistry {
    tasks: Mutex<HashMap<String, TaskRecord>>,
    next_id: AtomicU64,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn start_task(&self, id_hint: Option<&str>, prompt: &str) -> Result<String> {
        let task_id = match id_hint {
            Some(raw) => normalize_task_id(raw)?,
            None => self.generate_task_id(),
        };

        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| anyhow::anyhow!("task registry lock poisoned"))?;

        if tasks.contains_key(&task_id) {
            bail!("task id '{}' already exists", task_id);
        }

        let now = now_unix_ms();
        tasks.insert(
            task_id.clone(),
            TaskRecord {
                id: task_id.clone(),
                prompt: prompt.to_string(),
                status: TaskStatus::Running,
                assigned_teammate: None,
                result_preview: None,
                error: None,
                created_at_unix_ms: now,
                updated_at_unix_ms: now,
            },
        );

        Ok(task_id)
    }

    pub fn complete_task(&self, task_id: &str, result_preview: &str) -> Result<()> {
        self.update_task(task_id, TaskStatus::Completed, Some(result_preview), None)
    }

    pub fn fail_task(&self, task_id: &str, error: &str) -> Result<()> {
        self.update_task(task_id, TaskStatus::Failed, None, Some(error))
    }

    pub fn bind_teammate(&self, task_id: &str, teammate: &str) -> Result<()> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| anyhow::anyhow!("task registry lock poisoned"))?;
        let record = tasks
            .get_mut(task_id)
            .ok_or_else(|| anyhow::anyhow!("task '{}' not found", task_id))?;

        record.assigned_teammate = Some(teammate.to_string());
        record.updated_at_unix_ms = now_unix_ms();
        Ok(())
    }

    pub fn get(&self, task_id: &str) -> Option<TaskRecord> {
        let tasks = self.tasks.lock().ok()?;
        tasks.get(task_id).cloned()
    }

    pub fn all_tasks(&self) -> Result<Vec<TaskRecord>> {
        let tasks = self
            .tasks
            .lock()
            .map_err(|_| anyhow::anyhow!("task registry lock poisoned"))?;
        let mut records = tasks.values().cloned().collect::<Vec<_>>();
        records.sort_by_key(|r| r.created_at_unix_ms);
        Ok(records)
    }

    pub fn list_tasks(
        &self,
        status_filter: Option<&[TaskStatus]>,
        limit: Option<usize>,
    ) -> Result<Vec<TaskRecord>> {
        let mut records = self.all_tasks()?;

        if let Some(filters) = status_filter {
            records.retain(|record| filters.contains(&record.status));
        }

        if let Some(max_items) = limit {
            records.truncate(max_items);
        }

        Ok(records)
    }

    pub fn has_task(&self, task_id: &str) -> bool {
        let tasks = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        tasks.contains_key(task_id)
    }

    fn update_task(
        &self,
        task_id: &str,
        status: TaskStatus,
        result_preview: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| anyhow::anyhow!("task registry lock poisoned"))?;
        let record = tasks
            .get_mut(task_id)
            .ok_or_else(|| anyhow::anyhow!("task '{}' not found", task_id))?;

        record.status = status;
        record.result_preview = result_preview.map(|v| v.to_string());
        record.error = error.map(|v| v.to_string());
        record.updated_at_unix_ms = now_unix_ms();
        Ok(())
    }

    fn generate_task_id(&self) -> String {
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("task-{}", seq)
    }
}

fn normalize_task_id(raw: &str) -> Result<String> {
    let id = raw.trim();
    if id.is_empty() {
        bail!("task_id cannot be empty");
    }
    if id.len() > 64 {
        bail!("task_id too long (max 64 chars)");
    }

    let valid = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        bail!("task_id can only contain letters, digits, '_' or '-'");
    }

    Ok(id.to_string())
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
