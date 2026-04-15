use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, create_dir_all};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::ContextCompactConfig;

const COMPACTED_TOOL_RESULT_NOTICE: &str = "[Previous tool_result compacted]";
const CONTINUATION_PREFIX: &str = "[Context compacted]";

#[derive(Debug, Clone)]
pub struct AutoCompactEvent {
    pub removed_messages: usize,
    pub estimated_tokens_before: usize,
    pub transcript_path: Option<PathBuf>,
}

pub fn apply_micro_compact(messages: &mut [Value], cfg: &ContextCompactConfig) {
    if !cfg.enabled {
        return;
    }

    let mut call_id_to_tool_name: BTreeMap<String, String> = BTreeMap::new();
    for message in messages.iter() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in tool_calls {
            let Some(call_id) = call.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(name) = call
                .get("function")
                .and_then(Value::as_object)
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            call_id_to_tool_name.insert(call_id.to_string(), name.to_string());
        }
    }

    let mut compactable_tool_indices: Vec<usize> = Vec::new();
    for (idx, message) in messages.iter().enumerate() {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(content) = message.get("content").and_then(Value::as_str) else {
            continue;
        };
        if content == COMPACTED_TOOL_RESULT_NOTICE {
            continue;
        }
        if content.chars().count() < cfg.micro_min_tool_result_chars {
            continue;
        }
        compactable_tool_indices.push(idx);
    }

    if compactable_tool_indices.len() <= cfg.micro_keep_recent_tool_results {
        return;
    }

    let to_compact = compactable_tool_indices
        .len()
        .saturating_sub(cfg.micro_keep_recent_tool_results);
    for idx in compactable_tool_indices.into_iter().take(to_compact) {
        let notice = tool_notice_for_message(&messages[idx], &call_id_to_tool_name);
        messages[idx]["content"] = Value::String(notice);
    }
}

pub fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(estimate_message_tokens)
        .sum::<usize>()
}

pub fn auto_compact_if_needed(
    messages: &mut Vec<Value>,
    cfg: &ContextCompactConfig,
) -> Result<Option<AutoCompactEvent>> {
    if !cfg.enabled {
        return Ok(None);
    }

    if cfg.auto_token_threshold == 0 {
        return Ok(None);
    }

    let estimated_tokens_before = estimate_messages_tokens(messages);
    if estimated_tokens_before < cfg.auto_token_threshold {
        return Ok(None);
    }

    if messages.len() <= cfg.auto_preserve_recent_messages {
        return Ok(None);
    }

    let transcript_path = match backup_transcript(messages, &cfg.transcript_dir) {
        Ok(path) => Some(path),
        Err(err) => {
            eprintln!("warn: failed to save compact transcript: {err}");
            None
        }
    };

    let keep_from = messages
        .len()
        .saturating_sub(cfg.auto_preserve_recent_messages);
    let removed = messages[..keep_from].to_vec();
    let summary = build_compact_summary(&removed);

    let mut next_messages: Vec<Value> = Vec::with_capacity(cfg.auto_preserve_recent_messages + 1);
    next_messages.push(json!({
        "role": "system",
        "content": format!("{CONTINUATION_PREFIX}\n\n{summary}\n\nContinue from this summary and the recent messages."),
    }));
    next_messages.extend_from_slice(&messages[keep_from..]);

    let removed_messages = keep_from;
    *messages = next_messages;

    Ok(Some(AutoCompactEvent {
        removed_messages,
        estimated_tokens_before,
        transcript_path,
    }))
}

fn tool_notice_for_message(message: &Value, call_id_to_tool_name: &BTreeMap<String, String>) -> String {
    let tool_name = message
        .get("tool_call_id")
        .and_then(Value::as_str)
        .and_then(|id| call_id_to_tool_name.get(id))
        .cloned();

    match tool_name {
        Some(name) => format!("[Previous tool_result compacted: used {name}]"),
        None => COMPACTED_TOOL_RESULT_NOTICE.to_string(),
    }
}

fn backup_transcript(messages: &[Value], dir: &str) -> Result<PathBuf> {
    let dir_path = Path::new(dir);
    create_dir_all(dir_path)
        .with_context(|| format!("failed to create transcript dir: {}", dir_path.display()))?;

    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock error")?
        .as_millis();

    let path = dir_path.join(format!("transcript_{ts_ms}.jsonl"));
    let mut file = File::create(&path)
        .with_context(|| format!("failed to create transcript file: {}", path.display()))?;

    for message in messages {
        let line = serde_json::to_string(message).context("failed to encode transcript line")?;
        writeln!(file, "{line}").context("failed to write transcript line")?;
    }

    Ok(path)
}

fn build_compact_summary(removed: &[Value]) -> String {
    let mut role_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut tool_names: BTreeSet<String> = BTreeSet::new();
    let mut user_requests: Vec<String> = Vec::new();
    let mut timeline: Vec<String> = Vec::new();

    for message in removed {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        *role_counts.entry(role.clone()).or_insert(0) += 1;

        if role == "assistant" {
            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    if let Some(name) = call
                        .get("function")
                        .and_then(Value::as_object)
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                    {
                        tool_names.insert(name.to_string());
                    }
                }
            }
        }

        if role == "user" {
            if let Some(content) = message.get("content").and_then(Value::as_str) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    user_requests.push(truncate_line(trimmed, 180));
                }
            }
        }

        let item = summarize_message_for_timeline(message);
        if !item.is_empty() {
            timeline.push(item);
        }
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Compacted {} earlier messages.", removed.len()));

    if !role_counts.is_empty() {
        let mut parts = Vec::new();
        for (role, count) in role_counts {
            parts.push(format!("{role}={count}"));
        }
        lines.push(format!("Role counts: {}", parts.join(", ")));
    }

    if !tool_names.is_empty() {
        let names = tool_names.into_iter().collect::<Vec<_>>().join(", ");
        lines.push(format!("Tools used: {names}"));
    }

    if !user_requests.is_empty() {
        lines.push("Recent user requests: ".to_string());
        for req in user_requests.iter().rev().take(3).rev() {
            lines.push(format!("- {req}"));
        }
    }

    if !timeline.is_empty() {
        lines.push("Timeline highlights: ".to_string());
        for item in timeline.into_iter().rev().take(8).rev() {
            lines.push(format!("- {item}"));
        }
    }

    lines.join("\n")
}

fn summarize_message_for_timeline(message: &Value) -> String {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    match role {
        "assistant" => {
            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                if !tool_calls.is_empty() {
                    let mut names = Vec::new();
                    for call in tool_calls {
                        if let Some(name) = call
                            .get("function")
                            .and_then(Value::as_object)
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                        {
                            names.push(name.to_string());
                        }
                    }
                    if !names.is_empty() {
                        return format!("assistant called tools: {}", names.join(", "));
                    }
                }
            }

            message
                .get("content")
                .and_then(Value::as_str)
                .map(|s| format!("assistant: {}", truncate_line(s.trim(), 160)))
                .unwrap_or_default()
        }
        "tool" => {
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("tool result: {}", truncate_line(content.trim(), 160))
        }
        "user" => message
            .get("content")
            .and_then(Value::as_str)
            .map(|s| format!("user: {}", truncate_line(s.trim(), 160)))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn truncate_line(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out = String::new();
    for ch in input.chars().take(max_chars) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn estimate_message_tokens(message: &Value) -> usize {
    let mut chars = 0usize;
    if let Some(role) = message.get("role").and_then(Value::as_str) {
        chars += role.chars().count();
    }

    if let Some(content) = message.get("content") {
        chars += estimate_value_chars(content);
    }

    if let Some(tool_calls) = message.get("tool_calls") {
        chars += estimate_value_chars(tool_calls);
    }

    (chars / 4).saturating_add(1)
}

fn estimate_value_chars(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 4,
        Value::Number(n) => n.to_string().chars().count(),
        Value::String(s) => s.chars().count(),
        Value::Array(arr) => arr.iter().map(estimate_value_chars).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| k.chars().count() + estimate_value_chars(v))
            .sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micro_compacts_old_tool_results() {
        let mut messages = vec![
            json!({
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "function": { "name": "grep_search", "arguments": "{}" }
                    },
                    {
                        "id": "call_2",
                        "function": { "name": "glob_search", "arguments": "{}" }
                    }
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "x".repeat(200)}),
            json!({"role": "tool", "tool_call_id": "call_2", "content": "y".repeat(200)}),
        ];

        let cfg = ContextCompactConfig {
            enabled: true,
            micro_keep_recent_tool_results: 1,
            micro_min_tool_result_chars: 100,
            auto_token_threshold: 50_000,
            auto_preserve_recent_messages: 4,
            transcript_dir: ".transcripts".to_string(),
        };

        apply_micro_compact(&mut messages, &cfg);

        assert_eq!(
            messages[1].get("content").and_then(Value::as_str),
            Some("[Previous tool_result compacted: used grep_search]")
        );
        assert_eq!(
            messages[2]
                .get("content")
                .and_then(Value::as_str)
                .map(|s| s.chars().count()),
            Some(200)
        );
    }

    #[test]
    fn auto_compacts_when_threshold_crossed() {
        let mut messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "u".repeat(800)}),
            json!({"role": "assistant", "content": "a".repeat(800)}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "t".repeat(800)}),
            json!({"role": "user", "content": "keep me"}),
        ];

        let cfg = ContextCompactConfig {
            enabled: true,
            micro_keep_recent_tool_results: 3,
            micro_min_tool_result_chars: 100,
            auto_token_threshold: 100,
            auto_preserve_recent_messages: 2,
            transcript_dir: ".transcripts_test".to_string(),
        };

        let event = auto_compact_if_needed(&mut messages, &cfg)
            .expect("auto compact should not fail")
            .expect("auto compact should trigger");

        assert!(event.removed_messages >= 1);
        assert_eq!(messages.first().and_then(|m| m.get("role")).and_then(Value::as_str), Some("system"));
        assert!(
            messages
                .first()
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains(CONTINUATION_PREFIX)
        );
    }
}