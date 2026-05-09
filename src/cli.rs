use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "my_claw",
    version,
    about = "Scribe Engine local web runtime and tool interface"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print model-facing tool registrations as JSON.
    Tools,
    /// Execute a model tool call by name and JSON input.
    ToolCall {
        /// Tool name. Builtin tools and MCP-loaded tools are supported.
        #[arg(long)]
        name: String,
        /// JSON tool input payload.
        #[arg(long)]
        input: String,
    },
    /// Start a local web UI backed by the same ask runtime.
    Serve {
        /// Host to bind the local web server to.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind the local web server to.
        #[arg(long, default_value_t = 3000)]
        port: u16,
        /// Maximum model-tool turns per user message.
        #[arg(long, default_value_t = 6)]
        max_steps: usize,
    },
}
