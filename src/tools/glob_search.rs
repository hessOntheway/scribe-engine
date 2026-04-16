use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use globset::Glob;
use serde::{Deserialize, Serialize};
use serde_json::json;
use walkdir::WalkDir;

use super::{ToolDefinition, ToolExecutor, ToolHandler};

#[derive(Debug, Deserialize)]
struct GlobSearchInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default = "default_head_limit")]
    head_limit: usize,
}

fn default_head_limit() -> usize {
    100
}

#[derive(Debug, Serialize)]
struct GlobSearchMatch {
    path: String,
    kind: String,
}

#[derive(Debug, Serialize)]
struct GlobSearchOutput {
    query: String,
    total_matches: usize,
    truncated: bool,
    duration_ms: u128,
    matches: Vec<GlobSearchMatch>,
}

pub fn glob_search_handler() -> ToolHandler {
    let definition = ToolDefinition {
        name: "glob_search".to_string(),
        description: "Search for files and directories by glob pattern for path discovery."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern for file or directory paths. Example: \"src/*\" or \"**/*.rs\"."
                },
                "path": {
                    "type": "string",
                    "description": "Base path to search from. Example: \".\". Defaults to current directory."
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Maximum number of matches returned. Use an integer such as 20 or 100."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = std::sync::Arc::new(move |input_json: &str| {
        let input: GlobSearchInput =
            serde_json::from_str(input_json).context("invalid input JSON for glob_search")?;
        let output = run_glob_search(&input)?;
        serde_json::to_string_pretty(&output).context("failed to serialize glob_search output")
    });

    ToolHandler::new(definition, execute)
}

fn run_glob_search(input: &GlobSearchInput) -> Result<GlobSearchOutput> {
    let started_at = Instant::now();
    let root = resolve_root(input.path.as_deref())?;
    let matcher = Glob::new(&input.pattern)
        .with_context(|| format!("invalid glob pattern: {}", input.pattern))?
        .compile_matcher();

    let mut matches = Vec::new();
    let mut truncated = false;

    if root.is_file() {
        if let Some(file_name) = root.file_name() {
            if matcher.is_match(file_name) {
                matches.push(GlobSearchMatch {
                    path: file_name.to_string_lossy().to_string(),
                    kind: "file".to_string(),
                });
            }
        }

        return Ok(GlobSearchOutput {
            query: input.pattern.clone(),
            total_matches: matches.len(),
            truncated,
            duration_ms: started_at.elapsed().as_millis(),
            matches,
        });
    }

    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if entry.depth() == 0 {
            continue;
        }

        let relative_path = entry.path().strip_prefix(&root).unwrap_or(entry.path());

        if matcher.is_match(relative_path) {
            let kind = if entry.file_type().is_dir() {
                "dir"
            } else {
                "file"
            };
            matches.push(GlobSearchMatch {
                path: relative_path.to_string_lossy().to_string(),
                kind: kind.to_string(),
            });

            if matches.len() >= input.head_limit {
                truncated = true;
                break;
            }
        }
    }

    Ok(GlobSearchOutput {
        query: input.pattern.clone(),
        total_matches: matches.len(),
        truncated,
        duration_ms: started_at.elapsed().as_millis(),
        matches,
    })
}

fn resolve_root(path: Option<&str>) -> Result<PathBuf> {
    let candidate = match path {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from("."),
    };

    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        std::env::current_dir()
            .context("failed to get current directory")?
            .join(candidate)
    };

    std::fs::canonicalize(&absolute)
        .with_context(|| format!("failed to resolve path: {}", absolute.display()))
}
