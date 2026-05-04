use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ToolDefinition, ToolExecutor, ToolHandler};

#[derive(Debug, Deserialize)]
struct WebFetchInput {
    url: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    include_headers: bool,
}

#[derive(Debug, Serialize)]
struct WebFetchHeader {
    name: String,
    value: String,
}

#[derive(Debug, Serialize)]
struct WebFetchOutput {
    url: String,
    final_url: String,
    status_code: u16,
    status_text: Option<String>,
    content_type: Option<String>,
    title: Option<String>,
    truncated: bool,
    byte_count: usize,
    headers: Option<Vec<WebFetchHeader>>,
    text: String,
}

pub fn web_fetch_handler() -> ToolHandler {
    let definition = ToolDefinition {
        name: "web_fetch".to_string(),
        description: "Fetch a public web page over HTTP(S), extract a readable text view, and return response metadata for research tasks.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Required HTTP or HTTPS URL to fetch. Only public GET requests are supported."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Optional request timeout in seconds. Defaults to 15 seconds."
                },
                "max_bytes": {
                    "type": "integer",
                    "description": "Optional maximum response size to read in bytes. Defaults to 200000 bytes."
                },
                "include_headers": {
                    "type": "boolean",
                    "description": "Include response headers in the output when true."
                }
            },
            "required": ["url"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = std::sync::Arc::new(move |input_json: &str| {
        let input: WebFetchInput = serde_json::from_str(input_json).with_context(|| {
            "invalid input JSON for web_fetch; expected {\"url\": \"https://example.com\", \"timeout_seconds\": 15, \"max_bytes\": 200000, \"include_headers\": false}".to_string()
        })?;
        let output = run_web_fetch(&input)?;
        serde_json::to_string_pretty(&output).context("failed to serialize web_fetch output")
    });

    ToolHandler::new(definition, execute)
}

fn run_web_fetch(input: &WebFetchInput) -> Result<WebFetchOutput> {
    let parsed_url = reqwest::Url::parse(&input.url)
        .with_context(|| format!("invalid url: {}", input.url))?;

    match parsed_url.scheme() {
        "http" | "https" => {}
        other => bail!("unsupported URL scheme: {other}; only http and https are allowed"),
    }

    let timeout_seconds = input.timeout_seconds.unwrap_or(15).clamp(1, 60);
    let max_bytes = input.max_bytes.unwrap_or(200_000).clamp(1, 1_000_000);

    let client = Client::builder()
        .user_agent("my_claw-web-fetch/0.1")
        .redirect(Policy::limited(5))
        .timeout(Duration::from_secs(timeout_seconds))
        .build()
        .context("failed to build web fetch client")?;

    let response = client
        .get(parsed_url.clone())
        .send()
        .with_context(|| format!("failed to fetch url: {}", input.url))?;

    let status = response.status();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let response_headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.as_str().to_string(), value.to_str().ok()?.to_string())))
        .collect::<Vec<_>>();

    let mut body = Vec::new();
    let mut limited_reader = response.take(max_bytes as u64 + 1);
    limited_reader
        .read_to_end(&mut body)
        .context("failed to read web response body")?;

    let truncated = body.len() > max_bytes;
    if truncated {
        body.truncate(max_bytes);
    }

    let raw_text = String::from_utf8_lossy(&body).to_string();
    let text = decode_response_text(&raw_text, content_type.as_deref());
    let title = extract_title(&raw_text, content_type.as_deref());
    let headers = if input.include_headers {
        Some(
            response_headers
                .into_iter()
                .map(|(name, value)| WebFetchHeader { name, value })
                .collect(),
        )
    } else {
        None
    };

    Ok(WebFetchOutput {
        url: input.url.clone(),
        final_url,
        status_code: status.as_u16(),
        status_text: status.canonical_reason().map(|value| value.to_string()),
        content_type,
        title,
        truncated,
        byte_count: body.len(),
        headers,
        text,
    })
}

fn decode_response_text(raw: &str, content_type: Option<&str>) -> String {
    if content_type
        .map(|value| value.contains("html") || value.contains("xhtml"))
        .unwrap_or(false)
    {
        html_to_text(raw)
    } else {
        normalize_whitespace(raw)
    }
}

fn extract_title(text: &str, content_type: Option<&str>) -> Option<String> {
    if !content_type
        .map(|value| value.contains("html") || value.contains("xhtml"))
        .unwrap_or(false)
    {
        return None;
    }

    let title_re = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").ok()?;
    let title = title_re.captures(text)?.get(1)?.as_str();
    let cleaned = normalize_whitespace(&strip_tags(title));
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn html_to_text(html: &str) -> String {
    let script_re = Regex::new(r"(?is)<(script|style|noscript)[^>]*>.*?</(script|style|noscript)>").expect("valid regex");
    let tag_re = Regex::new(r"(?is)<[^>]+>").expect("valid regex");

    let without_blocks = script_re.replace_all(html, " ");
    let stripped = tag_re.replace_all(&without_blocks, " ");
    normalize_whitespace(&strip_html_entities(&stripped))
}

fn strip_tags(input: &str) -> String {
    let tag_re = Regex::new(r"(?is)<[^>]+>").expect("valid regex");
    tag_re.replace_all(input, " ").to_string()
}

fn strip_html_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn normalize_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_markup() {
        let html = "<html><head><title>Hello &amp; World</title><style>body{}</style></head><body><h1>Alpha</h1><script>bad()</script><p>Beta</p></body></html>";
        let text = decode_response_text(html, Some("text/html"));

        assert!(text.contains("Hello & World"));
        assert!(text.contains("Alpha"));
        assert!(text.contains("Beta"));
        assert!(!text.contains("bad()"));
    }

    #[test]
    fn extract_title_returns_none_for_non_html() {
        assert!(extract_title("plain text", Some("text/plain")).is_none());
    }

    #[test]
    fn rejects_unsupported_scheme() {
        let input = WebFetchInput {
            url: "ftp://example.com".to_string(),
            timeout_seconds: None,
            max_bytes: None,
            include_headers: false,
        };

        let err = run_web_fetch(&input).expect_err("ftp should be rejected");
        assert!(err.to_string().contains("unsupported URL scheme"));
    }
}
