use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

pub mod github_wiki;
mod glob_search;
mod grep_search;
mod mcp;
mod read_file;
mod web_fetch;
mod write_file;
pub mod team;
mod todo_write;

pub use glob_search::glob_search_handler;
pub use grep_search::grep_search_handler;
pub use mcp::mcp_plugin_tools_from_config;
pub use read_file::read_file_handler;
pub use task::TaskRegistry;
pub use web_fetch::web_fetch_handler;
pub use team::{TeamManager, team_tool_handlers};
pub use todo_write::todo_write_handler;
pub use write_file::write_file_handler;

use self::github_wiki::github_wiki_publish_handler;

#[derive(Debug, Serialize, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub(super) type ToolExecutor = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

#[derive(Clone)]
pub struct ToolHandler {
    definition: ToolDefinition,
    execute: ToolExecutor,
}

impl ToolHandler {
    pub fn new(definition: ToolDefinition, execute: ToolExecutor) -> Self {
        Self {
            definition,
            execute,
        }
    }

    fn run(&self, input_json: &str) -> Result<String> {
        (self.execute)(input_json)
    }

    fn name(&self) -> &str {
        &self.definition.name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }
}

#[derive(Clone)]
pub struct GlobalToolRegistry {
    handlers: HashMap<String, ToolHandler>,
}

impl GlobalToolRegistry {
    pub fn builtins() -> Self {
        let mut handlers = HashMap::new();

        let github_wiki_publish = github_wiki_publish_handler();
        handlers.insert(
            github_wiki_publish.name().to_string(),
            github_wiki_publish,
        );

        let glob_search = glob_search_handler();
        handlers.insert(glob_search.name().to_string(), glob_search);

        let grep_search = grep_search_handler();
        handlers.insert(grep_search.name().to_string(), grep_search);

        let read_file = read_file_handler();
        handlers.insert(read_file.name().to_string(), read_file);

        let web_fetch = web_fetch_handler();
        handlers.insert(web_fetch.name().to_string(), web_fetch);

        let write_file = write_file_handler();
        handlers.insert(write_file.name().to_string(), write_file);

        let todo_write = todo_write_handler();
        handlers.insert(todo_write.name().to_string(), todo_write);

        Self { handlers }
    }

    pub fn with_tool(mut self, tool: ToolHandler) -> Result<Self> {
        let name = tool.name().to_string();
        if self.handlers.contains_key(&name) {
            bail!("tool name conflicts with existing handler: {}", name);
        }

        self.handlers.insert(name, tool);
        Ok(self)
    }

    pub fn without_tool(&self, name: &str) -> Self {
        let handlers = self
            .handlers
            .iter()
            .filter(|(tool_name, _)| tool_name.as_str() != name)
            .map(|(tool_name, handler)| (tool_name.clone(), handler.clone()))
            .collect();

        Self { handlers }
    }

    pub fn with_plugin_tools(mut self, plugin_tools: Vec<ToolHandler>) -> Result<Self> {
        let mut plugin_names = HashSet::new();

        for plugin in plugin_tools {
            let name = plugin.name().to_string();
            if self.handlers.contains_key(&name) {
                bail!("plugin tool name conflicts with builtin: {}", name);
            }
            if !plugin_names.insert(name.clone()) {
                bail!("duplicate plugin tool name: {}", name);
            }
            self.handlers.insert(name, plugin);
        }

        Ok(self)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<ToolDefinition> = self.handlers.values().map(|h| h.definition()).collect();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    pub fn execute(&self, name: &str, input_json: &str) -> Result<String> {
        let handler = self
            .handlers
            .get(name)
            .with_context(|| format!("unsupported tool: {}", name))?;
        handler.run(input_json)
    }
}

pub mod task;
