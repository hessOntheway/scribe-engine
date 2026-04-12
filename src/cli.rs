use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "my_claw",
    version,
    about = "Restricted GitHub Pages blog publisher"
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
        /// Tool name. Only github_pages_publish is supported.
        #[arg(long)]
        name: String,
        /// JSON tool input payload.
        #[arg(long)]
        input: String,
    },
    /// Publish a new blog post to GitHub Pages.
    Publish {
        /// Blog file path in the Pages repo. Must be under posts/ and end with .md
        #[arg(long)]
        path: String,
        /// Local markdown file to upload
        #[arg(long)]
        file: String,
        /// Commit message
        #[arg(long, default_value = "publish blog post")]
        message: String,
    },
    /// Update an existing blog post on GitHub Pages.
    Update {
        /// Blog file path in the Pages repo. Must be under posts/ and end with .md
        #[arg(long)]
        path: String,
        /// Local markdown file to upload
        #[arg(long)]
        file: String,
        /// Commit message
        #[arg(long, default_value = "update blog post")]
        message: String,
    },
    /// Ask the model to plan/execute with registered tools.
    Ask {
        /// User prompt sent to the model.
        #[arg(long)]
        prompt: String,
        /// Maximum model-tool turns.
        #[arg(long, default_value_t = 6)]
        max_steps: usize,
    },
}
