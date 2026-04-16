mod cli;
mod compact;
mod config;
mod llm;
mod runtime;
mod tools;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;

use crate::cli::{Cli, Command};
use crate::config::{GithubConfig, LlmConfig};
use crate::llm::openai::OpenAiCompatClient;
use crate::runtime::ConversationRuntime;
use crate::tools::github_pages::GithubPagesClient;
use crate::tools::task::task_handler;
use crate::tools::{GlobalToolRegistry, mcp_plugin_tools_from_config};

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    match cli.command {
        Command::Tools => {
            let registry = build_registry()?;
            let defs = registry.definitions();
            println!(
                "{}",
                serde_json::to_string_pretty(&defs).context("failed to serialize tools")?
            );
            Ok(())
        }
        Command::ToolCall { name, input } => {
            let registry = build_registry()?;
            let output = registry.execute(&name, &input)?;
            println!("{}", output);
            Ok(())
        }
        Command::Publish {
            path,
            file,
            message,
        } => {
            let github = init_github_client()?;
            github.publish_post(&path, &file, &message)?;
            println!("successfully synced blog file: {}", path);
            Ok(())
        }
        Command::Update {
            path,
            file,
            message,
        } => {
            let github = init_github_client()?;
            github.update_post(&path, &file, &message)?;
            println!("successfully synced blog file: {}", path);
            Ok(())
        }
        Command::Ask { prompt, max_steps } => {
            let llm_cfg = LlmConfig::from_env()?;
            let llm = Arc::new(OpenAiCompatClient::new(llm_cfg)?);
            let base_registry = build_registry()?;
            let child_registry = Arc::new(base_registry.without_tool("task"));
            let task_tool = task_handler(
                Arc::new(crate::runtime::AgentLoop::new(Arc::clone(&llm), max_steps)),
                Arc::clone(&child_registry),
            );
            let parent_registry = Arc::new(base_registry.with_tool(task_tool)?);
            let runtime =
                ConversationRuntime::new(Arc::clone(&llm), Arc::clone(&parent_registry), max_steps);
            let answer = runtime.run_turn(&prompt)?;

            println!("{}", answer);
            Ok(())
        }
    }
}

fn build_registry() -> Result<GlobalToolRegistry> {
    let plugin_tools = mcp_plugin_tools_from_config().context("failed to load MCP plugin tools")?;
    GlobalToolRegistry::builtins().with_plugin_tools(plugin_tools)
}

fn init_github_client() -> Result<GithubPagesClient> {
    let github_cfg = GithubConfig::from_env()?;
    let github = GithubPagesClient::new(github_cfg)?;
    github.auth_check()?;
    Ok(github)
}
