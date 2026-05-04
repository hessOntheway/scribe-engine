use std::fs::{create_dir_all, read_to_string, write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const USER_PROMPT_PREFIX: &str = "USER_PROMPT:";
const SESSION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSessionSnapshot {
    pub schema_version: u32,
    pub session_id: String,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub prompt_history: Vec<String>,
    pub messages: Vec<Value>,
}

pub struct ConversationSession {
    snapshot: ConversationSessionSnapshot,
    session_path: PathBuf,
}

impl ConversationSession {
    pub fn new(initial_prompt: String, system_prompt: &str, transcript_dir: &str) -> Result<Self> {
        let created_at_unix_ms = now_unix_ms()?;
        let session_id = format!("session_{}", created_at_unix_ms);

        let mut messages = Vec::new();
        messages.push(json!({"role": "system", "content": system_prompt}));
        messages.push(json!({"role": "user", "content": initial_prompt.clone()}));

        let snapshot = ConversationSessionSnapshot {
            schema_version: SESSION_SCHEMA_VERSION,
            session_id: session_id.clone(),
            created_at_unix_ms,
            updated_at_unix_ms: created_at_unix_ms,
            prompt_history: vec![initial_prompt],
            messages,
        };

        let session_path = session_snapshot_path(transcript_dir, &session_id);
        Ok(Self { snapshot, session_path })
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

        Ok(Self { snapshot, session_path })
    }

    pub fn messages_mut(&mut self) -> &mut Vec<Value> {
        &mut self.snapshot.messages
    }

    pub fn append_user_prompt(&mut self, prompt: String) {
        self.snapshot.prompt_history.push(prompt.clone());
        self.snapshot.messages.push(json!({"role": "user", "content": prompt}));
        self.snapshot.updated_at_unix_ms = now_unix_ms().unwrap_or(self.snapshot.updated_at_unix_ms);
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.session_path.parent() {
            if !parent.as_os_str().is_empty() {
                create_dir_all(parent)
                    .with_context(|| format!("failed to create session dir: {}", parent.display()))?;
            }
        }

        let payload = serde_json::to_string_pretty(&self.snapshot)
            .context("failed to serialize conversation session")?;
        write(&self.session_path, payload)
            .with_context(|| format!("failed to write session file: {}", self.session_path.display()))?;
        Ok(())
    }

    pub fn extract_user_prompt(reply: &str) -> Option<String> {
        for line in reply.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix(USER_PROMPT_PREFIX) {
                let prompt = rest.trim();
                if prompt.is_empty() {
                    return Some(String::new());
                }
                return Some(prompt.to_string());
            }
        }
        None
    }
}

fn now_unix_ms() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock error")?
        .as_millis())
}

fn session_snapshot_path(transcript_dir: &str, session_id: &str) -> PathBuf {
    Path::new(transcript_dir)
        .join("sessions")
        .join(format!("{session_id}.json"))
}