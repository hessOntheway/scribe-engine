pub mod agents;
pub mod ask;
pub mod cli;
pub mod compact;
pub mod config;
pub mod llm;
pub mod runtime;
pub mod tools;
pub mod web;

use anyhow::{Context, Result};

use crate::ask::AskApp;
use crate::tools::{GlobalToolRegistry, mcp_plugin_tools_from_config};

pub fn build_registry() -> Result<GlobalToolRegistry> {
    let plugin_tools = mcp_plugin_tools_from_config().context("failed to load MCP plugin tools")?;
    GlobalToolRegistry::builtins().with_plugin_tools(plugin_tools)
}

pub fn ask_app_from_env(max_steps: usize) -> Result<AskApp> {
    AskApp::from_env(max_steps, build_registry()?)
}

pub fn github_auth_available() -> bool {
    std::env::var("GITHUB_USERNAME").is_ok() && std::env::var("GITHUB_PASSWORD").is_ok()
}
