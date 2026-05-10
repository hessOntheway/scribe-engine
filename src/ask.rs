use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agents::AgentKind;
use crate::config::LlmConfig;
use crate::llm::openai::OpenAiCompatClient;
use crate::llm::session::{ConversationSession, ConversationSessionSnapshot};
use crate::runtime::{AgentLoop, ConversationRuntime, RuntimeEventSink};
use crate::tools::task::{task_handler, task_query_handlers};
use crate::tools::{GlobalToolRegistry, TaskRegistry, TeamManager, team_tool_handlers};

#[derive(Clone)]
pub struct AskApp {
    materials_runtime: Arc<ConversationRuntime>,
    interviewer_runtime: Arc<ConversationRuntime>,
    transcript_dir: String,
}

impl AskApp {
    pub fn new(
        materials_runtime: Arc<ConversationRuntime>,
        interviewer_runtime: Arc<ConversationRuntime>,
        transcript_dir: String,
    ) -> Self {
        Self {
            materials_runtime,
            interviewer_runtime,
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

        let materials_runtime = Arc::new(ConversationRuntime::new(
            Arc::clone(&llm),
            Arc::new(parent_registry),
            max_steps,
        ));

        let interviewer_runtime = Arc::new(ConversationRuntime::new(
            Arc::clone(&llm),
            Arc::new(GlobalToolRegistry::empty()),
            max_steps,
        ));

        Ok(Self::new(
            materials_runtime,
            interviewer_runtime,
            transcript_dir,
        ))
    }

    pub fn new_session(
        &self,
        agent_kind: AgentKind,
        initial_prompt: String,
    ) -> Result<ConversationSession> {
        let session = ConversationSession::new(
            agent_kind,
            initial_prompt,
            agent_kind.system_prompt(),
            &self.transcript_dir,
        )?;
        session.save()?;
        Ok(session)
    }

    pub fn new_empty_session(&self, agent_kind: AgentKind) -> Result<ConversationSession> {
        let session = ConversationSession::new_empty(
            agent_kind,
            agent_kind.system_prompt(),
            &self.transcript_dir,
        )?;
        session.save()?;
        Ok(session)
    }

    pub fn new_seeded_interview_session(
        &self,
        session_id: String,
        materials: &str,
        initial_prompt: String,
    ) -> Result<ConversationSession> {
        let context = format!(
            "Use the following interview materials as your only codebase context. Do not inspect the repository directly.\n\n---\n\n{materials}"
        );
        let mut session = ConversationSession::new_with_session_id(
            AgentKind::ProgrammerInterview,
            session_id,
            vec![
                serde_json::json!({"role": "system", "content": AgentKind::ProgrammerInterview.system_prompt()}),
                serde_json::json!({"role": "user", "content": context}),
                serde_json::json!({"role": "assistant", "content": "Understood. I will interview the programmer using only these materials."}),
            ],
            Vec::new(),
            &self.transcript_dir,
        )?;
        session.append_user_prompt(initial_prompt);
        session.save()?;
        Ok(session)
    }

    pub fn run_session_turn_with_events(
        &self,
        agent_kind: AgentKind,
        session: &mut ConversationSession,
        event_sink: Option<RuntimeEventSink>,
    ) -> Result<String> {
        let runtime = match agent_kind {
            AgentKind::InterviewMaterials => &self.materials_runtime,
            AgentKind::ProgrammerInterview => &self.interviewer_runtime,
        };
        let answer = runtime.run_session_turn_with_events(session, event_sink)?;
        session.save()?;
        Ok(answer)
    }

    pub fn load_session(
        &self,
        agent_kind: AgentKind,
        session_id: &str,
    ) -> Result<ConversationSession> {
        let path = self.session_path(agent_kind, session_id);
        if !path.exists() && agent_kind == AgentKind::InterviewMaterials {
            let legacy_path = self.legacy_session_path(session_id);
            if legacy_path.exists() {
                return ConversationSession::load(legacy_path);
            }
        }
        ConversationSession::load(path)
    }

    pub fn load_latest_session(
        &self,
        agent_kind: AgentKind,
    ) -> Result<Option<ConversationSession>> {
        let latest = self
            .list_session_snapshots(agent_kind)?
            .into_iter()
            .next()
            .map(|snapshot| snapshot.session_id);
        match latest {
            Some(session_id) => self.load_session(agent_kind, &session_id).map(Some),
            None => Ok(None),
        }
    }

    pub fn list_session_snapshots(
        &self,
        agent_kind: AgentKind,
    ) -> Result<Vec<ConversationSessionSnapshot>> {
        let mut snapshots = Vec::new();
        for sessions_dir in self.session_dirs(agent_kind) {
            if !sessions_dir.exists() {
                continue;
            }

            for entry in fs::read_dir(&sessions_dir).with_context(|| {
                format!("failed to read sessions dir: {}", sessions_dir.display())
            })? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                let session = ConversationSession::load(&path)?;
                if session.snapshot().agent_kind == agent_kind {
                    snapshots.push(session.snapshot().clone());
                }
            }
        }

        snapshots.sort_by(|left, right| {
            right
                .updated_at_unix_ms
                .cmp(&left.updated_at_unix_ms)
                .then_with(|| right.created_at_unix_ms.cmp(&left.created_at_unix_ms))
        });

        Ok(snapshots)
    }

    pub fn live_ui_snapshot_path(&self, agent_kind: AgentKind) -> PathBuf {
        self.agent_dir(agent_kind).join("live_ui_snapshot.json")
    }

    pub fn workflow_snapshot_path(&self) -> PathBuf {
        Path::new(&self.transcript_dir).join("workflow_snapshot.json")
    }

    pub fn latest_materials_path(&self) -> PathBuf {
        self.agent_dir(AgentKind::InterviewMaterials)
            .join("latest_materials.md")
    }

    pub fn session_materials_path(&self, session_id: &str) -> PathBuf {
        self.agent_dir(AgentKind::InterviewMaterials)
            .join("materials")
            .join(format!("{session_id}.md"))
    }

    pub fn latest_report_path(&self) -> PathBuf {
        self.agent_dir(AgentKind::ProgrammerInterview)
            .join("latest_evaluation_report.md")
    }

    pub fn session_report_path(&self, session_id: &str) -> PathBuf {
        self.agent_dir(AgentKind::ProgrammerInterview)
            .join("reports")
            .join(format!("{session_id}.md"))
    }

    fn agent_dir(&self, agent_kind: AgentKind) -> PathBuf {
        Path::new(&self.transcript_dir).join(agent_kind.as_str())
    }

    fn session_dirs(&self, agent_kind: AgentKind) -> Vec<PathBuf> {
        let mut dirs = vec![self.agent_dir(agent_kind).join("sessions")];
        if agent_kind == AgentKind::InterviewMaterials {
            dirs.push(Path::new(&self.transcript_dir).join("sessions"));
        }
        dirs
    }

    fn session_path(&self, agent_kind: AgentKind, session_id: &str) -> PathBuf {
        self.agent_dir(agent_kind)
            .join("sessions")
            .join(format!("{session_id}.json"))
    }

    fn legacy_session_path(&self, session_id: &str) -> PathBuf {
        Path::new(&self.transcript_dir)
            .join("sessions")
            .join(format!("{session_id}.json"))
    }
}
