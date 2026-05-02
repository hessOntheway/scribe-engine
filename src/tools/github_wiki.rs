use std::fs;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::GithubConfig;

use super::{ToolDefinition, ToolExecutor, ToolHandler};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PublishAction {
    Publish,
    Update,
}

#[derive(Debug, Deserialize)]
struct GithubWikiPublishInput {
    action: PublishAction,
    path: String,
    file: String,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExistingFile {
    sha: String,
}

#[derive(Debug, Serialize)]
struct PutFileRequest<'a> {
    message: &'a str,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
}

#[derive(Debug)]
pub struct GithubWikiClient {
    http: Client,
    cfg: GithubConfig,
}

pub fn github_wiki_publish_handler() -> ToolHandler {
    let definition = ToolDefinition {
        name: "github_wiki_publish".to_string(),
        description: "Publish or update a markdown page in the repository wiki".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["publish", "update"],
                    "description": "publish creates a new page; update modifies an existing page"
                },
                "path": {
                    "type": "string",
                    "description": "Path in wiki repo, must be a relative *.md path"
                },
                "file": {
                    "type": "string",
                    "description": "Local markdown file path to upload"
                },
                "message": {
                    "type": "string",
                    "description": "Optional commit message"
                }
            },
            "required": ["action", "path", "file"],
            "additionalProperties": false
        }),
    };

    let execute: ToolExecutor = std::sync::Arc::new(move |input_json: &str| {
        let github_cfg = GithubConfig::from_env()
            .context("github_wiki_publish requires GITHUB_USERNAME/GITHUB_PASSWORD")?;
        let github = GithubWikiClient::new(github_cfg)?;
        github.auth_check()?;

        let input: GithubWikiPublishInput = serde_json::from_str(input_json)
            .context("invalid input JSON for github_wiki_publish")?;

        let message = input.message.unwrap_or_else(|| match input.action {
            PublishAction::Publish => "publish wiki page".to_string(),
            PublishAction::Update => "update wiki page".to_string(),
        });

        match input.action {
            PublishAction::Publish => {
                github.publish_page(&input.path, &input.file, &message)?;
                Ok(format!("published {}", input.path))
            }
            PublishAction::Update => {
                github.update_page(&input.path, &input.file, &message)?;
                Ok(format!("updated {}", input.path))
            }
        }
    });

    ToolHandler::new(definition, execute)
}

impl GithubWikiClient {
    pub fn new(cfg: GithubConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("my-claw-blog-agent/0.1"),
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );

        let http = Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build github http client")?;

        Ok(Self { http, cfg })
    }

    pub fn auth_check(&self) -> Result<()> {
        let response = self
            .http
            .get("https://api.github.com/user")
            .basic_auth(&self.cfg.username, Some(&self.cfg.password))
            .send()
            .context("failed to connect to GitHub")?;

        if !response.status().is_success() {
            bail!(
                "GitHub auth failed with status {}. Ensure GITHUB_USERNAME and GITHUB_PASSWORD are valid.",
                response.status()
            );
        }

        Ok(())
    }

    pub fn publish_page(&self, wiki_path: &str, local_file: &str, message: &str) -> Result<()> {
        validate_wiki_path(wiki_path)?;

        if self.fetch_existing_sha(wiki_path)?.is_some() {
            bail!(
                "publish only creates new pages: {} already exists. Use update instead.",
                wiki_path
            );
        }

        self.put_file(wiki_path, local_file, message, None)
    }

    pub fn update_page(&self, wiki_path: &str, local_file: &str, message: &str) -> Result<()> {
        validate_wiki_path(wiki_path)?;

        let sha = self.fetch_existing_sha(wiki_path)?.with_context(|| {
            format!(
                "update only modifies existing pages: {} does not exist. Use publish first.",
                wiki_path
            )
        })?;

        self.put_file(wiki_path, local_file, message, Some(sha))
    }

    fn put_file(
        &self,
        wiki_path: &str,
        local_file: &str,
        message: &str,
        sha: Option<String>,
    ) -> Result<()> {
        let content = fs::read_to_string(local_file)
            .with_context(|| format!("failed to read local file: {}", local_file))?;
        let encoded_content = STANDARD.encode(content.as_bytes());

        let req = PutFileRequest {
            message,
            content: encoded_content,
            sha,
        };

        let url = self.contents_url(wiki_path);
        let response = self
            .http
            .put(url)
            .basic_auth(&self.cfg.username, Some(&self.cfg.password))
            .json(&req)
            .send()
            .context("failed to upload file to GitHub Wiki")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_else(|_| "<no body>".to_string());
            bail!("GitHub Wiki publish failed ({status}): {body}");
        }

        Ok(())
    }

    fn fetch_existing_sha(&self, wiki_path: &str) -> Result<Option<String>> {
        let url = self.contents_url(wiki_path);
        let response = self
            .http
            .get(url)
            .basic_auth(&self.cfg.username, Some(&self.cfg.password))
            .send()
            .context("failed to query GitHub Wiki file")?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_else(|_| "<no body>".to_string());
            bail!("GitHub query failed ({status}): {body}");
        }

        let existing: ExistingFile = response
            .json()
            .context("failed to parse GitHub content response")?;
        Ok(Some(existing.sha))
    }

    fn contents_url(&self, wiki_path: &str) -> String {
        format!(
            "https://api.github.com/repos/{}/{}.wiki/contents/{}",
            self.cfg.owner, self.cfg.repo, wiki_path
        )
    }
}

fn validate_wiki_path(path: &str) -> Result<()> {
    if path.starts_with('/') || path.contains("..") {
        bail!("invalid wiki path: absolute paths and '..' are not allowed");
    }

    if !path.ends_with(".md") {
        bail!("path must be a markdown file ending with .md");
    }

    Ok(())
}
