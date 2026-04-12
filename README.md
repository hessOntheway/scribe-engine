# Scribe Engine

[English](#english) | [中文](#中文)

---

## English

Scribe Engine is a lightweight Rust agent that helps developers analyze codebases and quickly produce blog-ready documentation.

---

## What This Project Does

Scribe Engine focuses on two things:

1. Code understanding: it can search file paths and source content to collect evidence from a repository.
2. Blog publishing workflow: it can publish or update Markdown posts in a GitHub Pages repository.

In practice, this helps programmers:

- Learn open-source projects faster
- Turn code exploration into structured blog drafts
- Ship blog posts with less manual work

## Core Features

- Agent runtime with model-tool loop (`ask` command)
- OpenAI-compatible LLM client (custom base URL/model)
- Built-in repository search tools:
- `glob_search` for path/file discovery
- `grep_search` for content search
- GitHub Pages publishing tool:
- `github_pages_publish` with strict path policy (`posts/*.md` only)
- CLI-first workflow for local automation and scripting

## CLI Commands

- `tools`: print tool definitions (JSON)
- `tool-call`: manually execute a tool by name + JSON input
- `ask`: run the agent with a prompt and tool-calling loop
- `publish`: publish a new Markdown post to GitHub Pages
- `update`: update an existing Markdown post on GitHub Pages

## Environment Variables

LLM settings:

- `LLM_API_KEY` (required)
- `LLM_BASE_URL` (default: `https://api.openai.com/v1`)
- `LLM_MODEL` (default: `gpt-4.1-mini`)
- `LLM_SYSTEM_PROMPT` (optional)
- `LLM_WRITE_MODEL_AUDIT_LOG` (optional, default: `false`)
- `LLM_MODEL_AUDIT_LOG_PATH` (optional, default: `.auditlog/llm_response_audit.json`)

GitHub Pages settings (required for publish/update):

- `GITHUB_USERNAME`
- `GITHUB_PASSWORD`
- `GITHUB_PAGES_OWNER` (optional, defaults to username)
- `GITHUB_PAGES_BRANCH` (optional, default: `main`)

## Quick Start

```bash
cargo build

# Show all registered tools
cargo run -- tools

# Ask the agent to analyze code and draft blog content
cargo run -- ask --prompt "Analyze this project and write a blog outline" --max-steps 6

# Publish a markdown file to GitHub Pages repo
cargo run -- publish \
  --path posts/my-post.md \
  --file ./my-post.md \
  --message "publish blog post"
```

## Typical Use Case

1. Use `ask` to analyze a target repository.
2. Generate blog outline or draft from findings.
3. Refine markdown locally.
4. Use `publish` or `update` to sync the post to GitHub Pages.

## Notes

- This project currently ships with a focused toolset for search + publishing.
- You can extend it by registering plugin tools via the global tool registry.

---

## 中文

Scribe Engine 是一个轻量级 Rust Agent，帮助开发者分析代码仓库，并快速产出可发布的博客文档。

## 项目功能

Scribe Engine 主要解决两件事：

1. 代码理解：通过路径检索和内容检索工具，快速定位仓库中的关键实现。
2. 博客发布：将 Markdown 博文发布或更新到 GitHub Pages 仓库。

对程序员来说，它可以帮助你：

- 更快学习开源项目
- 把代码分析过程整理成结构化博客
- 降低从“阅读代码”到“输出内容”的时间成本

## 核心能力

- 具备模型与工具循环调用的 Agent 运行时（`ask`）
- 支持 OpenAI 兼容接口（可配置模型和 API 地址）
- 内置仓库检索工具：
- `glob_search`：查找文件或目录路径
- `grep_search`：查找代码文本内容
- 内置 GitHub Pages 发布工具：
- `github_pages_publish`，并限制只能写入 `posts/*.md`
- 以 CLI 为中心，便于本地脚本化和自动化

## 命令说明

- `tools`：输出工具定义（JSON）
- `tool-call`：手动调用某个工具（名称 + JSON 参数）
- `ask`：让 Agent 根据提示词执行分析或调用工具
- `publish`：发布新博文
- `update`：更新已有博文

## 环境变量

LLM 相关：

- `LLM_API_KEY`（必填）
- `LLM_BASE_URL`（默认：`https://api.openai.com/v1`）
- `LLM_MODEL`（默认：`gpt-4.1-mini`）
- `LLM_SYSTEM_PROMPT`（可选）
- `LLM_WRITE_MODEL_AUDIT_LOG`（可选，默认 `false`）
- `LLM_MODEL_AUDIT_LOG_PATH`（可选，默认 `.auditlog/llm_response_audit.json`）

GitHub Pages 相关（发布时必填）：

- `GITHUB_USERNAME`
- `GITHUB_PASSWORD`
- `GITHUB_PAGES_OWNER`（可选，默认使用用户名）
- `GITHUB_PAGES_BRANCH`（可选，默认 `main`）

## 快速开始

```bash
cargo build

# 查看当前内置工具
cargo run -- tools

# 让 Agent 分析代码并生成博客大纲
cargo run -- ask --prompt "分析这个项目并输出博客大纲" --max-steps 6

# 发布本地 markdown 到 GitHub Pages
cargo run -- publish \
  --path posts/my-post.md \
  --file ./my-post.md \
  --message "publish blog post"
```

## 典型流程

1. 用 `ask` 对目标仓库做分析。
2. 根据分析结果生成博客大纲或初稿。
3. 在本地完善 Markdown。
4. 用 `publish` 或 `update` 同步到 GitHub Pages。

## 说明

- 当前版本聚焦于“代码检索 + 博客发布”闭环。
- 如需扩展能力，可在全局工具注册器中添加插件工具。
