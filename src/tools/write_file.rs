use std::fs;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

use super::{ToolDefinition, ToolExecutor, ToolHandler};

#[derive(Debug, Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

pub fn write_file_handler() -> ToolHandler {
    let definition = ToolDefinition {
        name: "write_file".to_string(),
        description: "Write markdown or text content to a workspace-relative file".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative output path. Absolute paths and '..' are rejected."
                },
                "content": {
                    "type": "string",
                    "description": "File content to write."
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = std::sync::Arc::new(move |input_json: &str| {
        let input: WriteFileInput = serde_json::from_str(input_json)
            .context("invalid input JSON for write_file; expected {\"path\": \"docs/report.md\", \"content\": \"...\"}")?;

        let path = validate_write_path(&input.path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent directories for {}", path.display()))?;
        }

        fs::write(&path, input.content)
            .with_context(|| format!("failed to write file {}", path.display()))?;

        Ok(serde_json::to_string_pretty(&json!({
            "ok": true,
            "path": path.display().to_string(),
        }))
        .context("failed to encode write_file output")?)
    });

    ToolHandler::new(definition, execute)
}

fn validate_write_path(path: &str) -> Result<std::path::PathBuf> {
    if path.starts_with('/') || path.contains("..") {
        bail!("invalid write_file path: absolute paths and '..' are not allowed");
    }

    let normalized = std::path::Path::new(path).to_path_buf();
    if normalized.as_os_str().is_empty() {
        bail!("write_file path cannot be empty");
    }

    Ok(normalized)
}