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
    pub system_prompt: String,
    pub write_model_audit_log: bool,
    pub model_audit_log_path: String,
}

#[derive(Debug, Deserialize)]
struct LlmEnv {
    llm_api_key: String,
    #[serde(default = "default_llm_base_url")]
    llm_base_url: String,
    #[serde(default = "default_llm_model")]
    llm_model: String,
    #[serde(default = "default_llm_system_prompt")]
    llm_system_prompt: String,
    #[serde(default)]
    llm_write_model_audit_log: bool,
    #[serde(default = "default_llm_model_audit_log_path")]
    llm_model_audit_log_path: String,
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
            system_prompt: env.llm_system_prompt,
            write_model_audit_log: env.llm_write_model_audit_log,
            model_audit_log_path: env.llm_model_audit_log_path,
        })
    }
}

fn default_llm_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_llm_model() -> String {
    "gpt-4.1-mini".to_string()
}

fn default_llm_system_prompt() -> String {
    "You are a publishing assistant. Use github_pages_publish only when the user explicitly asks to publish or update blog content. Use glob_search for filenames, paths, and directory structure. Use grep_search for file content searches. When calling tools, output strict JSON arguments only; all string values must be quoted JSON strings.".to_string()
}

fn default_llm_model_audit_log_path() -> String {
    ".auditlog/llm_response_audit.json".to_string()
}
