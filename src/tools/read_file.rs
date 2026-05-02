use std::fs::read_to_string;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ToolDefinition, ToolExecutor, ToolHandler};

#[derive(Debug, Deserialize)]
struct ReadFileInput {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ReadFileLine {
    line: usize,
    text: String,
}

#[derive(Debug, Serialize)]
struct ReadFileOutput {
    path: String,
    resolved_path: String,
    start_line: usize,
    end_line: usize,
    total_lines: usize,
    lines: Vec<ReadFileLine>,
}

pub fn read_file_handler() -> ToolHandler {
    let definition = ToolDefinition {
        name: "read_file".to_string(),
        description: "Read a local file from the workspace, optionally with line bounds, for exact code inspection.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative file path. Absolute paths and '..' segments are rejected."
                },
                "start_line": {
                    "type": "integer",
                    "description": "Optional 1-based start line, inclusive. Defaults to the first line."
                },
                "end_line": {
                    "type": "integer",
                    "description": "Optional 1-based end line, inclusive. Defaults to the end of the file."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = std::sync::Arc::new(move |input_json: &str| {
        let input: ReadFileInput = serde_json::from_str(input_json)
            .context("invalid input JSON for read_file; expected {\"path\": \"src/main.rs\", \"start_line\": 1, \"end_line\": 50}")?;
        let output = run_read_file(&input)?;
        serde_json::to_string_pretty(&output).context("failed to serialize read_file output")
    });

    ToolHandler::new(definition, execute)
}

fn run_read_file(input: &ReadFileInput) -> Result<ReadFileOutput> {
    let workspace_root = std::env::current_dir().context("failed to resolve workspace root")?;
    let workspace_root = workspace_root
        .canonicalize()
        .context("failed to canonicalize workspace root")?;

    let resolved_path = resolve_file_path(&workspace_root, &input.path)?;
    let content = read_to_string(&resolved_path)
        .with_context(|| format!("failed to read file: {}", resolved_path.display()))?;

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let start_line = input.start_line.unwrap_or(1);
    let end_line = input.end_line.unwrap_or(total_lines.max(start_line));

    if start_line == 0 || end_line == 0 {
        bail!("start_line and end_line must be 1-based positive integers");
    }
    if start_line > end_line {
        bail!("start_line must be less than or equal to end_line");
    }
    if total_lines == 0 {
        return Ok(ReadFileOutput {
            path: input.path.clone(),
            resolved_path: resolved_path.display().to_string(),
            start_line,
            end_line,
            total_lines,
            lines: Vec::new(),
        });
    }
    if start_line > total_lines {
        bail!("start_line {} is beyond file length {}", start_line, total_lines);
    }

    let clamped_end_line = end_line.min(total_lines);
    let selected_lines = lines[(start_line - 1)..clamped_end_line]
        .iter()
        .enumerate()
        .map(|(idx, text)| ReadFileLine {
            line: start_line + idx,
            text: (*text).to_string(),
        })
        .collect();

    Ok(ReadFileOutput {
        path: input.path.clone(),
        resolved_path: resolved_path.display().to_string(),
        start_line,
        end_line: clamped_end_line,
        total_lines,
        lines: selected_lines,
    })
}

fn resolve_file_path(workspace_root: &Path, path: &str) -> Result<PathBuf> {
    let candidate = Path::new(path);

    if candidate.is_absolute() {
        bail!("absolute paths are not allowed");
    }

    if candidate.components().any(|component| matches!(component, Component::ParentDir)) {
        bail!("'..' path segments are not allowed");
    }

    let joined = workspace_root.join(candidate);
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("failed to resolve path: {}", joined.display()))?;

    if !canonical.starts_with(workspace_root) {
        bail!("path escapes the workspace root");
    }

    if !canonical.is_file() {
        bail!("path does not point to a regular file: {}", canonical.display());
    }

    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};

    fn unique_workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scribe-read-file-test-{}", uuid::Uuid::new_v4()));
        create_dir_all(&dir).expect("create temp workspace");
        dir
    }

    #[test]
    fn read_file_reads_full_file() {
        let workspace = unique_workspace();
        let file_path = workspace.join("sample.txt");
        write(&file_path, "alpha\nbeta\ngamma\n").expect("write file");

        let input = ReadFileInput {
            path: "sample.txt".to_string(),
            start_line: None,
            end_line: None,
        };

        let previous_dir = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(&workspace).expect("set current dir");
        let output = run_read_file(&input).expect("read file");
        std::env::set_current_dir(previous_dir).expect("restore current dir");

        assert_eq!(output.total_lines, 3);
        assert_eq!(output.lines.len(), 3);
        assert_eq!(output.lines[1].text, "beta");
    }

    #[test]
    fn read_file_reads_requested_range() {
        let workspace = unique_workspace();
        let file_path = workspace.join("sample.txt");
        write(&file_path, "alpha\nbeta\ngamma\ndelta\n").expect("write file");

        let input = ReadFileInput {
            path: "sample.txt".to_string(),
            start_line: Some(2),
            end_line: Some(3),
        };

        let previous_dir = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(&workspace).expect("set current dir");
        let output = run_read_file(&input).expect("read file");
        std::env::set_current_dir(previous_dir).expect("restore current dir");

        assert_eq!(output.start_line, 2);
        assert_eq!(output.end_line, 3);
        assert_eq!(output.lines.len(), 2);
        assert_eq!(output.lines[0].text, "beta");
        assert_eq!(output.lines[1].text, "gamma");
    }

    #[test]
    fn read_file_rejects_path_escape() {
        let workspace = unique_workspace();
        let input = ReadFileInput {
            path: "../Cargo.toml".to_string(),
            start_line: None,
            end_line: None,
        };

        let previous_dir = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(&workspace).expect("set current dir");
        let err = run_read_file(&input).expect_err("path escape should fail");
        std::env::set_current_dir(previous_dir).expect("restore current dir");

        assert!(err.to_string().contains("'..' path segments are not allowed"));
    }
}