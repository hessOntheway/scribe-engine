use std::fs::{create_dir_all, read_to_string, write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agents::{AgentKind, default_agent_kind};
use crate::llm::usage::PromptCacheStats;

const SESSION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSessionSnapshot {
    pub schema_version: u32,
    #[serde(default = "default_agent_kind")]
    pub agent_kind: AgentKind,
    pub session_id: String,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub prompt_history: Vec<String>,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub prompt_cache_stats: PromptCacheStats,
}

pub struct ConversationSession {
    snapshot: ConversationSessionSnapshot,
    session_path: PathBuf,
}

impl ConversationSession {
    pub fn new(
        agent_kind: AgentKind,
        initial_prompt: String,
        system_prompt: &str,
        transcript_dir: &str,
    ) -> Result<Self> {
        Self::new_with_messages(
            agent_kind,
            vec![
                json!({"role": "system", "content": system_prompt}),
                json!({"role": "user", "content": initial_prompt.clone()}),
            ],
            vec![initial_prompt],
            transcript_dir,
        )
    }

    pub fn new_empty(
        agent_kind: AgentKind,
        system_prompt: &str,
        transcript_dir: &str,
    ) -> Result<Self> {
        Self::new_with_messages(
            agent_kind,
            vec![json!({"role": "system", "content": system_prompt})],
            Vec::new(),
            transcript_dir,
        )
    }

    pub fn new_with_messages(
        agent_kind: AgentKind,
        messages: Vec<Value>,
        prompt_history: Vec<String>,
        transcript_dir: &str,
    ) -> Result<Self> {
        let created_at_unix_ms = now_unix_ms()?;
        let session_id = format!("session_{}", created_at_unix_ms);

        Self::new_with_session_id(
            agent_kind,
            session_id,
            messages,
            prompt_history,
            transcript_dir,
        )
    }

    pub fn new_with_session_id(
        agent_kind: AgentKind,
        session_id: String,
        messages: Vec<Value>,
        prompt_history: Vec<String>,
        transcript_dir: &str,
    ) -> Result<Self> {
        let created_at_unix_ms = now_unix_ms()?;
        let snapshot = ConversationSessionSnapshot {
            schema_version: SESSION_SCHEMA_VERSION,
            agent_kind,
            session_id: session_id.clone(),
            created_at_unix_ms,
            updated_at_unix_ms: created_at_unix_ms,
            prompt_history,
            messages,
            prompt_cache_stats: PromptCacheStats::default(),
        };

        let session_path = session_snapshot_path(transcript_dir, agent_kind, &session_id);
        Ok(Self {
            snapshot,
            session_path,
        })
    }

    pub fn load(session_path: impl AsRef<Path>) -> Result<Self> {
        let session_path = session_path.as_ref().to_path_buf();
        let contents = read_to_string(&session_path)
            .with_context(|| format!("failed to read session file: {}", session_path.display()))?;
        let snapshot: ConversationSessionSnapshot = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse session file: {}", session_path.display()))?;

        if snapshot.schema_version != SESSION_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported session schema version {} in {}",
                snapshot.schema_version,
                session_path.display()
            );
        }

        Ok(Self {
            snapshot,
            session_path,
        })
    }

    pub fn messages_and_prompt_cache_stats_mut(
        &mut self,
    ) -> (&mut Vec<Value>, &mut PromptCacheStats) {
        let snapshot = &mut self.snapshot;
        (&mut snapshot.messages, &mut snapshot.prompt_cache_stats)
    }

    pub fn append_user_prompt(&mut self, prompt: String) {
        self.snapshot.prompt_history.push(prompt.clone());
        self.snapshot
            .messages
            .push(json!({"role": "user", "content": prompt}));
        self.snapshot.updated_at_unix_ms =
            now_unix_ms().unwrap_or(self.snapshot.updated_at_unix_ms);
    }

    pub fn truncate_to(&mut self, message_count: usize, prompt_history_count: usize) {
        self.snapshot.messages.truncate(message_count);
        self.snapshot.prompt_history.truncate(prompt_history_count);
        self.snapshot.updated_at_unix_ms =
            now_unix_ms().unwrap_or(self.snapshot.updated_at_unix_ms);
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.session_path.parent() {
            if !parent.as_os_str().is_empty() {
                create_dir_all(parent).with_context(|| {
                    format!("failed to create session dir: {}", parent.display())
                })?;
            }
        }

        let payload = serde_json::to_string_pretty(&self.snapshot)
            .context("failed to serialize conversation session")?;
        write(&self.session_path, payload).with_context(|| {
            format!(
                "failed to write session file: {}",
                self.session_path.display()
            )
        })?;
        Ok(())
    }

    pub fn snapshot(&self) -> &ConversationSessionSnapshot {
        &self.snapshot
    }
}

fn now_unix_ms() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock error")?
        .as_millis())
}

fn session_snapshot_path(transcript_dir: &str, agent_kind: AgentKind, session_id: &str) -> PathBuf {
    Path::new(transcript_dir)
        .join(agent_kind.as_str())
        .join("sessions")
        .join(format!("{session_id}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{read_to_string, write};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_transcript_dir() -> PathBuf {
        let seq = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "scribe-session-test-{seq}-{}",
            now_unix_ms().unwrap()
        ))
    }

    #[test]
    fn new_session_uses_agent_specific_path() {
        let dir = unique_transcript_dir();
        let session = ConversationSession::new(
            AgentKind::ProgrammerInterview,
            "hello".to_string(),
            "system",
            dir.to_str().unwrap(),
        )
        .expect("create session");

        session.save().expect("save session");

        let expected = dir
            .join("programmer_interview")
            .join("sessions")
            .join(format!("{}.json", session.snapshot().session_id));
        assert!(
            expected.exists(),
            "expected session at {}",
            expected.display()
        );
    }

    #[test]
    fn old_session_without_agent_kind_loads_as_materials_agent() {
        let dir = unique_transcript_dir();
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("old.json");
        write(
            &path,
            r#"{
              "schema_version": 1,
              "session_id": "session_old",
              "created_at_unix_ms": 1,
              "updated_at_unix_ms": 1,
              "prompt_history": ["hello"],
              "messages": [{"role": "system", "content": "system"}],
              "prompt_cache_stats": {
                "total_prompt_tokens": 0,
                "total_completion_tokens": 0,
                "total_tokens": 0,
                "request_count": 0,
                "local_cache_hits": 0
              }
            }"#,
        )
        .expect("write old session");

        let session = ConversationSession::load(&path).expect("load old session");
        assert_eq!(session.snapshot().agent_kind, AgentKind::InterviewMaterials);

        let saved = read_to_string(path).expect("read old session");
        assert!(saved.contains("session_old"));
    }

    #[test]
    fn truncate_to_removes_cancelled_turn_messages_and_prompts() {
        let dir = unique_transcript_dir();
        let mut session = ConversationSession::new_empty(
            AgentKind::InterviewMaterials,
            "system",
            dir.to_str().unwrap(),
        )
        .expect("create session");
        let message_count = session.snapshot().messages.len();
        let prompt_count = session.snapshot().prompt_history.len();

        session.append_user_prompt("typo prompt".to_string());
        session
            .messages_and_prompt_cache_stats_mut()
            .0
            .push(json!({"role": "assistant", "content": "partial answer"}));
        session.truncate_to(message_count, prompt_count);

        assert_eq!(session.snapshot().messages.len(), message_count);
        assert_eq!(session.snapshot().prompt_history.len(), prompt_count);
        assert!(session.snapshot().messages.iter().all(|message| {
            message.get("content").and_then(Value::as_str) != Some("partial answer")
        }));
    }
}
