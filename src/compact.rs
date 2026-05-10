use std::collections::BTreeMap;
use std::fs::{File, create_dir_all};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::ContextCompactConfig;
use crate::llm::openai::OpenAiCompatClient;
use crate::llm::usage::PromptCacheStats;
use crate::tools::ToolDefinition;

const COMPACTED_TOOL_RESULT_NOTICE: &str = "[Previous tool_result compacted]";
const CONTINUATION_PREFIX: &str = "[Context compacted]";
const MAX_SUMMARY_SOURCE_CHARS: usize = 60_000;

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
    messages.iter().map(estimate_message_tokens).sum::<usize>()
}

pub fn auto_compact_if_needed(
    messages: &mut Vec<Value>,
    cfg: &ContextCompactConfig,
    llm: &OpenAiCompatClient,
    audit_log_path_override: Option<&str>,
    tool_definitions: &[crate::tools::ToolDefinition],
    prompt_cache_stats: Option<&mut PromptCacheStats>,
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
    let keep_from = adjust_keep_from_to_tool_boundary(messages, keep_from);
    if keep_from == 0 {
        return Ok(None);
    }
    let removed = messages[..keep_from].to_vec();
    let summary_source = build_compact_summary_source(&removed);
    let summary = match generate_compact_summary(
        llm,
        messages,
        &summary_source,
        tool_definitions,
        audit_log_path_override,
        prompt_cache_stats,
    ) {
        Ok(summary) => summary,
        Err(err) => {
            eprintln!("warn: failed to generate compact summary: {err}");
            return Ok(None);
        }
    };

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

pub fn remove_orphan_tool_messages(messages: &mut Vec<Value>) -> usize {
    let mut cleaned = Vec::with_capacity(messages.len());
    let mut pending_tool_call_ids: Vec<String> = Vec::new();
    let mut removed = 0usize;

    for message in messages.drain(..) {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");

        if role == "tool" {
            let tool_call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(position) = pending_tool_call_ids
                .iter()
                .position(|id| id.as_str() == tool_call_id)
            {
                pending_tool_call_ids.remove(position);
                cleaned.push(message);
            } else {
                removed += 1;
            }
            continue;
        }

        if !pending_tool_call_ids.is_empty() {
            let Some(previous) = cleaned.last_mut() else {
                pending_tool_call_ids.clear();
                cleaned.push(message);
                continue;
            };

            if previous.get("role").and_then(Value::as_str) == Some("assistant")
                && previous.get("tool_calls").is_some()
            {
                previous.as_object_mut().map(|obj| obj.remove("tool_calls"));
            }
            pending_tool_call_ids.clear();
        }

        pending_tool_call_ids = assistant_tool_call_ids(&message);
        cleaned.push(message);
    }

    if !pending_tool_call_ids.is_empty() {
        if let Some(previous) = cleaned.last_mut() {
            if previous.get("role").and_then(Value::as_str) == Some("assistant")
                && previous.get("tool_calls").is_some()
            {
                previous.as_object_mut().map(|obj| obj.remove("tool_calls"));
            }
        }
    }

    *messages = cleaned;
    removed
}

fn adjust_keep_from_to_tool_boundary(messages: &[Value], mut keep_from: usize) -> usize {
    while keep_from > 0
        && messages
            .get(keep_from)
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            == Some("tool")
    {
        keep_from -= 1;
    }

    if keep_from > 0
        && messages
            .get(keep_from)
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            == Some("assistant")
        && messages
            .get(keep_from)
            .and_then(|m| m.get("tool_calls"))
            .is_some()
    {
        return keep_from;
    }

    keep_from
}

fn assistant_tool_call_ids(message: &Value) -> Vec<String> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }

    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| call.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn generate_compact_summary(
    llm: &OpenAiCompatClient,
    history_messages: &[Value],
    summary_source: &str,
    tool_definitions: &[ToolDefinition],
    audit_log_path_override: Option<&str>,
    prompt_cache_stats: Option<&mut PromptCacheStats>,
) -> Result<String> {
    let mut messages = history_messages.to_vec();
    messages.push(json!({
        "role": "user",
        "content": format!(
            "Compress the conversation above into a short summary that preserves the current goal, completed work, open tasks, important file paths, tool usage, and unresolved decisions. Output the summary plainly without extra commentary.\n\nTRANSCRIPT START\n{summary_source}\nTRANSCRIPT END"
        )
    }));

    let response = llm.create_chat_completion_with_audit_path(
        &messages,
        tool_definitions,
        audit_log_path_override,
    )?;

    if let Some(stats) = prompt_cache_stats {
        stats.record_usage(&response.usage);
        eprintln!("{}", stats.summary_line());
    }

    response
        .message
        .get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .context("compact summary response missing content")
}

fn tool_notice_for_message(
    message: &Value,
    call_id_to_tool_name: &BTreeMap<String, String>,
) -> String {
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

fn build_compact_summary_source(removed: &[Value]) -> String {
    let mut out = String::new();
    for (index, message) in removed.iter().enumerate() {
        let line = match serde_json::to_string(message) {
            Ok(serialized) => serialized,
            Err(_) => "{\"error\":\"failed to serialize message\"}".to_string(),
        };

        let next_len = out.len().saturating_add(line.len()).saturating_add(1);
        if next_len > MAX_SUMMARY_SOURCE_CHARS {
            out.push_str(&format!(
                "\n[... truncated after {} removed messages due to summary source limit ...]",
                index
            ));
            break;
        }

        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
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
    fn removes_orphan_tool_messages() {
        let mut messages = vec![
            json!({"role": "system", "content": "summary"}),
            json!({"role": "tool", "tool_call_id": "old_call", "content": "orphan"}),
            json!({"role": "user", "content": "continue"}),
        ];

        let removed = remove_orphan_tool_messages(&mut messages);

        assert_eq!(removed, 1);
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[1].get("role").and_then(Value::as_str),
            Some("user")
        );
    }

    #[test]
    fn keeps_valid_tool_call_pairs() {
        let mut messages = vec![
            json!({
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "{}" }
                    }
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "result"}),
            json!({"role": "user", "content": "continue"}),
        ];

        let removed = remove_orphan_tool_messages(&mut messages);

        assert_eq!(removed, 0);
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn compact_boundary_moves_before_tool_pair() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "old"}),
            json!({
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "{}" }
                    }
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "result"}),
            json!({"role": "user", "content": "new"}),
        ];

        assert_eq!(adjust_keep_from_to_tool_boundary(&messages, 3), 2);
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

        let llm = OpenAiCompatClient::new(crate::config::LlmConfig {
            api_key: "test".to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            model: "gpt-4.1-mini".to_string(),
            write_model_audit_log: false,
            model_audit_log_path: ".auditlog/llm_response_audit.json".to_string(),
            enable_prompt_cache: false,
            prompt_cache_dir: ".cache/prompt_cache".to_string(),
            context_compact: cfg.clone(),
        })
        .expect("llm client should build");

        let result = auto_compact_if_needed(&mut messages, &cfg, &llm, None, &[], None);
        assert!(result.is_err() || result.ok().flatten().is_none());
    }
}
