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
struct GithubPagesPublishInput {
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
    branch: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
}

#[derive(Debug)]
pub struct GithubPagesClient {
    http: Client,
    cfg: GithubConfig,
}

pub fn github_pages_publish_handler() -> ToolHandler {
    let definition = ToolDefinition {
        name: "github_pages_publish".to_string(),
        description: "Publish or update a markdown blog post in GitHub Pages under posts/*.md only"
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["publish", "update"],
                    "description": "publish creates a new post; update modifies an existing post"
                },
                "path": {
                    "type": "string",
                    "description": "Path in Pages repo, must match posts/*.md"
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
            .context("github_pages_publish requires GITHUB_USERNAME/GITHUB_PASSWORD")?;
        let github = GithubPagesClient::new(github_cfg)?;
        github.auth_check()?;

        let input: GithubPagesPublishInput = serde_json::from_str(input_json)
            .context("invalid input JSON for github_pages_publish")?;

        let message = input.message.unwrap_or_else(|| match input.action {
            PublishAction::Publish => "publish blog post".to_string(),
            PublishAction::Update => "update blog post".to_string(),
        });

        match input.action {
            PublishAction::Publish => {
                github.publish_post(&input.path, &input.file, &message)?;
                Ok(format!("published {}", input.path))
            }
            PublishAction::Update => {
                github.update_post(&input.path, &input.file, &message)?;
                Ok(format!("updated {}", input.path))
            }
        }
    });

    ToolHandler::new(definition, execute)
}

impl GithubPagesClient {
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

    pub fn publish_post(&self, repo_path: &str, local_file: &str, message: &str) -> Result<()> {
        validate_blog_path(repo_path)?;

        if self.fetch_existing_sha(repo_path)?.is_some() {
            bail!(
                "publish only creates new posts: {} already exists. Use update instead.",
                repo_path
            );
        }

        self.put_file(repo_path, local_file, message, None)
    }

    pub fn update_post(&self, repo_path: &str, local_file: &str, message: &str) -> Result<()> {
        validate_blog_path(repo_path)?;

        let sha = self.fetch_existing_sha(repo_path)?.with_context(|| {
            format!(
                "update only modifies existing posts: {} does not exist. Use publish first.",
                repo_path
            )
        })?;

        self.put_file(repo_path, local_file, message, Some(sha))
    }

    fn put_file(
        &self,
        repo_path: &str,
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
            branch: &self.cfg.branch,
            sha,
        };

        let url = self.contents_url(repo_path);
        let response = self
            .http
            .put(url)
            .basic_auth(&self.cfg.username, Some(&self.cfg.password))
            .json(&req)
            .send()
            .context("failed to upload file to GitHub Pages")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_else(|_| "<no body>".to_string());
            bail!("GitHub Pages publish failed ({status}): {body}");
        }

        Ok(())
    }

    fn fetch_existing_sha(&self, repo_path: &str) -> Result<Option<String>> {
        let url = self.contents_url(repo_path);
        let response = self
            .http
            .get(url)
            .query(&[("ref", self.cfg.branch.as_str())])
            .basic_auth(&self.cfg.username, Some(&self.cfg.password))
            .send()
            .context("failed to query GitHub Pages file")?;

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

    fn contents_url(&self, repo_path: &str) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/contents/{}",
            self.cfg.owner, self.cfg.repo, repo_path
        )
    }
}

fn validate_blog_path(path: &str) -> Result<()> {
    if path.starts_with('/') || path.contains("..") {
        bail!("invalid blog path: absolute paths and '..' are not allowed");
    }

    if !path.starts_with("posts/") || !path.ends_with(".md") {
        bail!("path must match posts/*.md for strict permission control");
    }

    Ok(())
}
