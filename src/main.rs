mod ask;
mod cli;
mod compact;
mod config;
mod llm;
mod runtime;
mod tools;
mod web;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Command};
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
        Command::Serve {
            host,
            port,
            max_steps,
        } => {
            let ask_app = ask::AskApp::from_env(max_steps, build_registry()?)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build tokio runtime")?;
            runtime.block_on(web::serve(ask_app, host, port))
        }
    }
}

fn build_registry() -> Result<GlobalToolRegistry> {
    let plugin_tools = mcp_plugin_tools_from_config().context("failed to load MCP plugin tools")?;
    GlobalToolRegistry::builtins().with_plugin_tools(plugin_tools)
}

fn github_auth_available() -> bool {
    std::env::var("GITHUB_USERNAME").is_ok() && std::env::var("GITHUB_PASSWORD").is_ok()
}
