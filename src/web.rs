use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task;
use tower_http::services::{ServeDir, ServeFile};

use crate::ask::AskApp;
use crate::llm::session::{ConversationSession, ConversationSessionSnapshot};
use crate::runtime::{RuntimeEvent, RuntimeEventSink};

static TURN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct WebState {
    live: Arc<LiveConversationManager>,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct PromptBody {
    prompt: String,
}

#[derive(Debug, Deserialize)]
struct SelectSessionBody {
    session_id: String,
}

#[derive(Debug, Clone, Copy)]
enum LiveStatus {
    Idle,
    Thinking,
    WaitingForInput,
    Error,
}

impl LiveStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Thinking => "thinking",
            Self::WaitingForInput => "waiting_for_input",
            Self::Error => "error",
        }
    }

    fn is_busy(self) -> bool {
        matches!(self, Self::Thinking)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LiveSnapshot {
    title: String,
    session_id: Option<String>,
    status: String,
    last_error: Option<String>,
    messages: Vec<UiMessage>,
}

#[derive(Debug, Clone, Serialize)]
struct LiveSubmitResponse {
    title: String,
    session_id: Option<String>,
    status: String,
    last_error: Option<String>,
    new_messages: Vec<UiMessage>,
    total_message_count: usize,
}

#[derive(Debug, Clone, Serialize)]
struct SessionListItem {
    session_id: String,
    title: String,
    created_at_unix_ms: u128,
    updated_at_unix_ms: u128,
    prompt_count: usize,
    message_count: usize,
    is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UiMessage {
    id: String,
    role: String,
    kind: String,
    content: String,
    created_at: u128,
    turn_id: String,
    tool_name: Option<String>,
    tool_args: Option<String>,
    tool_output: Option<String>,
    render_blocks: Vec<UiRenderBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UiRenderBlock {
    #[serde(rename = "type")]
    block_type: String,
    content: String,
}

#[derive(Debug, Clone)]
struct PendingUiMessage {
    role: String,
    kind: String,
    content: String,
    created_at: u128,
    tool_name: Option<String>,
    tool_args: Option<String>,
    tool_output: Option<String>,
}

struct LiveConversationInner {
    session: Option<ConversationSession>,
    title: String,
    session_id: Option<String>,
    ui_messages: Vec<UiMessage>,
    next_message_index: usize,
    status: LiveStatus,
    last_error: Option<String>,
}

struct LiveConversationManager {
    ask_app: AskApp,
    snapshot_path: PathBuf,
    inner: Arc<Mutex<LiveConversationInner>>,
}

impl LiveConversationManager {
    fn bootstrap(ask_app: AskApp) -> Result<Self> {
        let snapshot_path = ask_app.live_ui_snapshot_path();
        let persisted_snapshot = load_persisted_snapshot(&snapshot_path)?;
        let session = match persisted_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.session_id.as_deref())
        {
            Some(session_id) => ask_app.load_session(session_id).ok(),
            None => None,
        }
        .or(ask_app.load_latest_session()?);

        let (title, session_id, ui_messages, status, last_error) =
            match (session.as_ref(), persisted_snapshot.as_ref()) {
                (Some(session), Some(persisted))
                    if persisted.session_id.as_deref()
                        == Some(session.snapshot().session_id.as_str()) =>
                {
                    (
                        persisted.title.clone(),
                        persisted.session_id.clone(),
                        persisted.messages.clone(),
                        status_from_str(&persisted.status),
                        persisted.last_error.clone(),
                    )
                }
                (Some(session), _) => session_state_from_snapshot(session.snapshot()),
                (None, Some(persisted)) if persisted.session_id.is_none() => (
                    persisted.title.clone(),
                    None,
                    persisted.messages.clone(),
                    status_from_str(&persisted.status),
                    persisted.last_error.clone(),
                ),
                (None, _) => (
                    "Live conversation".to_string(),
                    None,
                    Vec::new(),
                    LiveStatus::Idle,
                    None,
                ),
            };

        let manager = Self {
            ask_app,
            snapshot_path,
            inner: Arc::new(Mutex::new(LiveConversationInner {
                session,
                title,
                session_id,
                next_message_index: ui_messages.len(),
                ui_messages,
                status,
                last_error,
            })),
        };

        manager.persist_snapshot()?;
        Ok(manager)
    }

    fn snapshot(&self) -> Result<LiveSnapshot> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("failed to lock live conversation state"))?;
        Ok(self.snapshot_from_inner(&inner))
    }

    fn persist_snapshot(&self) -> Result<()> {
        let snapshot = self.snapshot()?;
        save_persisted_snapshot(&self.snapshot_path, &snapshot)
    }

    async fn submit_user_message(
        self: &Arc<Self>,
        prompt: String,
    ) -> Result<LiveSubmitResponse, AppError> {
        let turn_id = TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let manager = Arc::clone(self);
        task::spawn_blocking(move || manager.run_turn_blocking(prompt, turn_id))
            .await
            .map_err(|err| AppError::internal(anyhow!(err.to_string())))?
    }

    fn list_sessions(&self) -> Result<Vec<SessionListItem>, AppError> {
        let active_session_id = {
            let inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;
            inner.session_id.clone()
        };

        let sessions = self
            .ask_app
            .list_session_snapshots()
            .map_err(AppError::internal)?
            .into_iter()
            .map(|snapshot| SessionListItem {
                is_active: active_session_id.as_deref() == Some(snapshot.session_id.as_str()),
                title: summary_title(&snapshot),
                prompt_count: snapshot.prompt_history.len(),
                message_count: snapshot.messages.len(),
                created_at_unix_ms: snapshot.created_at_unix_ms,
                updated_at_unix_ms: snapshot.updated_at_unix_ms,
                session_id: snapshot.session_id,
            })
            .collect();

        Ok(sessions)
    }

    fn select_session(&self, session_id: String) -> Result<LiveSnapshot, AppError> {
        let session = self
            .ask_app
            .load_session(&session_id)
            .map_err(AppError::internal)?;
        let snapshot = session.snapshot().clone();
        let (title, current_session_id, ui_messages, status, last_error) =
            session_state_from_snapshot(&snapshot);

        let persisted = {
            let mut inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;

            if inner.status.is_busy() {
                return Err(AppError::conflict(
                    "agent is still working on the current turn",
                ));
            }

            inner.session = Some(session);
            inner.title = title;
            inner.session_id = current_session_id;
            inner.next_message_index = ui_messages.len();
            inner.ui_messages = ui_messages;
            inner.status = status;
            inner.last_error = last_error;
            self.snapshot_from_inner(&inner)
        };

        save_persisted_snapshot(&self.snapshot_path, &persisted).map_err(AppError::internal)?;
        Ok(persisted)
    }

    fn reset_to_new_session(&self) -> Result<LiveSnapshot, AppError> {
        {
            let inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;

            if inner.status.is_busy() {
                return Err(AppError::conflict(
                    "agent is still working on the current turn",
                ));
            }
        }

        let session = self
            .ask_app
            .new_empty_session()
            .map_err(AppError::internal)?;
        let snapshot = session.snapshot().clone();
        let (title, current_session_id, ui_messages, status, last_error) =
            session_state_from_snapshot(&snapshot);

        let persisted = {
            let mut inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;

            inner.session = Some(session);
            inner.title = title;
            inner.session_id = current_session_id;
            inner.next_message_index = ui_messages.len();
            inner.ui_messages = ui_messages;
            inner.status = status;
            inner.last_error = last_error;
            self.snapshot_from_inner(&inner)
        };

        save_persisted_snapshot(&self.snapshot_path, &persisted).map_err(AppError::internal)?;
        Ok(persisted)
    }

    fn run_turn_blocking(
        self: Arc<Self>,
        prompt: String,
        turn_id: u64,
    ) -> Result<LiveSubmitResponse, AppError> {
        let mut new_messages = Vec::new();

        let (mut session, session_id) = {
            let mut inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;

            if inner.status.is_busy() {
                return Err(AppError::conflict(
                    "agent is still working on the current turn",
                ));
            }

            inner.status = LiveStatus::Thinking;
            inner.last_error = None;

            let session = match inner.session.take() {
                Some(mut session) => {
                    session.append_user_prompt(prompt.clone());
                    session.save().map_err(AppError::internal)?;
                    session
                }
                None => self
                    .ask_app
                    .new_session(prompt.clone())
                    .map_err(AppError::internal)?,
            };

            let snapshot = session.snapshot().clone();
            inner.title = summary_title(&snapshot);
            inner.session_id = Some(snapshot.session_id.clone());

            let user_message = base_ui_message(
                &snapshot.session_id,
                inner.next_message_index,
                "user",
                "user",
                prompt,
                now_unix_ms(),
                None,
                None,
                None,
                turn_id,
            );
            inner.next_message_index += 1;
            inner.ui_messages.push(user_message.clone());
            new_messages.push(user_message);

            let persisted = self.snapshot_from_inner(&inner);
            save_persisted_snapshot(&self.snapshot_path, &persisted).map_err(AppError::internal)?;

            (session, snapshot.session_id)
        };

        let pending_messages = Arc::new(Mutex::new(Vec::<PendingUiMessage>::new()));
        let collector = Arc::clone(&pending_messages);
        let observer: RuntimeEventSink = Arc::new(move |event| {
            if let Some(message) = pending_message_from_event(event) {
                if let Ok(mut messages) = collector.lock() {
                    messages.push(message);
                }
            }
        });

        let runtime_result = self
            .ask_app
            .run_session_turn_with_events(&mut session, Some(observer));

        let collected_messages = pending_messages
            .lock()
            .map(|messages| messages.clone())
            .unwrap_or_default();

        if let Err(error) = runtime_result {
            let _ = session.save();
            let error_text = error.to_string();
            return self.finish_turn(
                session,
                session_id,
                turn_id,
                new_messages,
                collected_messages,
                Some(error_text),
            );
        }

        self.finish_turn(
            session,
            session_id,
            turn_id,
            new_messages,
            collected_messages,
            None,
        )
    }

    fn finish_turn(
        &self,
        session: ConversationSession,
        session_id: String,
        turn_id: u64,
        mut new_messages: Vec<UiMessage>,
        collected_messages: Vec<PendingUiMessage>,
        error_text: Option<String>,
    ) -> Result<LiveSubmitResponse, AppError> {
        let snapshot = session.snapshot().clone();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| AppError::internal(anyhow!("failed to lock live conversation state")))?;

        inner.title = summary_title(&snapshot);
        inner.session_id = Some(snapshot.session_id.clone());

        for pending in collected_messages {
            let ui_message = base_ui_message(
                &session_id,
                inner.next_message_index,
                &pending.role,
                &pending.kind,
                pending.content,
                pending.created_at,
                pending.tool_name,
                pending.tool_args,
                pending.tool_output,
                turn_id,
            );
            inner.next_message_index += 1;
            inner.ui_messages.push(ui_message.clone());
            new_messages.push(ui_message);
        }

        inner.session = Some(session);
        if let Some(error_text) = error_text.clone() {
            inner.status = LiveStatus::Error;
            inner.last_error = Some(error_text);
        } else {
            inner.status = LiveStatus::WaitingForInput;
            inner.last_error = None;
        }

        let persisted = self.snapshot_from_inner(&inner);
        save_persisted_snapshot(&self.snapshot_path, &persisted).map_err(AppError::internal)?;

        Ok(LiveSubmitResponse {
            title: persisted.title,
            session_id: persisted.session_id,
            status: persisted.status,
            last_error: persisted.last_error,
            new_messages,
            total_message_count: inner.ui_messages.len(),
        })
    }

    fn snapshot_from_inner(&self, inner: &LiveConversationInner) -> LiveSnapshot {
        LiveSnapshot {
            title: inner.title.clone(),
            session_id: inner.session_id.clone(),
            status: inner.status.as_str().to_string(),
            last_error: inner.last_error.clone(),
            messages: inner.ui_messages.clone(),
        }
    }
}

pub async fn serve(ask_app: AskApp, host: String, port: u16) -> Result<()> {
    let live = Arc::new(LiveConversationManager::bootstrap(ask_app)?);
    let state = WebState { live };
    let static_dir = PathBuf::from("static");
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/sessions", get(list_sessions))
        .route("/api/live", get(get_live))
        .route("/api/live/session", post(select_live_session))
        .route("/api/live/session/new", post(post_new_live_session))
        .route("/api/live/messages", post(post_live_message))
        .fallback_service(
            ServeDir::new(&static_dir)
                .not_found_service(ServeFile::new(static_dir.join("index.html"))),
        )
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("invalid bind address: {host}:{port}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind web server on http://{host}:{port}"))?;

    println!("Scribe web UI running at http://{host}:{port}");
    axum::serve(listener, app)
        .await
        .context("web server exited unexpectedly")
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

async fn get_live(State(state): State<WebState>) -> Result<Json<LiveSnapshot>, AppError> {
    Ok(Json(state.live.snapshot().map_err(AppError::internal)?))
}

async fn list_sessions(
    State(state): State<WebState>,
) -> Result<Json<Vec<SessionListItem>>, AppError> {
    Ok(Json(state.live.list_sessions()?))
}

async fn select_live_session(
    State(state): State<WebState>,
    Json(body): Json<SelectSessionBody>,
) -> Result<Json<LiveSnapshot>, AppError> {
    let session_id = body.session_id.trim().to_string();
    if session_id.is_empty() {
        return Err(AppError::bad_request("session_id must not be empty"));
    }

    Ok(Json(state.live.select_session(session_id)?))
}

async fn post_new_live_session(
    State(state): State<WebState>,
) -> Result<Json<LiveSnapshot>, AppError> {
    Ok(Json(state.live.reset_to_new_session()?))
}

async fn post_live_message(
    State(state): State<WebState>,
    Json(body): Json<PromptBody>,
) -> Result<Json<LiveSubmitResponse>, AppError> {
    let prompt = non_empty_prompt(body.prompt)?;
    let response = state.live.submit_user_message(prompt).await?;
    Ok(Json(response))
}

fn load_persisted_snapshot(path: &Path) -> Result<Option<LiveSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read(path)
        .with_context(|| format!("failed to read live snapshot: {}", path.display()))?;
    let snapshot = serde_json::from_slice::<LiveSnapshot>(&content)
        .with_context(|| format!("failed to parse live snapshot: {}", path.display()))?;
    Ok(Some(snapshot))
}

fn save_persisted_snapshot(path: &Path, snapshot: &LiveSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create live snapshot dir: {}", parent.display()))?;
    }

    let content =
        serde_json::to_vec_pretty(snapshot).context("failed to serialize live snapshot")?;
    std::fs::write(path, content)
        .with_context(|| format!("failed to write live snapshot: {}", path.display()))
}

fn pending_message_from_event(event: RuntimeEvent) -> Option<PendingUiMessage> {
    match event {
        RuntimeEvent::AssistantMessage(message) => {
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if content.is_empty() {
                return None;
            }

            Some(PendingUiMessage {
                role: "assistant".to_string(),
                kind: "assistant".to_string(),
                content,
                created_at: now_unix_ms(),
                tool_name: None,
                tool_args: None,
                tool_output: None,
            })
        }
        RuntimeEvent::ToolCall {
            tool_call_id: _tool_call_id,
            name,
            arguments,
        } => Some(PendingUiMessage {
            role: "assistant".to_string(),
            kind: "tool_call".to_string(),
            content: String::new(),
            created_at: now_unix_ms(),
            tool_name: Some(name),
            tool_args: Some(arguments),
            tool_output: None,
        }),
        RuntimeEvent::ToolResult {
            tool_call_id: _tool_call_id,
            name,
            arguments,
            result,
        } => Some(PendingUiMessage {
            role: "tool".to_string(),
            kind: "tool_result".to_string(),
            content: String::new(),
            created_at: now_unix_ms(),
            tool_name: Some(name),
            tool_args: Some(arguments),
            tool_output: Some(result),
        }),
    }
}

fn flatten_messages(
    session_id: &str,
    messages: &[Value],
    offset: usize,
    created_at: u128,
) -> Vec<UiMessage> {
    let mut ui_messages = Vec::new();
    let mut tool_calls = std::collections::HashMap::<String, (String, String)>::new();
    let mut turn_number = 0u64;

    for (index, message) in messages.iter().enumerate() {
        let absolute_index = offset + index;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("assistant")
            .to_string();
        if role == "system" {
            continue;
        }
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if role == "user" {
            turn_number += 1;
            ui_messages.push(base_ui_message(
                session_id,
                absolute_index,
                "user",
                "user",
                content,
                created_at,
                None,
                None,
                None,
                turn_number,
            ));
            continue;
        }

        let effective_turn = if turn_number == 0 { 1 } else { turn_number };

        if role == "assistant" {
            if !content.trim().is_empty() {
                ui_messages.push(base_ui_message(
                    session_id,
                    absolute_index,
                    "assistant",
                    "assistant",
                    content.clone(),
                    created_at,
                    None,
                    None,
                    None,
                    effective_turn,
                ));
            }

            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                for (call_index, call) in calls.iter().enumerate() {
                    let tool_call_id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = call
                        .get("function")
                        .and_then(|v| v.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let arguments = call
                        .get("function")
                        .and_then(|v| v.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();

                    tool_calls.insert(tool_call_id, (name.clone(), arguments.clone()));
                    ui_messages.push(base_ui_message(
                        session_id,
                        absolute_index * 100 + call_index,
                        "assistant",
                        "tool_call",
                        String::new(),
                        created_at,
                        Some(name),
                        Some(arguments),
                        None,
                        effective_turn,
                    ));
                }
            }
            continue;
        }

        if role == "tool" {
            let tool_call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let (tool_name, tool_args) = tool_calls
                .get(tool_call_id)
                .cloned()
                .unwrap_or_else(|| ("unknown".to_string(), String::new()));
            ui_messages.push(base_ui_message(
                session_id,
                absolute_index,
                "tool",
                "tool_result",
                String::new(),
                created_at,
                Some(tool_name),
                Some(tool_args),
                Some(content),
                effective_turn,
            ));
            continue;
        }

        ui_messages.push(base_ui_message(
            session_id,
            absolute_index,
            &role,
            &role,
            content,
            created_at,
            None,
            None,
            None,
            effective_turn,
        ));
    }

    ui_messages
}

fn base_ui_message(
    session_id: &str,
    index: usize,
    role: &str,
    kind: &str,
    content: String,
    created_at: u128,
    tool_name: Option<String>,
    tool_args: Option<String>,
    tool_output: Option<String>,
    turn_id: u64,
) -> UiMessage {
    let text_for_blocks = if kind == "tool_result" {
        tool_output.clone().unwrap_or_default()
    } else if kind == "tool_call" {
        tool_args.clone().unwrap_or_default()
    } else {
        content.clone()
    };

    UiMessage {
        id: format!("{session_id}-{index}-{kind}"),
        role: role.to_string(),
        kind: kind.to_string(),
        content,
        created_at,
        turn_id: format!("{session_id}-turn-{turn_id}"),
        tool_name,
        tool_args,
        tool_output,
        render_blocks: parse_render_blocks(&text_for_blocks, kind),
    }
}

fn parse_render_blocks(content: &str, kind: &str) -> Vec<UiRenderBlock> {
    if kind == "tool_call" {
        return vec![UiRenderBlock {
            block_type: "tool_call".to_string(),
            content: content.to_string(),
        }];
    }

    if kind == "tool_result" {
        return vec![UiRenderBlock {
            block_type: "tool_result".to_string(),
            content: content.to_string(),
        }];
    }

    let mut blocks = Vec::new();
    let mut cursor = 0usize;

    while let Some(start) = content[cursor..].find("```") {
        let absolute_start = cursor + start;
        let before = content[cursor..absolute_start].trim();
        if !before.is_empty() {
            blocks.push(UiRenderBlock {
                block_type: "text".to_string(),
                content: before.to_string(),
            });
        }

        let after_ticks = absolute_start + 3;
        let rest = &content[after_ticks..];
        let Some(newline_pos) = rest.find('\n') else {
            break;
        };
        let language = rest[..newline_pos].trim().to_lowercase();
        let code_start = after_ticks + newline_pos + 1;
        let Some(end_rel) = content[code_start..].find("```") else {
            break;
        };
        let code = content[code_start..code_start + end_rel].trim();
        blocks.push(UiRenderBlock {
            block_type: if language == "mermaid" {
                "mermaid".to_string()
            } else {
                "code".to_string()
            },
            content: code.to_string(),
        });
        cursor = code_start + end_rel + 3;
    }

    let trailing = content[cursor..].trim();
    if !trailing.is_empty() {
        blocks.push(UiRenderBlock {
            block_type: "text".to_string(),
            content: trailing.to_string(),
        });
    }

    if blocks.is_empty() {
        blocks.push(UiRenderBlock {
            block_type: "text".to_string(),
            content: content.to_string(),
        });
    }

    blocks
}

fn summary_title(snapshot: &ConversationSessionSnapshot) -> String {
    snapshot
        .prompt_history
        .last()
        .cloned()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Live conversation".to_string())
        .chars()
        .take(36)
        .collect()
}

fn session_state_from_snapshot(
    snapshot: &ConversationSessionSnapshot,
) -> (
    String,
    Option<String>,
    Vec<UiMessage>,
    LiveStatus,
    Option<String>,
) {
    let ui_messages = flatten_messages(
        &snapshot.session_id,
        &snapshot.messages,
        0,
        snapshot.updated_at_unix_ms,
    );

    (
        summary_title(snapshot),
        Some(snapshot.session_id.clone()),
        ui_messages,
        LiveStatus::WaitingForInput,
        None,
    )
}

fn status_from_str(status: &str) -> LiveStatus {
    match status {
        "thinking" => LiveStatus::Thinking,
        "waiting_for_input" => LiveStatus::WaitingForInput,
        "error" => LiveStatus::Error,
        _ => LiveStatus::Idle,
    }
}

fn non_empty_prompt(prompt: String) -> Result<String, AppError> {
    let trimmed = prompt.trim().to_string();
    if trimmed.is_empty() {
        return Err(AppError::bad_request("prompt must not be empty"));
    }
    Ok(trimmed)
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    fn internal(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiError {
                error: self.message,
            }),
        )
            .into_response()
    }
}
