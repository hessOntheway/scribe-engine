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
use crate::llm::session::ConversationSession;
use crate::runtime::ConversationRuntime;
use crate::tools::github_wiki::GithubWikiClient;
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
            github.publish_page(&path, &file, &message)?;
            println!("successfully synced wiki file: {}", path);
            Ok(())
        }
        Command::Update {
            path,
            file,
            message,
        } => {
            let github = init_github_client()?;
            github.update_page(&path, &file, &message)?;
            println!("successfully synced wiki file: {}", path);
            Ok(())
        }
        Command::Ask { max_steps } => {
            use std::io::{self, BufRead};

            let llm_cfg = LlmConfig::from_env()?;
            let llm = Arc::new(OpenAiCompatClient::new(llm_cfg)?);
            let transcript_dir = llm.context_compact_config().transcript_dir.clone();

            let base_registry = build_registry()?;
            let ask_registry = if github_auth_available() {
                base_registry
            } else {
                base_registry.without_tool("github_wiki_publish")
            };

            let shared_agent_loop = Arc::new(crate::runtime::AgentLoop::new(Arc::clone(&llm), max_steps));
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
            let parent_registry = Arc::new(parent_registry);
            let runtime = ConversationRuntime::new(Arc::clone(&llm), Arc::clone(&parent_registry), max_steps);

            let stdin = io::stdin();
            let mut lines = stdin.lock().lines();
            println!("Enter your request, then press Enter:");
            let first_prompt = match lines.next() {
                Some(Ok(user_input)) if !user_input.trim().is_empty() => user_input,
                _ => return Ok(()),
            };

            let mut session = match std::env::var("ASK_SESSION_PATH") {
                Ok(session_path) => {
                    let mut loaded = ConversationSession::load(session_path)?;
                    loaded.append_user_prompt(first_prompt);
                    loaded
                }
                Err(_) => ConversationSession::new(first_prompt, llm.system_prompt(), &transcript_dir)?,
            };
            // soft limits disabled for now; controlling via --max-steps only
            session.save()?;

            loop {
                // no session-level soft-limit enforcement; per-turn max_steps controls execution
                let answer = runtime.run_session_turn(&mut session)?;
                println!("{}", answer);
                session.save()?;

                let awaiting_follow_up = ConversationSession::extract_user_prompt(&answer).is_some();
                if awaiting_follow_up {
                    println!("\n(Waiting for your input. Type your response and press Enter.)");
                } else {
                    println!("\n(Enter a new request to continue the same session, or an empty line to exit.)");
                }

                match lines.next() {
                    Some(Ok(user_input)) if !user_input.trim().is_empty() => {
                        session.append_user_prompt(user_input);
                        session.save()?;
                        continue;
                    }
                    _ => break,
                }
            }

            Ok(())
        }
    }
}

fn build_registry() -> Result<GlobalToolRegistry> {
    let plugin_tools = mcp_plugin_tools_from_config().context("failed to load MCP plugin tools")?;
    GlobalToolRegistry::builtins().with_plugin_tools(plugin_tools)
}

fn init_github_client() -> Result<GithubWikiClient> {
    let github_cfg = GithubConfig::from_env()?;
    let github = GithubWikiClient::new(github_cfg)?;
    github.auth_check()?;
    Ok(github)
}

fn github_auth_available() -> bool {
    std::env::var("GITHUB_USERNAME").is_ok() && std::env::var("GITHUB_PASSWORD").is_ok()
}
