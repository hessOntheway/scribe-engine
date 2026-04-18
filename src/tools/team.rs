use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::runtime::AgentLoop;

use super::task_registry::TaskStatus;
use super::{GlobalToolRegistry, TaskRegistry, ToolDefinition, ToolExecutor, ToolHandler};

const TEAM_LOOP_IDLE_MS: u64 = 400;
const TASK_RESULT_PREVIEW_CHARS: usize = 2_000;
const TEAM_TODO_COORDINATION_CONTRACT: &str = "Team planning contract: for multi-step work, maintain a single shared todo_write plan with exactly one in_progress item. Lead owns final todo_write updates; teammates should propose plan changes back to lead unless explicitly asked to edit the plan directly.";
const TEAM_ANALYSIS_CONTRACT: &str = "Team analysis contract: teammates may analyze single-repo or cross-repo systems, including microservice interactions. Use concrete evidence from available code and docs. If asked for teaching output, return tutorial-style, step-by-step explanations. If architecture or flow is requested, return diagram-ready structure (for example Mermaid) and explicitly note missing repositories or interfaces instead of guessing.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemberStatus {
    Working,
    Idle,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TeamMember {
    name: String,
    role: String,
    #[serde(default)]
    task_id: Option<String>,
    status: MemberStatus,
    created_at_unix_ms: u128,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TeamState {
    members: Vec<TeamMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TeamMessage {
    #[serde(rename = "type")]
    msg_type: String,
    from: String,
    content: String,
    timestamp_unix_ms: u128,
}

#[derive(Debug)]
struct WorkerHandle {
    shutdown: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct TeamManager {
    team_dir: PathBuf,
    config_path: PathBuf,
    inbox_dir: PathBuf,
    io_lock: Mutex<()>,
    workers: Mutex<HashMap<String, WorkerHandle>>,
}

impl TeamManager {
    pub fn new(team_dir: impl Into<PathBuf>) -> Result<Self> {
        let team_dir = team_dir.into();
        let inbox_dir = team_dir.join("inbox");
        let config_path = team_dir.join("config.json");

        fs::create_dir_all(&inbox_dir).with_context(|| {
            format!(
                "failed to create agent-team inbox directory: {}",
                inbox_dir.display()
            )
        })?;

        if !config_path.exists() {
            let state = TeamState::default();
            let config_text =
                serde_json::to_string_pretty(&state).context("failed to encode team config")?;
            fs::write(&config_path, config_text).with_context(|| {
                format!("failed to initialize team config: {}", config_path.display())
            })?;
        }

        Ok(Self {
            team_dir,
            config_path,
            inbox_dir,
            io_lock: Mutex::new(()),
            workers: Mutex::new(HashMap::new()),
        })
    }

    pub fn status_json(&self, task_registry: Option<&TaskRegistry>) -> Result<String> {
        let state = self.load_state()?;
        let active_workers = self.active_worker_names()?;
        let mut running = 0usize;
        let mut completed = 0usize;
        let mut failed = 0usize;
        let mut unavailable = 0usize;
        let mut todo_projection = Vec::new();
        let mut active_projection_assigned = false;
        let mut task_snapshots = Vec::new();

        for member in &state.members {
            let snapshot = task_registry
                .map(|registry| task_snapshot_value(registry, member.task_id.as_deref()))
                .unwrap_or_else(|| json!({"available": false, "reason": "task registry unavailable"}));

            if snapshot
                .get("available")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                match snapshot.get("status").and_then(|v| v.as_str()).unwrap_or("") {
                    "running" => running += 1,
                    "completed" => completed += 1,
                    "failed" => failed += 1,
                    _ => unavailable += 1,
                }
            } else {
                unavailable += 1;
            }

            if let Some(task_id) = member.task_id.as_deref() {
                let status = snapshot.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
                let (todo_status, active_hint) = match status {
                    "running" if !active_projection_assigned => {
                        active_projection_assigned = true;
                        ("in_progress", true)
                    }
                    "running" => ("pending", false),
                    "completed" => ("completed", false),
                    "failed" => ("pending", false),
                    _ => ("pending", false),
                };

                todo_projection.push(json!({
                    "id": task_id,
                    "content": format!("teammate '{}' task", member.name),
                    "status": todo_status,
                    "source": {
                        "teammate": member.name,
                        "task_id": task_id,
                        "task_status": status,
                        "active": active_hint,
                    }
                }));
            }

            task_snapshots.push(json!({
                "teammate": member.name,
                "task_id": member.task_id,
                "snapshot": snapshot,
            }));
        }

        serde_json::to_string_pretty(&json!({
            "team_dir": self.team_dir,
            "members": state.members,
            "active_workers": active_workers,
            "inbox_dir": self.inbox_dir,
            "task_snapshots": task_snapshots,
            "task_snapshot_summary": {
                "running": running,
                "completed": completed,
                "failed": failed,
                "unavailable": unavailable,
            },
            "todo_projection": todo_projection,
        }))
        .context("failed to encode team status")
    }

    fn active_worker_names(&self) -> Result<Vec<String>> {
        let workers = self
            .workers
            .lock()
            .map_err(|_| anyhow::anyhow!("worker mutex poisoned"))?;
        Ok(workers.keys().cloned().collect())
    }

    pub fn spawn_teammate(
        self: &Arc<Self>,
        name: &str,
        role: &str,
        prompt: &str,
        task_id: Option<&str>,
        validation_mode: TaskBindingMode,
        agent_loop: Arc<AgentLoop>,
        child_registry: Arc<GlobalToolRegistry>,
        task_registry: Arc<TaskRegistry>,
    ) -> Result<String> {
        let name = normalize_member_name(name)?;
        let role = role.trim();
        if role.is_empty() {
            bail!("teammate role cannot be empty");
        }

        let (bound_task_id, binding_note) =
            resolve_task_binding(task_registry.as_ref(), &name, task_id, validation_mode)?;

        {
            let mut workers = self
                .workers
                .lock()
                .map_err(|_| anyhow::anyhow!("worker mutex poisoned"))?;
            if workers.contains_key(&name) {
                bail!("teammate '{}' already exists", name);
            }

            let mut state = self.load_state()?;
            if state.members.iter().any(|m| m.name == name) {
                bail!("teammate '{}' already exists", name);
            }

            state.members.push(TeamMember {
                name: name.clone(),
                role: role.to_string(),
                task_id: bound_task_id.clone(),
                status: MemberStatus::Working,
                created_at_unix_ms: now_unix_ms(),
            });
            self.save_state(&state)?;

            let shutdown = Arc::new(AtomicBool::new(false));
            workers.insert(
                name.clone(),
                WorkerHandle {
                    shutdown: Arc::clone(&shutdown),
                },
            );

            let manager = Arc::clone(self);
            let worker_name = name.clone();
            let worker_role = role.to_string();
            let worker_prompt = prompt.trim().to_string();
            let worker_task_id = bound_task_id.clone();

            thread::spawn(move || {
                if let Err(err) = manager.worker_loop(
                    &worker_name,
                    &worker_role,
                    &worker_prompt,
                    worker_task_id.as_deref(),
                    agent_loop,
                    child_registry,
                    task_registry,
                    shutdown,
                ) {
                    let _ = manager.update_member_status(&worker_name, MemberStatus::Idle);
                    let _ = manager.send("system", "lead", &format!(
                        "teammate '{}' worker loop failed: {}",
                        worker_name, err
                    ));
                }
                if let Ok(mut workers) = manager.workers.lock() {
                    workers.remove(&worker_name);
                }
            });
        }

        let mut response = format!("spawned teammate '{}' with role '{}'", name, role);
        if let Some(task_id) = bound_task_id {
            response.push_str(&format!(" (bound to task '{}')", task_id));
        }
        if let Some(note) = binding_note {
            response.push_str(&format!("; {}", note));
        }

        Ok(response)
    }

    pub fn shutdown_teammate(&self, name: &str) -> Result<String> {
        let name = normalize_member_name(name)?;
        let workers = self
            .workers
            .lock()
            .map_err(|_| anyhow::anyhow!("worker mutex poisoned"))?;
        let handle = workers
            .get(&name)
            .with_context(|| format!("teammate '{}' not running", name))?;
        handle.shutdown.store(true, Ordering::Relaxed);
        Ok(format!("shutdown signal sent to teammate '{}'", name))
    }

    pub fn send(&self, from: &str, to: &str, content: &str) -> Result<String> {
        let from = normalize_member_name(from)?;
        let to = normalize_member_name(to)?;
        let content = content.trim();
        if content.is_empty() {
            bail!("message content cannot be empty");
        }

        let msg = TeamMessage {
            msg_type: "message".to_string(),
            from,
            content: content.to_string(),
            timestamp_unix_ms: now_unix_ms(),
        };
        self.append_message(&to, &msg)?;
        Ok(format!("sent message to '{}'", to))
    }

    pub fn broadcast(&self, from: &str, content: &str) -> Result<String> {
        let from = normalize_member_name(from)?;
        let content = content.trim();
        if content.is_empty() {
            bail!("message content cannot be empty");
        }

        let state = self.load_state()?;
        let mut sent = 0usize;
        for member in state.members {
            let msg = TeamMessage {
                msg_type: "message".to_string(),
                from: from.clone(),
                content: content.to_string(),
                timestamp_unix_ms: now_unix_ms(),
            };
            self.append_message(&member.name, &msg)?;
            sent += 1;
        }

        Ok(format!("broadcast message sent to {} teammate(s)", sent))
    }

    pub fn read_inbox_json(&self, name: &str) -> Result<String> {
        let name = normalize_member_name(name)?;
        let messages = self.drain_inbox(&name)?;
        serde_json::to_string_pretty(&messages).context("failed to encode inbox messages")
    }

    fn team_huddle_json(
        self: &Arc<Self>,
        members: &[TeamHuddleMember],
        agent_loop: Arc<AgentLoop>,
        child_registry: Arc<GlobalToolRegistry>,
        task_registry: Arc<TaskRegistry>,
        wait_ms: u64,
    ) -> Result<String> {
        let mut spawned = Vec::new();
        for member in members {
            let result = self.spawn_teammate(
                &member.name,
                &member.role,
                &member.prompt,
                member.task_id.as_deref(),
                member.validation_mode,
                Arc::clone(&agent_loop),
                Arc::clone(&child_registry),
                Arc::clone(&task_registry),
            )?;
            spawned.push(result);
        }

        self.wait_for_members_idle(
            &members.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
            wait_ms,
        )?;

        let lead_messages = self.drain_inbox("lead")?;
        let status = self.status_json(Some(task_registry.as_ref()))?;

        serde_json::to_string_pretty(&json!({
            "spawned": spawned,
            "lead_messages": lead_messages,
            "status": serde_json::from_str::<serde_json::Value>(&status).unwrap_or_else(|_| json!({"status": "unavailable"}))
        }))
        .context("failed to encode team huddle result")
    }

    fn worker_loop(
        &self,
        name: &str,
        role: &str,
        initial_prompt: &str,
        task_id: Option<&str>,
        agent_loop: Arc<AgentLoop>,
        child_registry: Arc<GlobalToolRegistry>,
        task_registry: Arc<TaskRegistry>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()> {
        self.send(
            "system",
            "lead",
            &format!("teammate '{}' is online (role: {})", name, role),
        )?;

        if !initial_prompt.trim().is_empty() {
            let task_context = render_task_context(task_registry.as_ref(), task_id);
            let prompt = format!(
                "{}\n{}\n\n{}\n\nYou are teammate '{}' with role '{}'. Initial assignment:\n{}",
                TEAM_TODO_COORDINATION_CONTRACT,
                TEAM_ANALYSIS_CONTRACT,
                task_context,
                name,
                role,
                initial_prompt.trim()
            );
            let output = match agent_loop.run_subagent(&prompt, child_registry.as_ref()) {
                Ok(text) => text,
                Err(err) => {
                    if let Some(task_id) = task_id {
                        let _ = task_registry.fail_task(task_id, &err.to_string());
                    }
                    return Err(err)
                        .with_context(|| format!("initial assignment failed for teammate '{}'", name));
                }
            };

            if let Some(task_id) = task_id {
                let preview = truncate_preview(&output);
                task_registry
                    .complete_task(task_id, &preview)
                    .with_context(|| format!("failed to mark task '{}' completed", task_id))?;
            }
            self.send(name, "lead", &output)?;
        }

        self.update_member_status(name, MemberStatus::Idle)?;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                self.update_member_status(name, MemberStatus::Shutdown)?;
                self.send(
                    "system",
                    "lead",
                    &format!("teammate '{}' has shut down", name),
                )?;
                break;
            }

            let messages = self.drain_inbox(name)?;
            if messages.is_empty() {
                thread::sleep(Duration::from_millis(TEAM_LOOP_IDLE_MS));
                continue;
            }

            self.update_member_status(name, MemberStatus::Working)?;

            let mut inbox_text = String::new();
            for msg in messages {
                inbox_text.push_str(&format!("from {}: {}\n", msg.from, msg.content));
            }

            let task_context = render_task_context(task_registry.as_ref(), task_id);
            let prompt = format!(
                "{}\n{}\n\n{}\n\nYou are teammate '{}' with role '{}'. Handle the inbox items below and return a concise, actionable response.\n\n{}",
                TEAM_TODO_COORDINATION_CONTRACT,
                TEAM_ANALYSIS_CONTRACT,
                task_context,
                name,
                role,
                inbox_text
            );

            let output = match agent_loop.run_subagent(&prompt, child_registry.as_ref()) {
                Ok(text) => {
                    if let Some(task_id) = task_id {
                        let preview = truncate_preview(&text);
                        let _ = task_registry.complete_task(task_id, &preview);
                    }
                    text
                }
                Err(err) => {
                    if let Some(task_id) = task_id {
                        let _ = task_registry.fail_task(task_id, &err.to_string());
                    }
                    format!("teammate '{}' failed to handle inbox: {}", name, err)
                }
            };

            self.send(name, "lead", &output)?;
            self.update_member_status(name, MemberStatus::Idle)?;
        }

        Ok(())
    }

    fn update_member_status(&self, name: &str, status: MemberStatus) -> Result<()> {
        let mut state = self.load_state()?;
        let member = state
            .members
            .iter_mut()
            .find(|m| m.name == name)
            .with_context(|| format!("teammate '{}' not found", name))?;
        member.status = status;
        self.save_state(&state)
    }

    fn wait_for_members_idle(&self, names: &[String], wait_ms: u64) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }

        let deadline = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
            + u128::from(wait_ms);

        loop {
            let state = self.load_state()?;
            let all_settled = names.iter().all(|name| {
                state
                    .members
                    .iter()
                    .find(|member| &member.name == name)
                    .map(|member| matches!(member.status, MemberStatus::Idle | MemberStatus::Shutdown))
                    .unwrap_or(false)
            });

            if all_settled {
                return Ok(());
            }

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if now >= deadline {
                return Ok(());
            }

            thread::sleep(Duration::from_millis(100));
        }
    }

    fn append_message(&self, to: &str, msg: &TeamMessage) -> Result<()> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("team io mutex poisoned"))?;
        let inbox_file = self.inbox_dir.join(format!("{}.jsonl", to));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&inbox_file)
            .with_context(|| format!("failed to open inbox file: {}", inbox_file.display()))?;
        let line = serde_json::to_string(msg).context("failed to encode team message")?;
        writeln!(file, "{}", line).with_context(|| {
            format!(
                "failed to append message to inbox file: {}",
                inbox_file.display()
            )
        })?;
        Ok(())
    }

    fn drain_inbox(&self, name: &str) -> Result<Vec<TeamMessage>> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("team io mutex poisoned"))?;
        let inbox_file = self.inbox_dir.join(format!("{}.jsonl", name));
        if !inbox_file.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&inbox_file)
            .with_context(|| format!("failed to read inbox: {}", inbox_file.display()))?;
        fs::write(&inbox_file, "")
            .with_context(|| format!("failed to drain inbox: {}", inbox_file.display()))?;

        let mut messages = Vec::new();
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            let msg: TeamMessage =
                serde_json::from_str(line).context("failed to parse inbox jsonl line")?;
            messages.push(msg);
        }

        Ok(messages)
    }

    fn load_state(&self) -> Result<TeamState> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("team io mutex poisoned"))?;
        let text = fs::read_to_string(&self.config_path).with_context(|| {
            format!("failed to read team config: {}", self.config_path.display())
        })?;
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse team config: {}", self.config_path.display()))
    }

    fn save_state(&self, state: &TeamState) -> Result<()> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("team io mutex poisoned"))?;
        let text =
            serde_json::to_string_pretty(state).context("failed to encode team config state")?;
        fs::write(&self.config_path, text)
            .with_context(|| format!("failed to write team config: {}", self.config_path.display()))
    }
}

#[derive(Debug, Deserialize)]
struct SpawnInput {
    name: String,
    role: String,
    prompt: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default = "default_task_binding_mode")]
    validation_mode: TaskBindingMode,
}

#[derive(Debug, Deserialize)]
struct SendInput {
    to: String,
    content: String,
    #[serde(default = "default_sender")]
    from: String,
}

#[derive(Debug, Deserialize)]
struct BroadcastInput {
    content: String,
    #[serde(default = "default_sender")]
    from: String,
}

#[derive(Debug, Deserialize)]
struct ReadInboxInput {
    #[serde(default = "default_sender")]
    name: String,
}

#[derive(Debug, Deserialize)]
struct ShutdownInput {
    name: String,
}

#[derive(Debug, Deserialize)]
struct TeamHuddleMember {
    name: String,
    role: String,
    prompt: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default = "default_task_binding_mode")]
    validation_mode: TaskBindingMode,
}

#[derive(Debug, Deserialize)]
struct TeamHuddleInput {
    members: Vec<TeamHuddleMember>,
    #[serde(default = "default_huddle_wait_ms")]
    wait_ms: u64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskBindingMode {
    BestEffort,
    Strict,
}

fn default_sender() -> String {
    "lead".to_string()
}

fn default_huddle_wait_ms() -> u64 {
    5_000
}

fn default_task_binding_mode() -> TaskBindingMode {
    TaskBindingMode::BestEffort
}

pub fn team_tool_handlers(
    manager: Arc<TeamManager>,
    agent_loop: Arc<AgentLoop>,
    child_registry: Arc<GlobalToolRegistry>,
    task_registry: Arc<TaskRegistry>,
) -> Vec<ToolHandler> {
    vec![
        spawn_teammate_handler(
            Arc::clone(&manager),
            Arc::clone(&agent_loop),
            Arc::clone(&child_registry),
            Arc::clone(&task_registry),
        ),
        team_huddle_handler(
            Arc::clone(&manager),
            Arc::clone(&agent_loop),
            Arc::clone(&child_registry),
            Arc::clone(&task_registry),
        ),
        send_teammate_message_handler(Arc::clone(&manager)),
        broadcast_teammate_message_handler(Arc::clone(&manager)),
        read_teammate_inbox_handler(Arc::clone(&manager)),
        team_status_handler(Arc::clone(&manager), Arc::clone(&task_registry)),
        shutdown_teammate_handler(manager),
    ]
}

fn spawn_teammate_handler(
    manager: Arc<TeamManager>,
    agent_loop: Arc<AgentLoop>,
    child_registry: Arc<GlobalToolRegistry>,
    task_registry: Arc<TaskRegistry>,
) -> ToolHandler {
    let definition = ToolDefinition {
        name: "spawn_teammate".to_string(),
        description: "Spawn a persistent teammate worker with a role and an initial assignment. The teammate keeps listening on its inbox and reports back to lead.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Unique teammate name, letters/digits/_/- only."},
                "role": {"type": "string", "description": "Teammate role, such as coder/tester/researcher."},
                "prompt": {"type": "string", "description": "Initial assignment for the teammate."},
                "task_id": {"type": "string", "description": "Optional task id to bind this teammate to."},
                "validation_mode": {"type": "string", "enum": ["best_effort", "strict"], "description": "Binding validation behavior for task_id. Default: best_effort."}
            },
            "required": ["name", "role", "prompt"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: SpawnInput = serde_json::from_str(input_json)
            .context("invalid input JSON for spawn_teammate")?;
        manager.spawn_teammate(
            &input.name,
            &input.role,
            &input.prompt,
            input.task_id.as_deref(),
            input.validation_mode,
            Arc::clone(&agent_loop),
            Arc::clone(&child_registry),
            Arc::clone(&task_registry),
        )
    });

    ToolHandler::new(definition, execute)
}

fn send_teammate_message_handler(manager: Arc<TeamManager>) -> ToolHandler {
    let definition = ToolDefinition {
        name: "send_teammate_message".to_string(),
        description: "Send a direct inbox message to one teammate.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "Recipient teammate name."},
                "content": {"type": "string", "description": "Message content."},
                "from": {"type": "string", "description": "Sender name, default is lead."}
            },
            "required": ["to", "content"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: SendInput = serde_json::from_str(input_json)
            .context("invalid input JSON for send_teammate_message")?;
        manager.send(&input.from, &input.to, &input.content)
    });

    ToolHandler::new(definition, execute)
}

fn team_huddle_handler(
    manager: Arc<TeamManager>,
    agent_loop: Arc<AgentLoop>,
    child_registry: Arc<GlobalToolRegistry>,
    task_registry: Arc<TaskRegistry>,
) -> ToolHandler {
    let definition = ToolDefinition {
        name: "team_huddle".to_string(),
        description: "Spawn a small set of teammates, let them work in parallel, and return a structured lead summary. This is the simplest fan-out/fan-in team orchestration tool.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "members": {
                    "type": "array",
                    "description": "Teammates to spawn for this huddle.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "role": {"type": "string"},
                            "prompt": {"type": "string"},
                            "task_id": {"type": "string"},
                            "validation_mode": {"type": "string", "enum": ["best_effort", "strict"]}
                        },
                        "required": ["name", "role", "prompt"],
                        "additionalProperties": false
                    }
                },
                "wait_ms": {"type": "integer", "minimum": 0, "description": "How long to wait for teammate replies before returning."}
            },
            "required": ["members"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: TeamHuddleInput = serde_json::from_str(input_json)
            .context("invalid input JSON for team_huddle")?;
        if input.members.is_empty() {
            bail!("team_huddle requires at least one member");
        }

        manager.team_huddle_json(
            &input.members,
            Arc::clone(&agent_loop),
            Arc::clone(&child_registry),
            Arc::clone(&task_registry),
            input.wait_ms,
        )
    });

    ToolHandler::new(definition, execute)
}

fn broadcast_teammate_message_handler(manager: Arc<TeamManager>) -> ToolHandler {
    let definition = ToolDefinition {
        name: "broadcast_teammate_message".to_string(),
        description: "Broadcast a message to all teammates currently in the team roster.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "Message content."},
                "from": {"type": "string", "description": "Sender name, default is lead."}
            },
            "required": ["content"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: BroadcastInput = serde_json::from_str(input_json)
            .context("invalid input JSON for broadcast_teammate_message")?;
        manager.broadcast(&input.from, &input.content)
    });

    ToolHandler::new(definition, execute)
}

fn read_teammate_inbox_handler(manager: Arc<TeamManager>) -> ToolHandler {
    let definition = ToolDefinition {
        name: "read_teammate_inbox".to_string(),
        description: "Read and drain one inbox in JSON format. Defaults to lead inbox.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Inbox owner name. Default: lead."}
            },
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: ReadInboxInput = serde_json::from_str(input_json)
            .context("invalid input JSON for read_teammate_inbox")?;
        manager.read_inbox_json(&input.name)
    });

    ToolHandler::new(definition, execute)
}

fn team_status_handler(manager: Arc<TeamManager>, task_registry: Arc<TaskRegistry>) -> ToolHandler {
    let definition = ToolDefinition {
        name: "team_status".to_string(),
        description: "Show team roster and current teammate statuses.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let empty: serde_json::Value = serde_json::from_str(input_json)
            .context("invalid input JSON for team_status")?;
        if !empty.is_object() {
            bail!("team_status expects an object payload");
        }
        manager.status_json(Some(task_registry.as_ref()))
    });

    ToolHandler::new(definition, execute)
}

fn shutdown_teammate_handler(manager: Arc<TeamManager>) -> ToolHandler {
    let definition = ToolDefinition {
        name: "shutdown_teammate".to_string(),
        description: "Gracefully stop a running teammate worker.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Teammate name."}
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = Arc::new(move |input_json: &str| {
        let input: ShutdownInput = serde_json::from_str(input_json)
            .context("invalid input JSON for shutdown_teammate")?;
        manager.shutdown_teammate(&input.name)
    });

    ToolHandler::new(definition, execute)
}

fn normalize_member_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        bail!("member name cannot be empty");
    }
    if name.len() > 64 {
        bail!("member name too long (max 64 chars)");
    }

    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        bail!("member name can only contain letters, digits, '_' or '-'");
    }
    Ok(name.to_string())
}

fn resolve_task_binding(
    task_registry: &TaskRegistry,
    teammate_name: &str,
    task_id: Option<&str>,
    mode: TaskBindingMode,
) -> Result<(Option<String>, Option<String>)> {
    let Some(raw_task_id) = task_id else {
        return Ok((None, None));
    };

    let normalized_task_id = raw_task_id.trim();
    if normalized_task_id.is_empty() {
        bail!("task_id cannot be empty when provided");
    }

    if task_registry.has_task(normalized_task_id) {
        task_registry.bind_teammate(normalized_task_id, teammate_name)?;
        return Ok((Some(normalized_task_id.to_string()), None));
    }

    match mode {
        TaskBindingMode::Strict => {
            bail!(
                "task_id '{}' not found (validation_mode=strict)",
                normalized_task_id
            )
        }
        TaskBindingMode::BestEffort => Ok((
            None,
            Some(format!(
                "task_id '{}' not found; continuing without task binding",
                normalized_task_id
            )),
        )),
    }
}

fn render_task_context(task_registry: &TaskRegistry, task_id: Option<&str>) -> String {
    let Some(id) = task_id else {
        return "No task binding for this teammate.".to_string();
    };

    let Some(task) = task_registry.get(id) else {
        return format!("Task binding requested ('{}'), but task is not currently registered.", id);
    };

    format!(
        "Task context: id='{}', status='{:?}', assigned_teammate='{}', prompt='{}'",
        task.id,
        task.status,
        task.assigned_teammate.unwrap_or_else(|| "<none>".to_string()),
        task.prompt
    )
}

fn task_snapshot_value(task_registry: &TaskRegistry, task_id: Option<&str>) -> serde_json::Value {
    let Some(id) = task_id else {
        return json!({
            "available": false,
            "reason": "no task bound",
        });
    };

    let Some(task) = task_registry.get(id) else {
        return json!({
            "available": false,
            "reason": "task not found",
            "task_id": id,
        });
    };

    json!({
        "available": true,
        "task_id": task.id,
        "status": task_status_str(task.status),
        "assigned_teammate": task.assigned_teammate,
        "result_preview": task.result_preview,
        "error": task.error,
        "updated_at_unix_ms": task.updated_at_unix_ms,
        "is_terminal": matches!(task.status, TaskStatus::Completed | TaskStatus::Failed),
    })
}

fn task_status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "running",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
    }
}

fn truncate_preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= TASK_RESULT_PREVIEW_CHARS {
        return trimmed.to_string();
    }

    trimmed.chars().take(TASK_RESULT_PREVIEW_CHARS).collect()
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}