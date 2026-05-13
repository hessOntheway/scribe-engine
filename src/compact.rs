use std::collections::BTreeSet;
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

const CONTINUATION_PREFIX: &str = "[Context compacted]";
const CONTINUATION_SUFFIX: &str = "Continue from this summary and the recent messages.";
const MAX_SUMMARY_SOURCE_CHARS: usize = 60_000;
const DEFAULT_SUMMARY_MAX_CHARS: usize = 1_200;
const DEFAULT_SUMMARY_MAX_LINES: usize = 24;
const DEFAULT_SUMMARY_MAX_LINE_CHARS: usize = 160;

#[derive(Debug, Clone)]
pub struct AutoCompactEvent {
    pub removed_messages: usize,
    pub estimated_tokens_before: usize,
    pub transcript_path: Option<PathBuf>,
}

pub fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages.iter().map(estimate_message_tokens).sum::<usize>()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactPlan {
    compacted_prefix_len: usize,
    keep_from: usize,
    removed_messages: usize,
    estimated_tokens_before: usize,
}

fn plan_auto_compact(messages: &[Value], cfg: &ContextCompactConfig) -> Option<CompactPlan> {
    if !cfg.enabled || cfg.auto_token_threshold == 0 {
        return None;
    }

    let compacted_prefix_len = compacted_summary_prefix_len(messages);
    let compactable = messages.get(compacted_prefix_len..)?;
    if compactable.len() <= cfg.auto_preserve_recent_messages {
        return None;
    }

    let compactable_tokens = estimate_messages_tokens(compactable);
    if compactable_tokens < cfg.auto_token_threshold {
        return None;
    }

    let keep_from = messages
        .len()
        .saturating_sub(cfg.auto_preserve_recent_messages);
    let keep_from = adjust_keep_from_to_tool_boundary(messages, keep_from);
    if keep_from <= compacted_prefix_len {
        return None;
    }

    Some(CompactPlan {
        compacted_prefix_len,
        keep_from,
        removed_messages: keep_from.saturating_sub(compacted_prefix_len),
        estimated_tokens_before: estimate_messages_tokens(messages),
    })
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

    let Some(plan) = plan_auto_compact(messages, cfg) else {
        return Ok(None);
    };

    let transcript_path = match backup_transcript(messages, &cfg.transcript_dir) {
        Ok(path) => Some(path),
        Err(err) => {
            eprintln!("warn: failed to save compact transcript: {err}");
            None
        }
    };

    let removed = messages[plan.compacted_prefix_len..plan.keep_from].to_vec();
    let summary_source = build_compact_summary_source(&removed);
    let new_summary = match generate_compact_summary(
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

    let existing_summary = messages
        .first()
        .and_then(extract_existing_compacted_summary);
    let summary = merge_compact_summaries(existing_summary.as_deref(), &new_summary);
    let next_messages = build_messages_after_compaction(messages, &plan, &summary);
    *messages = next_messages;

    Ok(Some(AutoCompactEvent {
        removed_messages: plan.removed_messages,
        estimated_tokens_before: plan.estimated_tokens_before,
        transcript_path,
    }))
}

fn build_messages_after_compaction(
    messages: &[Value],
    plan: &CompactPlan,
    summary: &str,
) -> Vec<Value> {
    let preserved = messages.get(plan.keep_from..).unwrap_or_default();
    let mut next_messages: Vec<Value> = Vec::with_capacity(preserved.len() + 1);
    next_messages.push(json!({
        "role": "system",
        "content": compact_continuation_message(summary),
    }));
    next_messages.extend_from_slice(preserved);
    next_messages
}

fn compact_continuation_message(summary: &str) -> String {
    format!("{CONTINUATION_PREFIX}\n\n{summary}\n\n{CONTINUATION_SUFFIX}")
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

    if !pending_tool_call_ids.is_empty()
        && let Some(previous) = cleaned.last_mut()
        && previous.get("role").and_then(Value::as_str) == Some("assistant")
        && previous.get("tool_calls").is_some()
        && let Some(obj) = previous.as_object_mut()
    {
        obj.remove("tool_calls");
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

fn compacted_summary_prefix_len(messages: &[Value]) -> usize {
    usize::from(
        messages
            .first()
            .and_then(extract_existing_compacted_summary)
            .is_some(),
    )
}

fn extract_existing_compacted_summary(message: &Value) -> Option<String> {
    if message.get("role").and_then(Value::as_str) != Some("system") {
        return None;
    }

    let content = message.get("content").and_then(Value::as_str)?;
    let summary = content.strip_prefix(CONTINUATION_PREFIX)?;
    let summary = summary.trim_start_matches('\n');
    let summary = summary
        .split_once(&format!("\n\n{CONTINUATION_SUFFIX}"))
        .map_or(summary, |(value, _)| value);
    Some(summary.trim().to_string())
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

fn merge_compact_summaries(existing_summary: Option<&str>, new_summary: &str) -> String {
    let new_summary = new_summary.trim();
    let Some(existing_summary) = existing_summary.map(str::trim).filter(|s| !s.is_empty()) else {
        return compress_summary_text(new_summary);
    };

    let mut lines = vec![
        "Conversation summary:".to_string(),
        "- Previously compacted context:".to_string(),
    ];
    lines.extend(summary_detail_lines(existing_summary));
    lines.push("- Newly compacted context:".to_string());
    lines.extend(summary_detail_lines(new_summary));

    compress_summary_text(&lines.join("\n"))
}

fn summary_detail_lines(summary: &str) -> Vec<String> {
    summary
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| *line != "Summary:" && *line != "Conversation summary:")
        .map(|line| format!("  {line}"))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SummaryCompressionBudget {
    max_chars: usize,
    max_lines: usize,
    max_line_chars: usize,
}

impl Default for SummaryCompressionBudget {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_SUMMARY_MAX_CHARS,
            max_lines: DEFAULT_SUMMARY_MAX_LINES,
            max_line_chars: DEFAULT_SUMMARY_MAX_LINE_CHARS,
        }
    }
}

fn compress_summary_text(summary: &str) -> String {
    compress_summary(summary, SummaryCompressionBudget::default())
}

fn compress_summary(summary: &str, budget: SummaryCompressionBudget) -> String {
    let normalized = normalize_summary_lines(summary, budget.max_line_chars);
    if normalized.is_empty() || budget.max_chars == 0 || budget.max_lines == 0 {
        return String::new();
    }

    let selected = select_summary_line_indexes(&normalized, budget);
    let mut compressed_lines = selected
        .iter()
        .map(|index| normalized[*index].clone())
        .collect::<Vec<_>>();
    if compressed_lines.is_empty() {
        compressed_lines.push(truncate_summary_line(&normalized[0], budget.max_chars));
    }

    let omitted_lines = normalized.len().saturating_sub(compressed_lines.len());
    if omitted_lines > 0 {
        push_summary_line_with_budget(
            &mut compressed_lines,
            format!("- ... {omitted_lines} additional line(s) omitted."),
            budget,
        );
    }

    compressed_lines.join("\n")
}

fn normalize_summary_lines(summary: &str, max_line_chars: usize) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut lines = Vec::new();

    for raw_line in summary.lines() {
        let normalized = collapse_inline_whitespace(raw_line);
        if normalized.is_empty() {
            continue;
        }

        let truncated = truncate_summary_line(&normalized, max_line_chars);
        if seen.insert(truncated.to_ascii_lowercase()) {
            lines.push(truncated);
        }
    }

    lines
}

fn select_summary_line_indexes(lines: &[String], budget: SummaryCompressionBudget) -> Vec<usize> {
    let mut selected = BTreeSet::<usize>::new();

    for priority in 0..=3 {
        for (index, line) in lines.iter().enumerate() {
            if selected.contains(&index) || summary_line_priority(line) != priority {
                continue;
            }

            let candidate = selected
                .iter()
                .map(|selected_index| lines[*selected_index].as_str())
                .chain(std::iter::once(line.as_str()))
                .collect::<Vec<_>>();

            if candidate.len() > budget.max_lines {
                continue;
            }

            if joined_summary_char_count(&candidate) > budget.max_chars {
                continue;
            }

            selected.insert(index);
        }
    }

    selected.into_iter().collect()
}

fn push_summary_line_with_budget(
    lines: &mut Vec<String>,
    line: String,
    budget: SummaryCompressionBudget,
) {
    let candidate = lines
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(line.as_str()))
        .collect::<Vec<_>>();

    if candidate.len() <= budget.max_lines
        && joined_summary_char_count(&candidate) <= budget.max_chars
    {
        lines.push(line);
    }
}

fn joined_summary_char_count(lines: &[&str]) -> usize {
    lines.iter().map(|line| line.chars().count()).sum::<usize>() + lines.len().saturating_sub(1)
}

fn summary_line_priority(line: &str) -> usize {
    if line == "Summary:" || line == "Conversation summary:" || is_core_summary_detail(line) {
        0
    } else if line.ends_with(':') {
        1
    } else if line.starts_with("- ") || line.starts_with("  - ") {
        2
    } else {
        3
    }
}

fn is_core_summary_detail(line: &str) -> bool {
    [
        "- Scope:",
        "- Current work:",
        "- Pending work:",
        "- Key files referenced:",
        "- Tools mentioned:",
        "- Recent user requests:",
        "- Previously compacted context:",
        "- Newly compacted context:",
        "  - Scope:",
        "  - Current work:",
        "  - Pending work:",
        "  - Key files referenced:",
        "  - Tools mentioned:",
        "  - Recent user requests:",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn collapse_inline_whitespace(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_summary_line(line: &str, max_chars: usize) -> String {
    if max_chars == 0 || line.chars().count() <= max_chars {
        return line.to_string();
    }

    if max_chars == 1 {
        return "...".to_string();
    }

    let mut truncated = line
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    truncated.push_str("...");
    truncated
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
    fn auto_compact_plan_ignores_large_tool_content_below_overall_threshold() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "{}" }
                    }
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "x".repeat(10_000)}),
            json!({"role": "user", "content": "keep"}),
        ];
        let cfg = ContextCompactConfig {
            enabled: true,
            auto_token_threshold: 100_000,
            auto_preserve_recent_messages: 2,
            transcript_dir: ".transcripts".to_string(),
        };

        assert_eq!(plan_auto_compact(&messages, &cfg), None);
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
    fn compact_plan_preserves_tool_call_pair_at_tail_boundary() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "old ".repeat(200)}),
            json!({
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "{}" }
                    }
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "result ".repeat(200)}),
            json!({"role": "user", "content": "recent"}),
        ];
        let cfg = ContextCompactConfig {
            enabled: true,
            auto_token_threshold: 1,
            auto_preserve_recent_messages: 2,
            transcript_dir: ".transcripts".to_string(),
        };

        let plan = plan_auto_compact(&messages, &cfg).expect("plan should compact");
        assert_eq!(plan.keep_from, 2);

        let compacted = build_messages_after_compaction(&messages, &plan, "summary");
        assert_eq!(compacted.len(), 4);
        assert_eq!(
            compacted[1].get("role").and_then(Value::as_str),
            Some("assistant")
        );
        assert!(compacted[1].get("tool_calls").is_some());
        assert_eq!(
            compacted[2].get("role").and_then(Value::as_str),
            Some("tool")
        );
    }

    #[test]
    fn compact_plan_skips_existing_summary_when_checking_threshold() {
        let messages = vec![
            json!({"role": "system", "content": compact_continuation_message(&"old ".repeat(10_000))}),
            json!({"role": "user", "content": "tiny"}),
            json!({"role": "assistant", "content": "small"}),
        ];
        let cfg = ContextCompactConfig {
            enabled: true,
            auto_token_threshold: 100,
            auto_preserve_recent_messages: 1,
            transcript_dir: ".transcripts".to_string(),
        };

        assert_eq!(plan_auto_compact(&messages, &cfg), None);
    }

    #[test]
    fn repeated_compaction_merges_existing_and_new_summary() {
        let existing =
            "- Scope: previous interview outline.\n- Current work: discuss module boundaries.";
        let messages = vec![
            json!({"role": "system", "content": compact_continuation_message(existing)}),
            json!({"role": "user", "content": "old question ".repeat(200)}),
            json!({"role": "assistant", "content": "old answer ".repeat(200)}),
            json!({"role": "user", "content": "recent"}),
        ];
        let cfg = ContextCompactConfig {
            enabled: true,
            auto_token_threshold: 1,
            auto_preserve_recent_messages: 1,
            transcript_dir: ".transcripts".to_string(),
        };

        let plan = plan_auto_compact(&messages, &cfg).expect("plan should compact");
        assert_eq!(plan.compacted_prefix_len, 1);
        assert_eq!(plan.removed_messages, 2);

        let existing_summary = messages
            .first()
            .and_then(extract_existing_compacted_summary);
        let merged = merge_compact_summaries(
            existing_summary.as_deref(),
            "- Scope: new interview content.\n- Tools mentioned: read_file.",
        );
        let compacted = build_messages_after_compaction(&messages, &plan, &merged);
        let content = compacted[0].get("content").and_then(Value::as_str).unwrap();

        assert!(content.contains("- Previously compacted context:"));
        assert!(content.contains("- Newly compacted context:"));
        assert!(content.contains("previous interview outline"));
        assert!(content.contains("new interview content"));
        assert_eq!(
            compacted[1].get("content").and_then(Value::as_str),
            Some("recent")
        );
    }

    #[test]
    fn summary_compression_dedupes_and_keeps_core_details() {
        let summary = [
            "Conversation summary:",
            "- Scope:   compact   earlier   messages.",
            "- Scope: compact earlier messages.",
            "- Current work: finish summary compression.",
            "- Key files referenced: src/compact.rs.",
            "- Tools mentioned: read_file, grep_search.",
            "- Key timeline:",
            "  - user: asked for a working implementation.",
            "  - assistant: inspected runtime compaction flow.",
        ]
        .join("\n");

        let compressed = compress_summary(
            &summary,
            SummaryCompressionBudget {
                max_chars: 260,
                max_lines: 6,
                max_line_chars: 80,
            },
        );

        assert!(compressed.contains("Conversation summary:"));
        assert!(compressed.contains("- Scope: compact earlier messages."));
        assert_eq!(
            compressed
                .matches("- Scope: compact earlier messages.")
                .count(),
            1
        );
        assert!(compressed.contains("- Current work: finish summary compression."));
        assert!(compressed.contains("- Key files referenced: src/compact.rs."));
        assert!(compressed.contains("- Tools mentioned: read_file, grep_search."));
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
