use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::LlmConfig;
use crate::llm::openai::OpenAiCompatClient;
use crate::llm::session::{ConversationSession, ConversationSessionSnapshot};
use crate::runtime::{AgentLoop, ConversationRuntime, RuntimeEventSink};
use crate::tools::task::{task_handler, task_query_handlers};
use crate::tools::{
    GlobalToolRegistry, TaskRegistry, TeamManager, team_tool_handlers,
};

#[derive(Clone)]
pub struct AskApp {
    llm: Arc<OpenAiCompatClient>,
    runtime: Arc<ConversationRuntime>,
    transcript_dir: String,
}

impl AskApp {
    pub fn new(llm: Arc<OpenAiCompatClient>, runtime: Arc<ConversationRuntime>, transcript_dir: String) -> Self {
        Self {
            llm,
            runtime,
            transcript_dir,
        }
    }

    pub fn from_env(max_steps: usize, base_registry: GlobalToolRegistry) -> Result<Self> {
        let llm_cfg = LlmConfig::from_env()?;
        let llm = Arc::new(OpenAiCompatClient::new(llm_cfg)?);
        let transcript_dir = llm.context_compact_config().transcript_dir.clone();

        let ask_registry = if crate::github_auth_available() {
            base_registry
        } else {
            base_registry.without_tool("github_wiki_publish")
        };

        let shared_agent_loop = Arc::new(AgentLoop::new(Arc::clone(&llm), max_steps));
        let task_registry = Arc::new(TaskRegistry::new());

        let team_child_registry = Arc::new(ask_registry.without_tool("task"));
        let task_tool = task_handler(
            Arc::clone(&shared_agent_loop),
            Arc::clone(&team_child_registry),
            Arc::clone(&task_registry),
        );
        let task_query_tools = task_query_handlers(Arc::clone(&task_registry));

        let team_dir = std::env::var("AGENT_TEAM_DIR").unwrap_or_else(|_| ".team".to_string());
        let team_manager = Arc::new(TeamManager::new(team_dir)?);
        let team_tools = team_tool_handlers(
            Arc::clone(&team_manager),
            Arc::clone(&shared_agent_loop),
            Arc::clone(&team_child_registry),
            Arc::clone(&task_registry),
        );

        let mut parent_registry = ask_registry.with_tool(task_tool)?;
        for tool in task_query_tools {
            parent_registry = parent_registry.with_tool(tool)?;
        }
        for tool in team_tools {
            parent_registry = parent_registry.with_tool(tool)?;
        }

        let runtime = Arc::new(ConversationRuntime::new(
            Arc::clone(&llm),
            Arc::new(parent_registry),
            max_steps,
        ));

        Ok(Self::new(llm, runtime, transcript_dir))
    }

    pub fn new_session(&self, initial_prompt: String) -> Result<ConversationSession> {
        let session = ConversationSession::new(initial_prompt, self.llm.system_prompt(), &self.transcript_dir)?;
        session.save()?;
        Ok(session)
    }

    pub fn run_session_turn_with_events(
        &self,
        session: &mut ConversationSession,
        event_sink: Option<RuntimeEventSink>,
    ) -> Result<String> {
        let answer = self.runtime.run_session_turn_with_events(session, event_sink)?;
        session.save()?;
        Ok(answer)
    }

    pub fn load_session(&self, session_id: &str) -> Result<ConversationSession> {
        let path = self.session_path(session_id);
        ConversationSession::load(path)
    }

    pub fn load_latest_session(&self) -> Result<Option<ConversationSession>> {
        let latest = self
            .list_session_snapshots()?
            .into_iter()
            .next()
            .map(|snapshot| snapshot.session_id);
        match latest {
            Some(session_id) => self.load_session(&session_id).map(Some),
            None => Ok(None),
        }
    }

    pub fn list_session_snapshots(&self) -> Result<Vec<ConversationSessionSnapshot>> {
        let sessions_dir = self.sessions_dir();
        if !sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut snapshots = Vec::new();
        for entry in fs::read_dir(&sessions_dir)
            .with_context(|| format!("failed to read sessions dir: {}", sessions_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let session = ConversationSession::load(&path)?;
            snapshots.push(session.snapshot().clone());
        }

        snapshots.sort_by(|left, right| {
            right
                .updated_at_unix_ms
                .cmp(&left.updated_at_unix_ms)
                .then_with(|| right.created_at_unix_ms.cmp(&left.created_at_unix_ms))
        });

        Ok(snapshots)
    }

    pub fn live_ui_snapshot_path(&self) -> PathBuf {
        Path::new(&self.transcript_dir).join("live_ui_snapshot.json")
    }

    fn sessions_dir(&self) -> PathBuf {
        Path::new(&self.transcript_dir).join("sessions")
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{session_id}.json"))
    }
}
