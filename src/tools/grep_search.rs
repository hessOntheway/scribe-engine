use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use walkdir::WalkDir;

use super::{ToolDefinition, ToolExecutor, ToolHandler};

#[derive(Debug, Deserialize)]
struct GrepSearchInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default = "default_true")]
    is_regexp: bool,
    #[serde(default = "default_true")]
    line_numbers: bool,
    #[serde(default = "default_head_limit")]
    head_limit: usize,
}

fn default_true() -> bool {
    true
}

fn default_head_limit() -> usize {
    200
}

#[derive(Debug, Serialize)]
struct GrepSearchMatch {
    path: String,
    line: Option<usize>,
    text: String,
}

#[derive(Debug, Serialize)]
struct GrepSearchOutput {
    query: String,
    total_matches: usize,
    truncated: bool,
    matches: Vec<GrepSearchMatch>,
}

pub fn grep_search_handler() -> ToolHandler {
    let definition = ToolDefinition {
        name: "grep_search".to_string(),
        description: "Search file contents with a regex pattern for code discovery.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Required search pattern as a JSON string. Example: \".*\" or \"README\". Use plain text when is_regexp=false."
                },
                "path": {
                    "type": "string",
                    "description": "Root path to search recursively. Example: \".\". Defaults to current directory."
                },
                "glob": {
                    "type": "string",
                    "description": "Optional glob filter as a JSON string. Example: \"**/*.rs\""
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case-insensitive matching. Use true or false."
                },
                "is_regexp": {
                    "type": "boolean",
                    "description": "If false, pattern is treated as plain text and escaped before matching. Use true or false."
                },
                "line_numbers": {
                    "type": "boolean",
                    "description": "Include matched line number. Use true or false."
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Maximum number of matches returned. Use an integer such as 10 or 30."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = std::sync::Arc::new(move |input_json: &str| {
        let input: GrepSearchInput =
            serde_json::from_str(input_json).context("invalid input JSON for grep_search; arguments must be strict JSON, for example {\"pattern\": \".*\", \"glob\": \"**/Cargo.toml\", \"is_regexp\": false, \"head_limit\": 30}")?;
        let output = run_grep_search(&input)?;
        serde_json::to_string_pretty(&output).context("failed to serialize grep_search output")
    });

    ToolHandler::new(definition, execute)
}

fn run_grep_search(input: &GrepSearchInput) -> Result<GrepSearchOutput> {
    let root = input.path.clone().unwrap_or_else(|| ".".to_string());
    let pattern = if input.is_regexp {
        input.pattern.clone()
    } else {
        regex::escape(&input.pattern)
    };

    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(input.case_insensitive)
        .build()
        .with_context(|| format!("invalid regex pattern: {}", input.pattern))?;

    let glob_set = if let Some(glob) = &input.glob {
        let mut builder = GlobSetBuilder::new();
        builder.add(Glob::new(glob).with_context(|| format!("invalid glob: {}", glob))?);
        Some(builder.build().context("failed to compile glob filter")?)
    } else {
        None
    };

    let mut matches = Vec::new();
    for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }

        let rel_or_abs = entry.path().to_string_lossy().to_string();
        if rel_or_abs.contains("/.git/") || rel_or_abs.contains("/target/") {
            continue;
        }

        if let Some(set) = &glob_set {
            if !set.is_match(entry.path()) {
                continue;
            }
        }

        let content = match std::fs::read_to_string(entry.path()) {
            Ok(v) => v,
            Err(_) => continue,
        };

        for (idx, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(GrepSearchMatch {
                    path: rel_or_abs.clone(),
                    line: if input.line_numbers {
                        Some(idx + 1)
                    } else {
                        None
                    },
                    text: line.to_string(),
                });
            }

            if matches.len() >= input.head_limit {
                return Ok(GrepSearchOutput {
                    query: input.pattern.clone(),
                    total_matches: matches.len(),
                    truncated: true,
                    matches,
                });
            }
        }
    }

    Ok(GrepSearchOutput {
        query: input.pattern.clone(),
        total_matches: matches.len(),
        truncated: false,
        matches,
    })
}
