use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct GithubConfig {
    pub username: String,
    pub password: String,
    pub owner: String,
    pub repo: String,
    pub branch: String,
}

#[derive(Debug, Deserialize)]
struct GithubEnv {
    github_username: String,
    github_password: String,
    #[serde(default = "default_branch")]
    github_pages_branch: String,
    #[serde(default)]
    github_pages_owner: Option<String>,
}

impl GithubConfig {
    pub fn from_env() -> Result<Self> {
        let env: GithubEnv = envy::prefixed("")
            .from_env()
            .context("failed to read env vars (need GITHUB_USERNAME/GITHUB_PASSWORD)")?;
        let owner = env
            .github_pages_owner
            .clone()
            .unwrap_or_else(|| env.github_username.clone());
        let repo = format!("{}.github.io", owner);

        Ok(Self {
            username: env.github_username,
            password: env.github_password,
            owner,
            repo,
            branch: env.github_pages_branch,
        })
    }
}

fn default_branch() -> String {
    "main".to_string()
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub write_model_audit_log: bool,
    pub model_audit_log_path: String,
    pub context_compact: ContextCompactConfig,
}

#[derive(Debug, Clone)]
pub struct ContextCompactConfig {
    pub enabled: bool,
    pub micro_keep_recent_tool_results: usize,
    pub micro_min_tool_result_chars: usize,
    pub auto_token_threshold: usize,
    pub auto_preserve_recent_messages: usize,
    pub transcript_dir: String,
}

#[derive(Debug, Deserialize)]
struct LlmEnv {
    llm_api_key: String,
    #[serde(default = "default_llm_base_url")]
    llm_base_url: String,
    #[serde(default = "default_llm_model")]
    llm_model: String,
    #[serde(default)]
    llm_write_model_audit_log: bool,
    #[serde(default = "default_llm_model_audit_log_path")]
    llm_model_audit_log_path: String,
    #[serde(default = "default_context_compact_enabled")]
    llm_context_compact_enabled: bool,
    #[serde(default = "default_micro_keep_recent_tool_results")]
    llm_micro_compact_keep_recent_tool_results: usize,
    #[serde(default = "default_micro_min_tool_result_chars")]
    llm_micro_compact_min_tool_result_chars: usize,
    #[serde(default = "default_auto_compact_token_threshold")]
    llm_auto_compact_token_threshold: usize,
    #[serde(default = "default_auto_compact_preserve_recent_messages")]
    llm_auto_compact_preserve_recent_messages: usize,
    #[serde(default = "default_context_compact_transcript_dir")]
    llm_context_compact_transcript_dir: String,
}

impl LlmConfig {
    pub fn from_env() -> Result<Self> {
        let env: LlmEnv = envy::prefixed("")
            .from_env()
            .context("failed to read env vars (need LLM_API_KEY)")?;

        Ok(Self {
            api_key: env.llm_api_key,
            base_url: env.llm_base_url.trim_end_matches('/').to_string(),
            model: env.llm_model,
            write_model_audit_log: env.llm_write_model_audit_log,
            model_audit_log_path: env.llm_model_audit_log_path,
            context_compact: ContextCompactConfig {
                enabled: env.llm_context_compact_enabled,
                micro_keep_recent_tool_results: env.llm_micro_compact_keep_recent_tool_results,
                micro_min_tool_result_chars: env.llm_micro_compact_min_tool_result_chars,
                auto_token_threshold: env.llm_auto_compact_token_threshold,
                auto_preserve_recent_messages: env.llm_auto_compact_preserve_recent_messages,
                transcript_dir: env.llm_context_compact_transcript_dir,
            },
        })
    }
}

fn default_llm_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_llm_model() -> String {
    "gpt-4.1-mini".to_string()
}

fn default_llm_model_audit_log_path() -> String {
    ".auditlog/llm_response_audit.json".to_string()
}

fn default_context_compact_enabled() -> bool {
    true
}

fn default_micro_keep_recent_tool_results() -> usize {
    3
}

fn default_micro_min_tool_result_chars() -> usize {
    100
}

fn default_auto_compact_token_threshold() -> usize {
    50_000
}

fn default_auto_compact_preserve_recent_messages() -> usize {
    4
}

fn default_context_compact_transcript_dir() -> String {
    ".transcripts".to_string()
}
