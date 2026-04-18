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
use crate::tools::task::{task_handler, task_query_handlers};
use crate::tools::{
    GlobalToolRegistry, TaskRegistry, TeamManager, mcp_plugin_tools_from_config,
    team_tool_handlers,
};

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
            let shared_agent_loop = Arc::new(crate::runtime::AgentLoop::new(Arc::clone(&llm), max_steps));
            let task_registry = Arc::new(TaskRegistry::new());

            let team_child_registry = Arc::new(base_registry.without_tool("task"));
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

            let mut parent_registry = base_registry.with_tool(task_tool)?;
            for tool in task_query_tools {
                parent_registry = parent_registry.with_tool(tool)?;
            }
            for tool in team_tools {
                parent_registry = parent_registry.with_tool(tool)?;
            }
            let parent_registry = Arc::new(parent_registry);
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
