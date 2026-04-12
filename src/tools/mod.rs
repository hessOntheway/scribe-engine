use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

pub mod github_pages;
mod glob_search;
mod grep_search;

pub use glob_search::glob_search_handler;
pub use grep_search::grep_search_handler;

use self::github_pages::github_pages_publish_handler;

#[derive(Debug, Serialize, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub(super) type ToolExecutor = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

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

pub struct GlobalToolRegistry {
    handlers: HashMap<String, ToolHandler>,
}

impl GlobalToolRegistry {
    pub fn builtins() -> Self {
        let mut handlers = HashMap::new();

        let github_pages_publish = github_pages_publish_handler();
        handlers.insert(
            github_pages_publish.name().to_string(),
            github_pages_publish,
        );

        let glob_search = glob_search_handler();
        handlers.insert(glob_search.name().to_string(), glob_search);

        let grep_search = grep_search_handler();
        handlers.insert(grep_search.name().to_string(), grep_search);

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
        self.handlers.values().map(|h| h.definition()).collect()
    }

    pub fn execute(&self, name: &str, input_json: &str) -> Result<String> {
        let handler = self
            .handlers
            .get(name)
            .with_context(|| format!("unsupported tool: {}", name))?;
        handler.run(input_json)
    }
}
