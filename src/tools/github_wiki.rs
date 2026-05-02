use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
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
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

enum PublishSource<'a> {
    File(&'a str),
    Content(&'a str),
}

#[derive(Debug)]
pub struct GithubWikiClient {
    http: Client,
    cfg: GithubConfig,
}

pub fn github_wiki_publish_handler() -> ToolHandler {
    let definition = ToolDefinition {
        name: "github_wiki_publish".to_string(),
        description: "Publish or update a markdown page in the repository wiki from either a local file or direct markdown content".to_string(),
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
                    "description": "Local markdown file path to upload. Optional when content is provided."
                },
                "content": {
                    "type": "string",
                    "description": "Markdown content to write directly into the wiki page. Use this when no local file is needed."
                },
                "message": {
                    "type": "string",
                    "description": "Optional commit message"
                }
            },
            "required": ["action", "path"],
            "anyOf": [
                {"required": ["file"]},
                {"required": ["content"]}
            ],
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

        let source = resolve_source(input.file.as_deref(), input.content.as_deref())?;

        match input.action {
            PublishAction::Publish => {
                match source {
                    PublishSource::File(file) => github.publish_page(&input.path, file, &message)?,
                    PublishSource::Content(content) => {
                        github.publish_page_content(&input.path, content, &message)?
                    }
                }
                Ok(format!("published {}", input.path))
            }
            PublishAction::Update => {
                match source {
                    PublishSource::File(file) => github.update_page(&input.path, file, &message)?,
                    PublishSource::Content(content) => {
                        github.update_page_content(&input.path, content, &message)?
                    }
                }
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
        ensure_local_file(local_file)?;

        let workspace = self.clone_wiki_repo()?;
        let target = workspace.join(wiki_path);
        if target.exists() {
            bail!(
                "publish only creates new pages: {} already exists. Use update instead.",
                wiki_path
            );
        }

        self.write_and_push(&workspace, wiki_path, local_file, message)
    }

    pub fn publish_page_content(&self, wiki_path: &str, content: &str, message: &str) -> Result<()> {
        validate_wiki_path(wiki_path)?;

        let workspace = self.clone_wiki_repo()?;
        let target = workspace.join(wiki_path);
        if target.exists() {
            bail!(
                "publish only creates new pages: {} already exists. Use update instead.",
                wiki_path
            );
        }

        self.write_content_and_push(&workspace, wiki_path, content, message)
    }

    pub fn update_page(&self, wiki_path: &str, local_file: &str, message: &str) -> Result<()> {
        validate_wiki_path(wiki_path)?;
        ensure_local_file(local_file)?;

        let workspace = self.clone_wiki_repo()?;
        let target = workspace.join(wiki_path);
        if !target.exists() {
            bail!(
                "update only modifies existing pages: {} does not exist. Use publish first.",
                wiki_path
            );
        }

        self.write_and_push(&workspace, wiki_path, local_file, message)
    }

    pub fn update_page_content(&self, wiki_path: &str, content: &str, message: &str) -> Result<()> {
        validate_wiki_path(wiki_path)?;

        let workspace = self.clone_wiki_repo()?;
        let target = workspace.join(wiki_path);
        if !target.exists() {
            bail!(
                "update only modifies existing pages: {} does not exist. Use publish first.",
                wiki_path
            );
        }

        self.write_content_and_push(&workspace, wiki_path, content, message)
    }

    fn write_and_push(
        &self,
        workspace: &Path,
        wiki_path: &str,
        local_file: &str,
        message: &str,
    ) -> Result<()> {
        let target = workspace.join(wiki_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create wiki dir: {}", parent.display()))?;
        }
        fs::copy(local_file, &target).with_context(|| {
            format!(
                "failed to copy local file {} to wiki path {}",
                local_file,
                target.display()
            )
        })?;

        self.run_git(
            workspace,
            &["add", wiki_path],
            "failed to stage wiki file",
        )?;

        let commit = Command::new("git")
            .current_dir(workspace)
            .arg("-c")
            .arg(format!("user.name={}", self.cfg.username))
            .arg("-c")
            .arg(format!("user.email={}@users.noreply.github.com", self.cfg.username))
            .arg("commit")
            .arg("-m")
            .arg(message)
            .output()
            .context("failed to run git commit")?;

        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            let stdout = String::from_utf8_lossy(&commit.stdout);
            let combined = format!("{}{}", stdout, stderr);
            if combined.contains("nothing to commit") {
                return Ok(());
            }
            bail!("failed to commit wiki change: {}", combined.trim());
        }

        self.run_git(workspace, &["push", "origin", "master"], "failed to push wiki changes")?;

        Ok(())
    }

    fn write_content_and_push(
        &self,
        workspace: &Path,
        wiki_path: &str,
        content: &str,
        message: &str,
    ) -> Result<()> {
        let target = workspace.join(wiki_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create wiki dir: {}", parent.display()))?;
        }
        fs::write(&target, content)
            .with_context(|| format!("failed to write wiki content to {}", target.display()))?;

        self.run_git(
            workspace,
            &["add", wiki_path],
            "failed to stage wiki file",
        )?;

        let commit = Command::new("git")
            .current_dir(workspace)
            .arg("-c")
            .arg(format!("user.name={}", self.cfg.username))
            .arg("-c")
            .arg(format!("user.email={}@users.noreply.github.com", self.cfg.username))
            .arg("commit")
            .arg("-m")
            .arg(message)
            .output()
            .context("failed to run git commit")?;

        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            let stdout = String::from_utf8_lossy(&commit.stdout);
            let combined = format!("{}{}", stdout, stderr);
            if combined.contains("nothing to commit") {
                return Ok(());
            }
            bail!("failed to commit wiki change: {}", combined.trim());
        }

        self.run_git(workspace, &["push", "origin", "master"], "failed to push wiki changes")?;

        Ok(())
    }

    fn clone_wiki_repo(&self) -> Result<PathBuf> {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let pid = std::process::id();
        let workspace = std::env::temp_dir().join(format!(
            "scribe-wiki-{}-{}",
            ts_ms, pid
        ));

        let remote = self.wiki_remote_url();
        self.run_git(
            Path::new("."),
            &["clone", &remote, workspace.to_string_lossy().as_ref()],
            "failed to clone wiki repository",
        )?;

        Ok(workspace)
    }

    fn wiki_remote_url(&self) -> String {
        format!(
            "https://{}:{}@github.com/{}/{}.wiki.git",
            self.cfg.username, self.cfg.password, self.cfg.owner, self.cfg.repo
        )
    }

    fn run_git(&self, cwd: &Path, args: &[&str], context_msg: &str) -> Result<()> {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .with_context(|| format!("{}: git {}", context_msg, args.join(" ")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            bail!(
                "{}: {}{}",
                context_msg,
                stdout.trim(),
                if stderr.trim().is_empty() {
                    "".to_string()
                } else {
                    format!(" {}", stderr.trim())
                }
            );
        }

        Ok(())
    }
}

fn ensure_local_file(path: &str) -> Result<()> {
    let metadata = fs::metadata(path).with_context(|| format!("failed to stat local file: {}", path))?;
    if !metadata.is_file() {
        bail!("local path must point to a file: {}", path);
    }
    Ok(())
}

fn resolve_source<'a>(file: Option<&'a str>, content: Option<&'a str>) -> Result<PublishSource<'a>> {
    match (file, content) {
        (Some(file), _) => {
            ensure_local_file(file)?;
            Ok(PublishSource::File(file))
        }
        (None, Some(content)) => Ok(PublishSource::Content(content)),
        (None, None) => bail!("github_wiki_publish requires either file or content"),
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
