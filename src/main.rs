use anyhow::{Context, Result};
use clap::Parser;

use my_claw::cli::{Cli, Command};
use my_claw::{ask_app_from_env, build_registry, web};

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
            let ask_app = ask_app_from_env(max_steps)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build tokio runtime")?;
            runtime.block_on(web::serve(ask_app, host, port))
        }
    }
}
