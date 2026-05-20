use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agents::AgentKind;
use anyhow::{Context, Result, anyhow};
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use crate::ask::AskApp;
use crate::llm::session::{ConversationSession, ConversationSessionSnapshot};
use crate::runtime::{CancellationToken, RuntimeEvent, RuntimeEventSink};

static TURN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct WebState {
    workflow: Arc<WorkflowManager>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InterviewStatus {
    NotStarted,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReportMeta {
    path: String,
    updated_at: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MaterialsMeta {
    exists: bool,
    path: String,
    updated_at: Option<u128>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowSnapshot {
    active_agent: AgentKind,
    available_agents: Vec<AgentInfo>,
    materials: MaterialsMeta,
    interview_status: InterviewStatus,
    interview_phase: String,
    materials_session_id: Option<String>,
    interview_session_id: Option<String>,
    report: Option<ReportMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentInfo {
    kind: AgentKind,
    id: String,
    title: String,
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
    #[serde(default = "crate::agents::default_agent_kind")]
    agent_kind: AgentKind,
    title: String,
    session_id: Option<String>,
    status: String,
    last_error: Option<String>,
    messages: Vec<UiMessage>,
}

#[derive(Debug, Clone, Serialize)]
struct LiveSubmitResponse {
    agent_kind: AgentKind,
    title: String,
    session_id: Option<String>,
    status: String,
    last_error: Option<String>,
    new_messages: Vec<UiMessage>,
    total_message_count: usize,
}

#[derive(Debug, Clone, Serialize)]
struct LiveEvent {
    #[serde(rename = "type")]
    event_type: String,
    agent_kind: AgentKind,
    snapshot: Option<LiveSnapshot>,
    response: Option<LiveSubmitResponse>,
    message: Option<UiMessage>,
    workflow: Option<WorkflowSnapshot>,
    error: Option<String>,
}

impl LiveEvent {
    fn snapshot(
        agent_kind: AgentKind,
        snapshot: LiveSnapshot,
        workflow: Option<WorkflowSnapshot>,
    ) -> Self {
        Self {
            event_type: "snapshot".to_string(),
            agent_kind,
            snapshot: Some(snapshot),
            response: None,
            message: None,
            workflow,
            error: None,
        }
    }

    fn turn_started(response: LiveSubmitResponse, workflow: Option<WorkflowSnapshot>) -> Self {
        Self {
            event_type: "turn_started".to_string(),
            agent_kind: response.agent_kind,
            snapshot: None,
            response: Some(response),
            message: None,
            workflow,
            error: None,
        }
    }

    fn message_added(agent_kind: AgentKind, message: UiMessage) -> Self {
        Self {
            event_type: "message_added".to_string(),
            agent_kind,
            snapshot: None,
            response: None,
            message: Some(message),
            workflow: None,
            error: None,
        }
    }

    fn trace_added(agent_kind: AgentKind, message: UiMessage) -> Self {
        Self {
            event_type: "trace_added".to_string(),
            agent_kind,
            snapshot: None,
            response: None,
            message: Some(message),
            workflow: None,
            error: None,
        }
    }

    fn turn_finished(response: LiveSubmitResponse, workflow: Option<WorkflowSnapshot>) -> Self {
        Self {
            event_type: "turn_finished".to_string(),
            agent_kind: response.agent_kind,
            snapshot: None,
            response: Some(response),
            message: None,
            workflow,
            error: None,
        }
    }

    fn turn_failed(response: LiveSubmitResponse, error: String) -> Self {
        Self {
            event_type: "turn_failed".to_string(),
            agent_kind: response.agent_kind,
            snapshot: None,
            response: Some(response),
            message: None,
            workflow: None,
            error: Some(error),
        }
    }

    fn turn_cancelled(
        agent_kind: AgentKind,
        snapshot: LiveSnapshot,
        workflow: Option<WorkflowSnapshot>,
    ) -> Self {
        Self {
            event_type: "turn_cancelled".to_string(),
            agent_kind,
            snapshot: Some(snapshot),
            response: None,
            message: None,
            workflow,
            error: None,
        }
    }
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
    interview_status: InterviewStatus,
    materials_exists: bool,
    report_exists: bool,
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

enum PendingLiveMessage {
    Visible(PendingUiMessage),
    Trace(PendingUiMessage),
}

type TurnCompletionHandler =
    Arc<dyn Fn(&LiveSubmitResponse) -> Option<WorkflowSnapshot> + Send + Sync>;

struct StartedTurn {
    session: ConversationSession,
    session_id: String,
    turn_id: u64,
    cancellation: CancellationToken,
    response: LiveSubmitResponse,
}

#[derive(Debug, Clone)]
struct ActiveTurn {
    session_id: String,
    turn_id: u64,
    cancellation: CancellationToken,
    rollback_message_count: usize,
    rollback_prompt_history_count: usize,
    rollback_ui_message_count: usize,
    rollback_next_message_index: usize,
}

struct LiveConversationInner {
    session: Option<ConversationSession>,
    title: String,
    session_id: Option<String>,
    ui_messages: Vec<UiMessage>,
    next_message_index: usize,
    next_trace_index: usize,
    status: LiveStatus,
    last_error: Option<String>,
    active_turn: Option<ActiveTurn>,
}

struct LiveConversationManager {
    agent_kind: AgentKind,
    ask_app: AskApp,
    snapshot_path: PathBuf,
    inner: Arc<Mutex<LiveConversationInner>>,
    event_tx: broadcast::Sender<LiveEvent>,
}

impl LiveConversationManager {
    fn bootstrap(agent_kind: AgentKind, ask_app: AskApp) -> Result<Self> {
        let snapshot_path = ask_app.live_ui_snapshot_path(agent_kind);
        let persisted_snapshot = load_persisted_snapshot(&snapshot_path)?;
        let session = match persisted_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.session_id.as_deref())
        {
            Some(session_id) => ask_app.load_session(agent_kind, session_id).ok(),
            None => None,
        }
        .or(ask_app.load_latest_session(agent_kind)?);

        let (title, session_id, ui_messages, status, last_error) =
            match (session.as_ref(), persisted_snapshot.as_ref()) {
                (Some(session), Some(persisted))
                    if persisted.session_id.as_deref()
                        == Some(session.snapshot().session_id.as_str()) =>
                {
                    let snapshot = session.snapshot();
                    let ui_snapshot = load_persisted_snapshot(
                        &ask_app.ui_session_snapshot_path(agent_kind, &snapshot.session_id),
                    )?;
                    let (_, _, fallback_messages, _, _) = session_state_from_snapshot(snapshot);
                    let messages = ui_snapshot
                        .as_ref()
                        .map(|snapshot| visible_messages_only(&snapshot.messages))
                        .unwrap_or(fallback_messages);
                    (
                        persisted.title.clone(),
                        persisted.session_id.clone(),
                        messages,
                        status_from_str(&persisted.status),
                        persisted.last_error.clone(),
                    )
                }
                (Some(session), _) => {
                    let snapshot = session.snapshot();
                    if let Some(ui_snapshot) = load_persisted_snapshot(
                        &ask_app.ui_session_snapshot_path(agent_kind, &snapshot.session_id),
                    )? {
                        (
                            ui_snapshot.title,
                            ui_snapshot.session_id,
                            visible_messages_only(&ui_snapshot.messages),
                            status_from_str(&ui_snapshot.status),
                            ui_snapshot.last_error,
                        )
                    } else {
                        session_state_from_snapshot(snapshot)
                    }
                }
                (None, Some(persisted)) if persisted.session_id.is_none() => (
                    persisted.title.clone(),
                    None,
                    visible_messages_only(&persisted.messages),
                    status_from_str(&persisted.status),
                    persisted.last_error.clone(),
                ),
                (None, _) => (
                    agent_kind.title().to_string(),
                    None,
                    Vec::new(),
                    LiveStatus::Idle,
                    None,
                ),
            };

        let manager = Self {
            agent_kind,
            ask_app,
            snapshot_path,
            inner: Arc::new(Mutex::new(LiveConversationInner {
                session,
                title,
                session_id,
                next_message_index: ui_messages.len(),
                next_trace_index: 0,
                ui_messages,
                status,
                last_error,
                active_turn: None,
            })),
            event_tx: broadcast::channel(512).0,
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
        self.persist_snapshot_value(&snapshot)
    }

    fn persist_snapshot_value(&self, snapshot: &LiveSnapshot) -> Result<()> {
        let mut snapshot = snapshot.clone();
        snapshot.messages = visible_messages_only(&snapshot.messages);
        save_persisted_snapshot(&self.snapshot_path, &snapshot)?;
        if let Some(session_id) = snapshot.session_id.as_deref() {
            save_persisted_snapshot(
                &self
                    .ask_app
                    .ui_session_snapshot_path(self.agent_kind, session_id),
                &snapshot,
            )?;
        }
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.event_tx.subscribe()
    }

    fn broadcast(&self, event: LiveEvent) {
        let _ = self.event_tx.send(event);
    }

    fn submit_user_message(
        self: &Arc<Self>,
        prompt: String,
        on_complete: Option<TurnCompletionHandler>,
    ) -> Result<LiveSubmitResponse, AppError> {
        let turn_id = TURN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let started = self.start_turn(prompt, turn_id)?;
        let response = started.response.clone();
        self.broadcast(LiveEvent::turn_started(response.clone(), None));
        let _ = self.append_trace_message(
            &started.session_id,
            started.turn_id,
            PendingUiMessage {
                role: "assistant".to_string(),
                kind: "assistant_trace".to_string(),
                content: "Preparing to analyze the request.".to_string(),
                created_at: now_unix_ms(),
                tool_name: None,
                tool_args: None,
                tool_output: None,
            },
        );

        let manager = Arc::clone(self);
        task::spawn_blocking(move || {
            if let Err(error) = manager.run_turn_background(started, on_complete) {
                eprintln!("error: failed to complete agent turn: {}", error.message);
            }
        });

        Ok(response)
    }

    fn is_busy(&self) -> Result<bool, AppError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| AppError::internal(anyhow!("failed to lock live conversation state")))?;
        Ok(inner.status.is_busy())
    }

    fn select_session(&self, session_id: &str) -> Result<Option<LiveSnapshot>, AppError> {
        if self.is_busy()? {
            return Err(AppError::conflict(
                "agent is still working on the current turn",
            ));
        }

        let session = match self.ask_app.load_session(self.agent_kind, session_id) {
            Ok(session) => session,
            Err(_) => {
                let persisted = {
                    let mut inner = self.inner.lock().map_err(|_| {
                        AppError::internal(anyhow!("failed to lock live conversation state"))
                    })?;
                    inner.session = None;
                    inner.title = self.agent_kind.title().to_string();
                    inner.session_id = None;
                    inner.next_message_index = 0;
                    inner.next_trace_index = 0;
                    inner.ui_messages = Vec::new();
                    inner.status = LiveStatus::Idle;
                    inner.last_error = None;
                    inner.active_turn = None;
                    self.snapshot_from_inner(&inner)
                };
                self.persist_snapshot_value(&persisted)
                    .map_err(AppError::internal)?;
                self.broadcast(LiveEvent::snapshot(self.agent_kind, persisted, None));
                return Ok(None);
            }
        };
        let snapshot = session.snapshot().clone();
        let (title, current_session_id, ui_messages, status, last_error) =
            if let Some(ui_snapshot) = load_persisted_snapshot(
                &self
                    .ask_app
                    .ui_session_snapshot_path(self.agent_kind, &snapshot.session_id),
            )
            .map_err(AppError::internal)?
            {
                (
                    ui_snapshot.title,
                    ui_snapshot.session_id,
                    visible_messages_only(&ui_snapshot.messages),
                    status_from_str(&ui_snapshot.status),
                    ui_snapshot.last_error,
                )
            } else {
                session_state_from_snapshot(&snapshot)
            };

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
            inner.next_trace_index = 0;
            inner.ui_messages = ui_messages;
            inner.status = status;
            inner.last_error = last_error;
            inner.active_turn = None;
            self.snapshot_from_inner(&inner)
        };

        self.persist_snapshot_value(&persisted)
            .map_err(AppError::internal)?;
        self.broadcast(LiveEvent::snapshot(
            self.agent_kind,
            persisted.clone(),
            None,
        ));
        Ok(Some(persisted))
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
            .new_empty_session(self.agent_kind)
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
            inner.next_trace_index = 0;
            inner.ui_messages = ui_messages;
            inner.status = status;
            inner.last_error = last_error;
            inner.active_turn = None;
            self.snapshot_from_inner(&inner)
        };

        self.persist_snapshot_value(&persisted)
            .map_err(AppError::internal)?;
        self.broadcast(LiveEvent::snapshot(
            self.agent_kind,
            persisted.clone(),
            None,
        ));
        Ok(persisted)
    }
    fn start_turn(&self, prompt: String, turn_id: u64) -> Result<StartedTurn, AppError> {
        let mut new_messages = Vec::new();
        let cancellation = CancellationToken::new();

        let (session, session_id) = {
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

            let mut session = match inner.session.take() {
                Some(session) => session,
                None => self
                    .ask_app
                    .new_empty_session(self.agent_kind)
                    .map_err(AppError::internal)?,
            };
            let rollback_message_count = session.snapshot().messages.len();
            let rollback_prompt_history_count = session.snapshot().prompt_history.len();
            let rollback_ui_message_count = inner.ui_messages.len();
            let rollback_next_message_index = inner.next_message_index;

            session.append_user_prompt(prompt.clone());
            session.save().map_err(AppError::internal)?;

            let snapshot = session.snapshot().clone();
            inner.title = summary_title(&snapshot);
            inner.session_id = Some(snapshot.session_id.clone());
            inner.active_turn = Some(ActiveTurn {
                session_id: snapshot.session_id.clone(),
                turn_id,
                cancellation: cancellation.clone(),
                rollback_message_count,
                rollback_prompt_history_count,
                rollback_ui_message_count,
                rollback_next_message_index,
            });

            if !is_internal_seed_message(&prompt) {
                let user_message = base_ui_message(UiMessageInput {
                    session_id: &snapshot.session_id,
                    index: inner.next_message_index,
                    role: "user",
                    kind: "user",
                    content: prompt,
                    created_at: now_unix_ms(),
                    tool_name: None,
                    tool_args: None,
                    tool_output: None,
                    turn_id,
                });
                inner.next_message_index += 1;
                inner.ui_messages.push(user_message.clone());
                new_messages.push(user_message);
            }

            let persisted = self.snapshot_from_inner(&inner);
            self.persist_snapshot_value(&persisted)
                .map_err(AppError::internal)?;

            (session, snapshot.session_id)
        };

        let total_message_count = self
            .inner
            .lock()
            .map(|inner| inner.ui_messages.len())
            .unwrap_or(new_messages.len());

        Ok(StartedTurn {
            session,
            session_id: session_id.clone(),
            turn_id,
            cancellation,
            response: LiveSubmitResponse {
                agent_kind: self.agent_kind,
                title: self
                    .snapshot()
                    .map(|snapshot| snapshot.title)
                    .unwrap_or_else(|_| self.agent_kind.title().to_string()),
                session_id: Some(session_id.clone()),
                status: LiveStatus::Thinking.as_str().to_string(),
                last_error: None,
                new_messages,
                total_message_count,
            },
        })
    }

    fn run_turn_background(
        self: Arc<Self>,
        mut started: StartedTurn,
        on_complete: Option<TurnCompletionHandler>,
    ) -> Result<(), AppError> {
        let event_manager = Arc::clone(&self);
        let event_session_id = started.session_id.clone();
        let event_turn_id = started.turn_id;
        let observer: RuntimeEventSink =
            Arc::new(move |event| {
                if let Some(message) = pending_message_from_event(event) {
                    let result =
                        match message {
                            PendingLiveMessage::Visible(message) => event_manager
                                .append_visible_message(&event_session_id, event_turn_id, message),
                            PendingLiveMessage::Trace(message) => event_manager
                                .append_trace_message(&event_session_id, event_turn_id, message),
                        };
                    if let Err(error) = result {
                        eprintln!(
                            "error: failed to append live runtime event: {}",
                            error.message
                        );
                    }
                }
            });

        let runtime_result = self.ask_app.run_session_turn_with_events(
            self.agent_kind,
            &mut started.session,
            Some(observer),
            Some(started.cancellation.clone()),
        );

        if let Err(error) = runtime_result {
            if started.cancellation.is_cancelled()
                || !self.is_active_turn(&started.session_id, started.turn_id)?
            {
                return Ok(());
            }
            let _ = started.session.save();
            let error_text = error.to_string();
            self.finish_turn(started, Some(error_text), None)?;
            return Ok(());
        }

        self.finish_turn(started, None, on_complete)?;
        Ok(())
    }

    fn append_visible_message(
        &self,
        session_id: &str,
        turn_id: u64,
        pending: PendingUiMessage,
    ) -> Result<Option<UiMessage>, AppError> {
        let ui_message = {
            let mut inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;
            if !active_turn_matches(&inner, session_id, turn_id) {
                return Ok(None);
            }
            let ui_message = base_ui_message(UiMessageInput {
                session_id,
                index: inner.next_message_index,
                role: &pending.role,
                kind: &pending.kind,
                content: pending.content,
                created_at: pending.created_at,
                tool_name: pending.tool_name,
                tool_args: pending.tool_args,
                tool_output: pending.tool_output,
                turn_id,
            });
            inner.next_message_index += 1;
            inner.ui_messages.push(ui_message.clone());
            let persisted = self.snapshot_from_inner(&inner);
            self.persist_snapshot_value(&persisted)
                .map_err(AppError::internal)?;
            ui_message
        };

        self.broadcast(LiveEvent::message_added(
            self.agent_kind,
            ui_message.clone(),
        ));
        Ok(Some(ui_message))
    }

    fn append_trace_message(
        &self,
        session_id: &str,
        turn_id: u64,
        pending: PendingUiMessage,
    ) -> Result<Option<UiMessage>, AppError> {
        let ui_message = {
            let mut inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;
            if !active_turn_matches(&inner, session_id, turn_id) {
                return Ok(None);
            }
            let ui_message = base_trace_message(UiMessageInput {
                session_id,
                index: inner.next_trace_index,
                role: &pending.role,
                kind: &pending.kind,
                content: pending.content,
                created_at: pending.created_at,
                tool_name: pending.tool_name,
                tool_args: pending.tool_args,
                tool_output: pending.tool_output,
                turn_id,
            });
            inner.next_trace_index += 1;
            ui_message
        };

        append_trace_message_to_disk(
            &self
                .ask_app
                .trace_turn_path(self.agent_kind, session_id, &ui_message.turn_id),
            &ui_message,
        )
        .map_err(AppError::internal)?;
        self.broadcast(LiveEvent::trace_added(
            self.agent_kind,
            public_trace_message(&ui_message),
        ));
        Ok(Some(ui_message))
    }

    fn finish_turn(
        &self,
        started: StartedTurn,
        error_text: Option<String>,
        on_complete: Option<TurnCompletionHandler>,
    ) -> Result<Option<LiveSubmitResponse>, AppError> {
        let snapshot = started.session.snapshot().clone();
        let turn_key = format!("{}-turn-{}", started.session_id, started.turn_id);
        let response = {
            let mut inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;

            if !active_turn_matches(&inner, &started.session_id, started.turn_id) {
                return Ok(None);
            }

            inner.title = summary_title(&snapshot);
            inner.session_id = Some(snapshot.session_id.clone());
            inner.session = Some(started.session);
            inner.active_turn = None;
            if let Some(error_text) = error_text.clone() {
                inner.status = LiveStatus::Error;
                inner.last_error = Some(error_text);
            } else {
                inner.status = LiveStatus::WaitingForInput;
                inner.last_error = None;
            }

            let persisted = self.snapshot_from_inner(&inner);
            self.persist_snapshot_value(&persisted)
                .map_err(AppError::internal)?;
            let turn_messages = inner
                .ui_messages
                .iter()
                .filter(|message| message.turn_id == turn_key)
                .cloned()
                .collect::<Vec<_>>();

            LiveSubmitResponse {
                agent_kind: self.agent_kind,
                title: persisted.title,
                session_id: persisted.session_id,
                status: persisted.status,
                last_error: persisted.last_error,
                new_messages: turn_messages,
                total_message_count: inner.ui_messages.len(),
            }
        };

        if let Some(error_text) = error_text {
            self.broadcast(LiveEvent::turn_failed(response.clone(), error_text));
        } else {
            let workflow = on_complete.and_then(|callback| callback(&response));
            self.broadcast(LiveEvent::turn_finished(response.clone(), workflow));
        }

        Ok(Some(response))
    }

    fn is_active_turn(&self, session_id: &str, turn_id: u64) -> Result<bool, AppError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| AppError::internal(anyhow!("failed to lock live conversation state")))?;
        Ok(active_turn_matches(&inner, session_id, turn_id))
    }

    fn cancel_turn(&self) -> Result<LiveSnapshot, AppError> {
        let active = {
            let inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;
            inner
                .active_turn
                .clone()
                .ok_or_else(|| AppError::conflict("no active turn to cancel"))?
        };

        active.cancellation.cancel();

        let mut session = self
            .ask_app
            .load_session(self.agent_kind, &active.session_id)
            .map_err(AppError::internal)?;
        session.truncate_to(
            active.rollback_message_count,
            active.rollback_prompt_history_count,
        );
        session.save().map_err(AppError::internal)?;
        let snapshot = session.snapshot().clone();

        let persisted = {
            let mut inner = self.inner.lock().map_err(|_| {
                AppError::internal(anyhow!("failed to lock live conversation state"))
            })?;

            if !active_turn_identity_matches(&inner, &active.session_id, active.turn_id) {
                return Ok(self.snapshot_from_inner(&inner));
            }

            inner.ui_messages.truncate(active.rollback_ui_message_count);
            inner.next_message_index = active.rollback_next_message_index;
            inner.title = summary_title(&snapshot);
            inner.session_id = Some(snapshot.session_id.clone());
            inner.session = Some(session);
            inner.status = LiveStatus::WaitingForInput;
            inner.last_error = None;
            inner.active_turn = None;

            let persisted = self.snapshot_from_inner(&inner);
            self.persist_snapshot_value(&persisted)
                .map_err(AppError::internal)?;
            persisted
        };

        Ok(persisted)
    }

    fn snapshot_from_inner(&self, inner: &LiveConversationInner) -> LiveSnapshot {
        LiveSnapshot {
            agent_kind: self.agent_kind,
            title: inner.title.clone(),
            session_id: inner.session_id.clone(),
            status: inner.status.as_str().to_string(),
            last_error: inner.last_error.clone(),
            messages: inner.ui_messages.clone(),
        }
    }

    fn replace_session(&self, session: ConversationSession) -> Result<()> {
        let snapshot = session.snapshot().clone();
        let ui_messages = load_persisted_snapshot(
            &self
                .ask_app
                .ui_session_snapshot_path(self.agent_kind, &snapshot.session_id),
        )?
        .map(|snapshot| visible_messages_only(&snapshot.messages))
        .unwrap_or_else(|| {
            flatten_messages(
                &snapshot.session_id,
                &snapshot.messages,
                0,
                snapshot.updated_at_unix_ms,
            )
        });
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("failed to lock live conversation state"))?;
        inner.title = summary_title(&snapshot);
        inner.session_id = Some(snapshot.session_id);
        inner.next_message_index = ui_messages.len();
        inner.next_trace_index = 0;
        inner.ui_messages = ui_messages;
        inner.session = Some(session);
        inner.status = LiveStatus::WaitingForInput;
        inner.last_error = None;
        inner.active_turn = None;
        let persisted = self.snapshot_from_inner(&inner);
        self.persist_snapshot_value(&persisted)?;
        self.broadcast(LiveEvent::snapshot(self.agent_kind, persisted, None));
        Ok(())
    }
}

fn active_turn_matches(inner: &LiveConversationInner, session_id: &str, turn_id: u64) -> bool {
    active_turn_identity_matches(inner, session_id, turn_id)
        && inner
            .active_turn
            .as_ref()
            .is_some_and(|active| !active.cancellation.is_cancelled())
}

fn active_turn_identity_matches(
    inner: &LiveConversationInner,
    session_id: &str,
    turn_id: u64,
) -> bool {
    inner
        .active_turn
        .as_ref()
        .is_some_and(|active| active.session_id == session_id && active.turn_id == turn_id)
}

struct WorkflowManager {
    ask_app: AskApp,
    workflow_path: PathBuf,
    materials_live: Arc<LiveConversationManager>,
    interview_live: Arc<LiveConversationManager>,
    inner: Arc<Mutex<WorkflowSnapshot>>,
}

impl WorkflowManager {
    fn bootstrap(ask_app: AskApp) -> Result<Self> {
        let workflow_path = ask_app.workflow_snapshot_path();
        let materials_live = Arc::new(LiveConversationManager::bootstrap(
            AgentKind::InterviewMaterials,
            ask_app.clone(),
        )?);
        let interview_live = Arc::new(LiveConversationManager::bootstrap(
            AgentKind::ProgrammerInterview,
            ask_app.clone(),
        )?);
        let persisted = load_workflow_snapshot(&workflow_path)?;
        let mut snapshot = persisted.unwrap_or_else(|| default_workflow_snapshot(&ask_app));
        refresh_workflow_metadata(&ask_app, &materials_live, &interview_live, &mut snapshot);

        let manager = Self {
            ask_app,
            workflow_path,
            materials_live,
            interview_live,
            inner: Arc::new(Mutex::new(snapshot)),
        };
        manager.persist_workflow()?;
        Ok(manager)
    }

    fn live_for(&self, agent_kind: AgentKind) -> Arc<LiveConversationManager> {
        match agent_kind {
            AgentKind::InterviewMaterials => Arc::clone(&self.materials_live),
            AgentKind::ProgrammerInterview => Arc::clone(&self.interview_live),
        }
    }

    fn snapshot(&self) -> Result<WorkflowSnapshot> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("failed to lock workflow state"))?;
        refresh_workflow_metadata(
            &self.ask_app,
            &self.materials_live,
            &self.interview_live,
            &mut inner,
        );
        Ok(inner.clone())
    }

    fn persist_workflow(&self) -> Result<()> {
        let snapshot = self.snapshot()?;
        save_workflow_snapshot(&self.workflow_path, &snapshot)
    }

    fn list_sessions(&self) -> Result<Vec<SessionListItem>, AppError> {
        let active_session_id = self
            .materials_live
            .snapshot()
            .map_err(AppError::internal)?
            .session_id;
        let sessions = self
            .ask_app
            .list_session_snapshots(AgentKind::InterviewMaterials)
            .map_err(AppError::internal)?
            .into_iter()
            .map(|snapshot| {
                let session_id = snapshot.session_id.clone();
                SessionListItem {
                    is_active: active_session_id.as_deref() == Some(session_id.as_str()),
                    title: summary_title(&snapshot),
                    prompt_count: snapshot.prompt_history.len(),
                    message_count: snapshot.messages.len(),
                    created_at_unix_ms: snapshot.created_at_unix_ms,
                    updated_at_unix_ms: snapshot.updated_at_unix_ms,
                    interview_status: self.interview_status_for_session(&session_id),
                    materials_exists: self.ask_app.session_materials_path(&session_id).is_file(),
                    report_exists: self.ask_app.session_report_path(&session_id).is_file(),
                    session_id,
                }
            })
            .collect();

        Ok(sessions)
    }

    fn select_session(&self, session_id: String) -> Result<LiveSnapshot, AppError> {
        let previous_active = self
            .inner
            .lock()
            .map_err(|_| AppError::internal(anyhow!("failed to lock workflow state")))?
            .active_agent;
        let materials_snapshot = self
            .materials_live
            .select_session(&session_id)?
            .ok_or_else(|| AppError::bad_request(format!("unknown session_id: {session_id}")))?;
        let interview_snapshot = self.interview_live.select_session(&session_id)?;

        let response = if previous_active == AgentKind::ProgrammerInterview {
            interview_snapshot.unwrap_or_else(|| materials_snapshot.clone())
        } else {
            materials_snapshot
        };

        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::internal(anyhow!("failed to lock workflow state")))?;
            inner.active_agent = response.agent_kind;
            inner.interview_status = self.interview_status_for_session(&session_id);
            inner.interview_phase = interview_phase_for_status(inner.interview_status).to_string();
            inner.materials_session_id = Some(session_id.clone());
            inner.interview_session_id = self
                .interview_live
                .snapshot()
                .ok()
                .and_then(|snapshot| snapshot.session_id);
            inner.materials = materials_meta_for_session(&self.ask_app, Some(&session_id));
            inner.report = report_meta_for_session(&self.ask_app, Some(&session_id));
        }
        self.persist_workflow().map_err(AppError::internal)?;
        Ok(response)
    }

    fn reset_to_new_session(&self) -> Result<LiveSnapshot, AppError> {
        if self.materials_live.is_busy()? || self.interview_live.is_busy()? {
            return Err(AppError::conflict(
                "agent is still working on the current turn",
            ));
        }

        let snapshot = self.materials_live.reset_to_new_session()?;
        if let Some(session_id) = snapshot.session_id.as_deref() {
            let _ = self.interview_live.select_session(session_id)?;
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::internal(anyhow!("failed to lock workflow state")))?;
            inner.active_agent = AgentKind::InterviewMaterials;
            inner.interview_status = InterviewStatus::NotStarted;
            inner.interview_phase = "INIT".to_string();
            inner.materials_session_id = Some(session_id.to_string());
            inner.interview_session_id = None;
            inner.materials = materials_meta_for_session(&self.ask_app, Some(session_id));
            inner.report = None;
        }
        self.persist_workflow().map_err(AppError::internal)?;
        Ok(snapshot)
    }

    fn interview_status_for_session(&self, session_id: &str) -> InterviewStatus {
        if self.ask_app.session_report_path(session_id).is_file() {
            InterviewStatus::Completed
        } else if self
            .ask_app
            .load_session(AgentKind::ProgrammerInterview, session_id)
            .is_ok()
        {
            InterviewStatus::InProgress
        } else {
            InterviewStatus::NotStarted
        }
    }

    fn copy_latest_materials_to_session(&self, session_id: &str) -> Result<()> {
        let source = self.ask_app.latest_materials_path();
        if !source.is_file() {
            return Ok(());
        }
        let target = self.ask_app.session_materials_path(session_id);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create materials dir: {}", parent.display()))?;
        }
        std::fs::copy(&source, &target).with_context(|| {
            format!(
                "failed to copy materials from {} to {}",
                source.display(),
                target.display()
            )
        })?;
        Ok(())
    }

    fn read_materials_for_session(&self, session_id: &str) -> Result<String, AppError> {
        let session_path = self.ask_app.session_materials_path(session_id);
        let fallback_path = self.ask_app.latest_materials_path();
        let path = if session_path.is_file() {
            session_path
        } else if allows_latest_materials_fallback(&self.ask_app, session_id) {
            fallback_path
        } else {
            session_path
        };
        let materials = std::fs::read_to_string(&path).map_err(|_| {
            AppError::conflict(format!(
                "interview materials not found at {}",
                path.display()
            ))
        })?;
        if materials.trim().is_empty() {
            return Err(AppError::conflict("interview materials file is empty"));
        }
        Ok(materials)
    }

    fn completion_handler(self: &Arc<Self>, agent_kind: AgentKind) -> TurnCompletionHandler {
        let workflow = Arc::clone(self);
        Arc::new(
            move |response| match workflow.complete_agent_turn(agent_kind, response) {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    eprintln!("error: failed to complete workflow turn: {}", error.message);
                    None
                }
            },
        )
    }

    fn finish_interview_completion_handler(
        self: &Arc<Self>,
        session_id: String,
    ) -> TurnCompletionHandler {
        let workflow = Arc::clone(self);
        Arc::new(move |response| {
            match workflow.write_report_and_complete_interview(&session_id, response) {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    eprintln!(
                        "error: failed to write interview report after completion: {}",
                        error.message
                    );
                    None
                }
            }
        })
    }

    fn complete_agent_turn(
        &self,
        agent_kind: AgentKind,
        response: &LiveSubmitResponse,
    ) -> Result<WorkflowSnapshot, AppError> {
        if agent_kind == AgentKind::InterviewMaterials
            && let Some(session_id) = response.session_id.as_deref()
        {
            self.copy_latest_materials_to_session(session_id)
                .map_err(AppError::internal)?;
        }

        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::internal(anyhow!("failed to lock workflow state")))?;
            inner.active_agent = agent_kind;
            if agent_kind == AgentKind::ProgrammerInterview {
                inner.interview_status = InterviewStatus::InProgress;
                inner.interview_phase = "IN_PROGRESS".to_string();
            }
        }
        self.persist_workflow().map_err(AppError::internal)?;
        self.snapshot().map_err(AppError::internal)
    }

    fn write_report_and_complete_interview(
        &self,
        session_id: &str,
        response: &LiveSubmitResponse,
    ) -> Result<WorkflowSnapshot, AppError> {
        let report = latest_assistant_content(&response.new_messages)
            .unwrap_or_else(|| "No evaluation report was produced.".to_string());
        let report_path = self.ask_app.latest_report_path();
        let session_report_path = self.ask_app.session_report_path(session_id);
        if let Some(parent) = session_report_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| AppError::internal(anyhow!(err)))?;
        }
        std::fs::write(&session_report_path, &report)
            .map_err(|err| AppError::internal(anyhow!(err)))?;
        if let Some(parent) = report_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| AppError::internal(anyhow!(err)))?;
        }
        std::fs::write(&report_path, report).map_err(|err| AppError::internal(anyhow!(err)))?;

        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::internal(anyhow!("failed to lock workflow state")))?;
            inner.active_agent = AgentKind::ProgrammerInterview;
            inner.interview_status = InterviewStatus::Completed;
            inner.interview_phase = "REPORT".to_string();
            inner.report = Some(ReportMeta {
                path: session_report_path.display().to_string(),
                updated_at: now_unix_ms(),
            });
        }
        self.persist_workflow().map_err(AppError::internal)?;
        self.snapshot().map_err(AppError::internal)
    }

    fn submit_user_message(
        self: &Arc<Self>,
        agent_kind: AgentKind,
        prompt: String,
    ) -> Result<LiveSubmitResponse, AppError> {
        if agent_kind == AgentKind::ProgrammerInterview
            && self
                .interview_live
                .snapshot()
                .map_err(AppError::internal)?
                .session_id
                .is_none()
        {
            return Err(AppError::conflict(
                "start the interview before sending candidate answers",
            ));
        }

        let response = self
            .live_for(agent_kind)
            .submit_user_message(prompt, Some(self.completion_handler(agent_kind)))?;
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::internal(anyhow!("failed to lock workflow state")))?;
            inner.active_agent = agent_kind;
            if agent_kind == AgentKind::ProgrammerInterview {
                inner.interview_status = InterviewStatus::InProgress;
                inner.interview_phase = "IN_PROGRESS".to_string();
            }
        }
        self.persist_workflow().map_err(AppError::internal)?;
        Ok(response)
    }

    fn cancel_agent_turn(&self, agent_kind: AgentKind) -> Result<LiveSnapshot, AppError> {
        let live = self.live_for(agent_kind);
        let snapshot = live.cancel_turn()?;
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::internal(anyhow!("failed to lock workflow state")))?;
            inner.active_agent = agent_kind;
        }
        self.persist_workflow().map_err(AppError::internal)?;
        let workflow = self.snapshot().map_err(AppError::internal)?;
        live.broadcast(LiveEvent::turn_cancelled(
            agent_kind,
            snapshot.clone(),
            Some(workflow),
        ));
        Ok(snapshot)
    }

    fn start_interview(self: &Arc<Self>) -> Result<LiveSnapshot, AppError> {
        let session_id = self
            .materials_live
            .snapshot()
            .map_err(AppError::internal)?
            .session_id
            .ok_or_else(|| {
                AppError::conflict("create or select a session before starting interview")
            })?;
        let materials = self.read_materials_for_session(&session_id)?;
        let should_begin;

        if let Ok(session) = self
            .ask_app
            .load_session(AgentKind::ProgrammerInterview, &session_id)
        {
            self.interview_live
                .replace_session(session)
                .map_err(AppError::internal)?;
            should_begin = false;
        } else {
            let session = self
                .ask_app
                .new_seeded_interview_session(
                    session_id.clone(),
                    &materials,
                    "Start the interview. Briefly introduce the process, then ask the first architecture question.".to_string(),
                )
                .map_err(AppError::internal)?;
            self.interview_live
                .replace_session(session)
                .map_err(AppError::internal)?;
            should_begin = true;
        }
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::internal(anyhow!("failed to lock workflow state")))?;
            inner.active_agent = AgentKind::ProgrammerInterview;
            inner.interview_status = InterviewStatus::InProgress;
            inner.interview_phase = "INTRODUCTION".to_string();
            inner.materials_session_id = Some(session_id.clone());
            inner.interview_session_id = Some(session_id.clone());
            inner.materials = materials_meta_for_session(&self.ask_app, Some(&session_id));
            inner.report = report_meta_for_session(&self.ask_app, Some(&session_id));
        }
        self.persist_workflow().map_err(AppError::internal)?;

        if should_begin {
            let _response = self.interview_live.submit_user_message(
                "Begin now.".to_string(),
                Some(self.completion_handler(AgentKind::ProgrammerInterview)),
            )?;
        }

        self.interview_live.snapshot().map_err(AppError::internal)
    }

    fn finish_interview(self: &Arc<Self>) -> Result<LiveSubmitResponse, AppError> {
        if self
            .interview_live
            .snapshot()
            .map_err(AppError::internal)?
            .session_id
            .is_none()
        {
            return Err(AppError::conflict("no interview session is in progress"));
        }
        let session_id = self
            .interview_live
            .snapshot()
            .map_err(AppError::internal)?
            .session_id
            .ok_or_else(|| AppError::conflict("no interview session is in progress"))?;

        let response = self.interview_live.submit_user_message(
            "Finish the interview now and produce the final evaluation report in Markdown."
                .to_string(),
            Some(self.finish_interview_completion_handler(session_id)),
        )?;
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::internal(anyhow!("failed to lock workflow state")))?;
            inner.active_agent = AgentKind::ProgrammerInterview;
            inner.interview_status = InterviewStatus::InProgress;
            inner.interview_phase = "REPORT".to_string();
        }
        self.persist_workflow().map_err(AppError::internal)?;
        Ok(response)
    }
}

pub fn api_router(ask_app: AskApp) -> Result<Router> {
    let workflow = Arc::new(WorkflowManager::bootstrap(ask_app)?);
    let state = WebState { workflow };
    Ok(api_routes().layer(CorsLayer::permissive()).with_state(state))
}

pub fn router(ask_app: AskApp) -> Result<Router> {
    let workflow = Arc::new(WorkflowManager::bootstrap(ask_app)?);
    let state = WebState { workflow };
    let frontend_dist_dir = PathBuf::from("frontend").join("dist");
    let frontend_index = frontend_dist_dir.join("index.html");
    if !frontend_dist_dir.is_dir() {
        return Err(anyhow!(
            "frontend assets not found at {}; run `npm run build` in frontend/ before starting the web server",
            frontend_dist_dir.display()
        ));
    }
    if !frontend_index.is_file() {
        return Err(anyhow!(
            "frontend entrypoint not found at {}; run `npm run build` in frontend/ before starting the web server",
            frontend_index.display()
        ));
    }
    Ok(api_routes()
        .fallback_service(
            ServeDir::new(&frontend_dist_dir).not_found_service(ServeFile::new(frontend_index)),
        )
        .layer(CorsLayer::permissive())
        .with_state(state))
}

fn api_routes() -> Router<WebState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/sessions", get(list_sessions))
        .route("/api/workflow", get(get_workflow))
        .route("/api/live", get(get_legacy_live))
        .route("/api/live/session", post(select_live_session))
        .route("/api/live/session/new", post(post_new_live_session))
        .route("/api/live/messages", post(post_legacy_live_message))
        .route("/api/live/events", get(get_legacy_live_events))
        .route("/api/agents/{agent_kind}/live", get(get_agent_live))
        .route("/api/agents/{agent_kind}/events", get(get_agent_events))
        .route(
            "/api/agents/{agent_kind}/messages",
            post(post_agent_message),
        )
        .route(
            "/api/agents/{agent_kind}/sessions/{session_id}/trace/{turn_id}",
            get(get_agent_turn_trace),
        )
        .route(
            "/api/agents/{agent_kind}/turn/cancel",
            post(post_agent_turn_cancel),
        )
        .route("/api/interview/start", post(post_interview_start))
        .route("/api/interview/finish", post(post_interview_finish))
        .route("/api/interview/report", get(get_interview_report))
}

pub async fn serve(ask_app: AskApp, host: String, port: u16) -> Result<()> {
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("invalid bind address: {host}:{port}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind web server on http://{host}:{port}"))?;

    serve_listener(ask_app, listener).await
}

pub async fn serve_listener(ask_app: AskApp, listener: tokio::net::TcpListener) -> Result<()> {
    let addr = listener
        .local_addr()
        .context("failed to read web server listener address")?;
    println!("Scribe web UI running at http://{addr}");
    axum::serve(listener, router(ask_app)?)
        .await
        .context("web server exited unexpectedly")
}

pub async fn serve_api_listener(ask_app: AskApp, listener: tokio::net::TcpListener) -> Result<()> {
    let addr = listener
        .local_addr()
        .context("failed to read API listener address")?;
    println!("Scribe API running at http://{addr}");
    axum::serve(listener, api_router(ask_app)?)
        .await
        .context("API server exited unexpectedly")
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

async fn get_workflow(State(state): State<WebState>) -> Result<Json<WorkflowSnapshot>, AppError> {
    Ok(Json(state.workflow.snapshot().map_err(AppError::internal)?))
}

async fn list_sessions(
    State(state): State<WebState>,
) -> Result<Json<Vec<SessionListItem>>, AppError> {
    Ok(Json(state.workflow.list_sessions()?))
}

async fn select_live_session(
    State(state): State<WebState>,
    Json(body): Json<SelectSessionBody>,
) -> Result<Json<LiveSnapshot>, AppError> {
    let session_id = body.session_id.trim().to_string();
    if session_id.is_empty() {
        return Err(AppError::bad_request("session_id must not be empty"));
    }

    Ok(Json(state.workflow.select_session(session_id)?))
}

async fn post_new_live_session(
    State(state): State<WebState>,
) -> Result<Json<LiveSnapshot>, AppError> {
    Ok(Json(state.workflow.reset_to_new_session()?))
}

async fn get_legacy_live(State(state): State<WebState>) -> Result<Json<LiveSnapshot>, AppError> {
    Ok(Json(
        state
            .workflow
            .live_for(AgentKind::InterviewMaterials)
            .snapshot()
            .map_err(AppError::internal)?,
    ))
}

async fn post_legacy_live_message(
    State(state): State<WebState>,
    Json(body): Json<PromptBody>,
) -> Result<Json<LiveSubmitResponse>, AppError> {
    let prompt = non_empty_prompt(body.prompt)?;
    let response = state
        .workflow
        .submit_user_message(AgentKind::InterviewMaterials, prompt)?;
    Ok(Json(response))
}

async fn get_legacy_live_events(
    State(state): State<WebState>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    live_events_sse(Arc::clone(&state.workflow), AgentKind::InterviewMaterials)
}

async fn get_agent_live(
    State(state): State<WebState>,
    AxumPath(agent_kind): AxumPath<String>,
) -> Result<Json<LiveSnapshot>, AppError> {
    let agent_kind = parse_agent_kind(&agent_kind)?;
    Ok(Json(
        state
            .workflow
            .live_for(agent_kind)
            .snapshot()
            .map_err(AppError::internal)?,
    ))
}

async fn get_agent_events(
    State(state): State<WebState>,
    AxumPath(agent_kind): AxumPath<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let agent_kind = parse_agent_kind(&agent_kind)?;
    live_events_sse(Arc::clone(&state.workflow), agent_kind)
}

async fn post_agent_message(
    State(state): State<WebState>,
    AxumPath(agent_kind): AxumPath<String>,
    Json(body): Json<PromptBody>,
) -> Result<Json<LiveSubmitResponse>, AppError> {
    let agent_kind = parse_agent_kind(&agent_kind)?;
    let prompt = non_empty_prompt(body.prompt)?;
    let response = state.workflow.submit_user_message(agent_kind, prompt)?;
    Ok(Json(response))
}

async fn get_agent_turn_trace(
    State(state): State<WebState>,
    AxumPath((agent_kind, session_id, turn_id)): AxumPath<(String, String, String)>,
) -> Result<Json<Vec<UiMessage>>, AppError> {
    let agent_kind = parse_agent_kind(&agent_kind)?;
    if session_id.trim().is_empty() || turn_id.trim().is_empty() {
        return Err(AppError::bad_request(
            "session_id and turn_id must not be empty",
        ));
    }
    let path = state
        .workflow
        .ask_app
        .trace_turn_path(agent_kind, &session_id, &turn_id);
    Ok(Json(
        load_trace_messages(&path).map_err(AppError::internal)?,
    ))
}

async fn post_agent_turn_cancel(
    State(state): State<WebState>,
    AxumPath(agent_kind): AxumPath<String>,
) -> Result<Json<LiveSnapshot>, AppError> {
    let agent_kind = parse_agent_kind(&agent_kind)?;
    let snapshot = state.workflow.cancel_agent_turn(agent_kind)?;
    Ok(Json(snapshot))
}

async fn post_interview_start(
    State(state): State<WebState>,
) -> Result<Json<LiveSnapshot>, AppError> {
    let response = state.workflow.start_interview()?;
    Ok(Json(response))
}

async fn post_interview_finish(
    State(state): State<WebState>,
) -> Result<Json<LiveSubmitResponse>, AppError> {
    let response = state.workflow.finish_interview()?;
    Ok(Json(response))
}

async fn get_interview_report(State(state): State<WebState>) -> Result<String, AppError> {
    let session_id = state
        .workflow
        .materials_live
        .snapshot()
        .map_err(AppError::internal)?
        .session_id;
    let report_path = session_id
        .as_deref()
        .map(|session_id| state.workflow.ask_app.session_report_path(session_id))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| state.workflow.ask_app.latest_report_path());
    std::fs::read_to_string(&report_path).map_err(|_| {
        AppError::conflict(format!(
            "evaluation report not found at {}",
            report_path.display()
        ))
    })
}

fn live_events_sse(
    workflow: Arc<WorkflowManager>,
    agent_kind: AgentKind,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>> + Send + 'static>, AppError> {
    let live = workflow.live_for(agent_kind);
    let snapshot = live.snapshot().map_err(AppError::internal)?;
    let workflow_snapshot = workflow.snapshot().map_err(AppError::internal)?;
    let initial = LiveEvent::snapshot(agent_kind, snapshot, Some(workflow_snapshot));
    let receiver = live.subscribe();

    let initial_stream = tokio_stream::iter(vec![live_event_to_sse(initial)]);
    let update_stream = BroadcastStream::new(receiver).filter_map(|result| match result {
        Ok(event) => Some(live_event_to_sse(event)),
        Err(_) => None,
    });

    Ok(Sse::new(initial_stream.chain(update_stream)).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

fn live_event_to_sse(event: LiveEvent) -> Result<Event, Infallible> {
    let event_type = event.event_type.clone();
    let data = serde_json::to_string(&event).unwrap_or_else(|error| {
        format!(
            r#"{{"type":"serialization_error","error":"failed to serialize live event: {}"}}"#,
            error
        )
    });
    Ok(Event::default().event(event_type).data(data))
}

fn default_workflow_snapshot(ask_app: &AskApp) -> WorkflowSnapshot {
    WorkflowSnapshot {
        active_agent: AgentKind::InterviewMaterials,
        available_agents: AgentKind::ALL
            .into_iter()
            .map(|kind| AgentInfo {
                kind,
                id: kind.as_str().to_string(),
                title: kind.title().to_string(),
            })
            .collect(),
        materials: materials_meta_for_session(ask_app, None),
        interview_status: InterviewStatus::NotStarted,
        interview_phase: "INIT".to_string(),
        materials_session_id: None,
        interview_session_id: None,
        report: report_meta_for_session(ask_app, None),
    }
}

fn refresh_workflow_metadata(
    ask_app: &AskApp,
    materials_live: &LiveConversationManager,
    interview_live: &LiveConversationManager,
    snapshot: &mut WorkflowSnapshot,
) {
    snapshot.available_agents = AgentKind::ALL
        .into_iter()
        .map(|kind| AgentInfo {
            kind,
            id: kind.as_str().to_string(),
            title: kind.title().to_string(),
        })
        .collect();
    snapshot.materials_session_id = materials_live
        .snapshot()
        .ok()
        .and_then(|snapshot| snapshot.session_id);
    snapshot.interview_session_id = interview_live
        .snapshot()
        .ok()
        .and_then(|snapshot| snapshot.session_id);
    snapshot.materials =
        materials_meta_for_session(ask_app, snapshot.materials_session_id.as_deref());
    if snapshot.interview_session_id.is_none() {
        snapshot.interview_status = InterviewStatus::NotStarted;
        snapshot.interview_phase = "INIT".to_string();
    } else if let Some(session_id) = snapshot.interview_session_id.as_deref()
        && ask_app.session_report_path(session_id).is_file()
    {
        snapshot.interview_status = InterviewStatus::Completed;
        snapshot.interview_phase = "REPORT".to_string();
    }
    snapshot.report = report_meta_for_session(ask_app, snapshot.materials_session_id.as_deref());
}

fn materials_meta_for_session(ask_app: &AskApp, session_id: Option<&str>) -> MaterialsMeta {
    let session_path = session_id.map(|session_id| ask_app.session_materials_path(session_id));
    let path = match (session_id, session_path) {
        (Some(_), Some(path)) if path.is_file() => path,
        (Some(session_id), Some(_)) if allows_latest_materials_fallback(ask_app, session_id) => {
            ask_app.latest_materials_path()
        }
        (Some(_), Some(path)) => path,
        _ => ask_app.latest_materials_path(),
    };
    let metadata = std::fs::metadata(&path).ok();
    MaterialsMeta {
        exists: metadata.as_ref().is_some_and(|metadata| metadata.is_file()),
        path: path.display().to_string(),
        updated_at: metadata
            .and_then(|metadata| metadata.modified().ok())
            .and_then(system_time_to_unix_ms),
    }
}

fn allows_latest_materials_fallback(ask_app: &AskApp, session_id: &str) -> bool {
    ask_app
        .load_session(AgentKind::InterviewMaterials, session_id)
        .map(|session| !session.snapshot().prompt_history.is_empty())
        .unwrap_or(false)
        && ask_app.latest_materials_path().is_file()
}

fn report_meta_for_session(ask_app: &AskApp, session_id: Option<&str>) -> Option<ReportMeta> {
    let session_path = session_id.map(|session_id| ask_app.session_report_path(session_id));
    let path = session_path
        .filter(|path| path.is_file())
        .unwrap_or_else(|| ask_app.latest_report_path());
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(ReportMeta {
        path: path.display().to_string(),
        updated_at: metadata
            .modified()
            .ok()
            .and_then(system_time_to_unix_ms)
            .unwrap_or_else(now_unix_ms),
    })
}

fn system_time_to_unix_ms(value: SystemTime) -> Option<u128> {
    value.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis())
}

fn load_workflow_snapshot(path: &FsPath) -> Result<Option<WorkflowSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read(path)
        .with_context(|| format!("failed to read workflow snapshot: {}", path.display()))?;
    let snapshot = serde_json::from_slice::<WorkflowSnapshot>(&content)
        .with_context(|| format!("failed to parse workflow snapshot: {}", path.display()))?;
    Ok(Some(snapshot))
}

fn save_workflow_snapshot(path: &FsPath, snapshot: &WorkflowSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create workflow snapshot dir: {}",
                parent.display()
            )
        })?;
    }

    let content =
        serde_json::to_vec_pretty(snapshot).context("failed to serialize workflow snapshot")?;
    std::fs::write(path, content)
        .with_context(|| format!("failed to write workflow snapshot: {}", path.display()))
}

fn latest_assistant_content(messages: &[UiMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant" && message.kind == "assistant")
        .map(|message| message.content.clone())
}

fn parse_agent_kind(value: &str) -> Result<AgentKind, AppError> {
    AgentKind::parse(value)
        .ok_or_else(|| AppError::bad_request(format!("unknown agent kind: {value}")))
}

fn load_persisted_snapshot(path: &FsPath) -> Result<Option<LiveSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read(path)
        .with_context(|| format!("failed to read live snapshot: {}", path.display()))?;
    let snapshot = serde_json::from_slice::<LiveSnapshot>(&content)
        .with_context(|| format!("failed to parse live snapshot: {}", path.display()))?;
    Ok(Some(snapshot))
}

fn save_persisted_snapshot(path: &FsPath, snapshot: &LiveSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create live snapshot dir: {}", parent.display()))?;
    }

    let content =
        serde_json::to_vec_pretty(snapshot).context("failed to serialize live snapshot")?;
    std::fs::write(path, content)
        .with_context(|| format!("failed to write live snapshot: {}", path.display()))
}

fn pending_message_from_event(event: RuntimeEvent) -> Option<PendingLiveMessage> {
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
            let has_tool_calls = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty());

            let pending = PendingUiMessage {
                role: "assistant".to_string(),
                kind: if has_tool_calls {
                    "assistant_trace".to_string()
                } else {
                    "assistant".to_string()
                },
                content,
                created_at: now_unix_ms(),
                tool_name: None,
                tool_args: None,
                tool_output: None,
            };

            if has_tool_calls {
                Some(PendingLiveMessage::Trace(pending))
            } else {
                Some(PendingLiveMessage::Visible(pending))
            }
        }
        RuntimeEvent::Compaction {
            removed_messages,
            estimated_tokens_before,
            transcript_path,
        } => Some(PendingLiveMessage::Trace(PendingUiMessage {
            role: "system".to_string(),
            kind: "context_compacted".to_string(),
            content: serde_json::json!({
                "removed_messages": removed_messages,
                "estimated_tokens_before": estimated_tokens_before,
                "transcript_path": transcript_path,
            })
            .to_string(),
            created_at: now_unix_ms(),
            tool_name: None,
            tool_args: None,
            tool_output: None,
        })),
        RuntimeEvent::ToolCall {
            tool_call_id: _tool_call_id,
            name,
            arguments,
        } => Some(PendingLiveMessage::Trace(PendingUiMessage {
            role: "assistant".to_string(),
            kind: "tool_call".to_string(),
            content: String::new(),
            created_at: now_unix_ms(),
            tool_name: Some(name),
            tool_args: Some(arguments),
            tool_output: None,
        })),
        RuntimeEvent::ToolResult {
            tool_call_id: _tool_call_id,
            name,
            arguments,
            result,
        } => Some(PendingLiveMessage::Trace(PendingUiMessage {
            role: "tool".to_string(),
            kind: "tool_result".to_string(),
            content: String::new(),
            created_at: now_unix_ms(),
            tool_name: Some(name),
            tool_args: Some(arguments),
            tool_output: Some(result),
        })),
    }
}

fn flatten_messages(
    session_id: &str,
    messages: &[Value],
    offset: usize,
    created_at: u128,
) -> Vec<UiMessage> {
    let mut ui_messages = Vec::new();
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
        if is_internal_seed_message(&content) {
            continue;
        }

        if role == "user" {
            turn_number += 1;
            ui_messages.push(base_ui_message(UiMessageInput {
                session_id,
                index: absolute_index,
                role: "user",
                kind: "user",
                content,
                created_at,
                tool_name: None,
                tool_args: None,
                tool_output: None,
                turn_id: turn_number,
            }));
            continue;
        }

        let effective_turn = if turn_number == 0 { 1 } else { turn_number };

        if role == "assistant" {
            let has_tool_calls = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty());
            if !has_tool_calls && !content.trim().is_empty() {
                ui_messages.push(base_ui_message(UiMessageInput {
                    session_id,
                    index: absolute_index,
                    role: "assistant",
                    kind: "assistant",
                    content: content.clone(),
                    created_at,
                    tool_name: None,
                    tool_args: None,
                    tool_output: None,
                    turn_id: effective_turn,
                }));
            }
            continue;
        }

        if role == "tool" {
            continue;
        }

        ui_messages.push(base_ui_message(UiMessageInput {
            session_id,
            index: absolute_index,
            role: &role,
            kind: &role,
            content,
            created_at,
            tool_name: None,
            tool_args: None,
            tool_output: None,
            turn_id: effective_turn,
        }));
    }

    ui_messages
}

struct UiMessageInput<'a> {
    session_id: &'a str,
    index: usize,
    role: &'a str,
    kind: &'a str,
    content: String,
    created_at: u128,
    tool_name: Option<String>,
    tool_args: Option<String>,
    tool_output: Option<String>,
    turn_id: u64,
}

fn base_ui_message(input: UiMessageInput<'_>) -> UiMessage {
    let text_for_blocks = if input.kind == "tool_result" {
        input.tool_output.clone().unwrap_or_default()
    } else if input.kind == "tool_call" {
        input.tool_args.clone().unwrap_or_default()
    } else {
        input.content.clone()
    };

    UiMessage {
        id: format!("{}-{}-{}", input.session_id, input.index, input.kind),
        role: input.role.to_string(),
        kind: input.kind.to_string(),
        content: input.content,
        created_at: input.created_at,
        turn_id: format!("{}-turn-{}", input.session_id, input.turn_id),
        tool_name: input.tool_name,
        tool_args: input.tool_args,
        tool_output: input.tool_output,
        render_blocks: parse_render_blocks(&text_for_blocks, input.kind),
    }
}

fn base_trace_message(input: UiMessageInput<'_>) -> UiMessage {
    let trace_id = format!(
        "{}-trace-{}-{}-{}",
        input.session_id, input.turn_id, input.index, input.kind
    );
    let mut message = base_ui_message(input);
    message.id = trace_id;
    message
}

fn visible_messages_only(messages: &[UiMessage]) -> Vec<UiMessage> {
    messages
        .iter()
        .filter(|message| is_visible_ui_message(message))
        .cloned()
        .collect()
}

fn is_visible_ui_message(message: &UiMessage) -> bool {
    matches!(message.kind.as_str(), "user" | "assistant" | "system")
}

fn public_trace_message(message: &UiMessage) -> UiMessage {
    let mut public = message.clone();
    match public.kind.as_str() {
        "tool_result" => {
            let output = public.tool_output.as_deref().unwrap_or_default();
            public.content = if output.to_ascii_lowercase().contains("tool_error") {
                "tool_error".to_string()
            } else {
                "completed".to_string()
            };
            public.tool_output = None;
            public.render_blocks = parse_render_blocks(&public.content, &public.kind);
        }
        "tool_call" => {
            public.tool_output = None;
            public.render_blocks = parse_render_blocks(
                public.tool_args.as_deref().unwrap_or_default(),
                &public.kind,
            );
        }
        "assistant_trace" | "context_compacted" => {}
        _ => {
            public.tool_args = None;
            public.tool_output = None;
        }
    }
    public
}

fn append_trace_message_to_disk(path: &FsPath, message: &UiMessage) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create trace dir: {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open trace file: {}", path.display()))?;
    let line = serde_json::to_string(message).context("failed to serialize trace message")?;
    writeln!(file, "{line}")
        .with_context(|| format!("failed to write trace file: {}", path.display()))
}

fn load_trace_messages(path: &FsPath) -> Result<Vec<UiMessage>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read trace file: {}", path.display()))?;
    let mut messages = Vec::new();
    for (line_number, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let message = serde_json::from_str::<UiMessage>(line).with_context(|| {
            format!(
                "failed to parse trace message at {}:{}",
                path.display(),
                line_number + 1
            )
        })?;
        messages.push(message);
    }
    Ok(messages)
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

    if kind == "assistant_trace" || kind == "context_compacted" {
        return vec![UiRenderBlock {
            block_type: kind.to_string(),
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
        .iter()
        .rev()
        .find(|value| !is_internal_seed_message(value))
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

fn interview_phase_for_status(status: InterviewStatus) -> &'static str {
    match status {
        InterviewStatus::NotStarted => "INIT",
        InterviewStatus::InProgress => "IN_PROGRESS",
        InterviewStatus::Completed => "REPORT",
    }
}

fn is_internal_seed_message(content: &str) -> bool {
    let trimmed = content.trim();
    trimmed.starts_with("Use the following interview materials as your only codebase context.")
        || trimmed == "Understood. I will interview the programmer using only these materials."
        || trimmed.starts_with("Start the interview. Briefly introduce the process")
        || trimmed == "Begin now."
        || trimmed.starts_with("Finish the interview now and produce the final evaluation report")
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flatten_messages_keeps_only_user_and_final_assistant_replies() {
        let messages = vec![
            json!({"role": "system", "content": "system"}),
            json!({"role": "user", "content": "Explain the architecture"}),
            json!({
                "role": "assistant",
                "content": "I will inspect the code first.",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "file contents"}),
            json!({"role": "assistant", "content": "Final architecture summary."}),
        ];

        let ui_messages = flatten_messages("session_test", &messages, 0, 1);

        assert_eq!(ui_messages.len(), 2);
        assert_eq!(ui_messages[0].kind, "user");
        assert_eq!(ui_messages[1].kind, "assistant");
        assert_eq!(ui_messages[1].content, "Final architecture summary.");
    }

    #[test]
    fn visible_messages_filter_excludes_trace_messages() {
        let messages = vec![
            base_ui_message(UiMessageInput {
                session_id: "session_test",
                index: 0,
                role: "user",
                kind: "user",
                content: "hello".to_string(),
                created_at: 1,
                tool_name: None,
                tool_args: None,
                tool_output: None,
                turn_id: 1,
            }),
            base_trace_message(UiMessageInput {
                session_id: "session_test",
                index: 0,
                role: "assistant",
                kind: "tool_call",
                content: String::new(),
                created_at: 1,
                tool_name: Some("read_file".to_string()),
                tool_args: Some("{}".to_string()),
                tool_output: None,
                turn_id: 1,
            }),
        ];

        let visible = visible_messages_only(&messages);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].kind, "user");
    }

    #[test]
    fn public_trace_message_strips_tool_result_output() {
        let message = base_trace_message(UiMessageInput {
            session_id: "session_test",
            index: 0,
            role: "tool",
            kind: "tool_result",
            content: String::new(),
            created_at: 1,
            tool_name: Some("read_file".to_string()),
            tool_args: Some(r#"{"path":"src/main.rs"}"#.to_string()),
            tool_output: Some("very large file contents".repeat(100)),
            turn_id: 1,
        });

        let public = public_trace_message(&message);

        assert_eq!(public.kind, "tool_result");
        assert_eq!(public.content, "completed");
        assert!(public.tool_output.is_none());
    }
}
