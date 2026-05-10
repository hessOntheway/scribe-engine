use std::fs::{File, OpenOptions, create_dir_all};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::openai::ChatCompletionResult;

#[derive(Debug, Clone)]
pub struct PromptCache {
    dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromptCacheEntry {
    pub created_at_unix_ms: u128,
    pub message: Value,
    pub usage: super::usage::ModelUsage,
}

impl PromptCache {
    pub fn new(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        create_dir_all(&dir)
            .with_context(|| format!("failed to create prompt cache dir: {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn lookup(&self, key: &str) -> Result<Option<ChatCompletionResult>> {
        let path = self.entry_path(key);
        if !path.exists() {
            return Ok(None);
        }

        let mut file = File::open(&path)
            .with_context(|| format!("failed to open prompt cache entry: {}", path.display()))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .with_context(|| format!("failed to read prompt cache entry: {}", path.display()))?;

        let entry: PromptCacheEntry = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse prompt cache entry: {}", path.display()))?;

        Ok(Some(ChatCompletionResult {
            message: entry.message,
            usage: entry.usage,
            cached: true,
        }))
    }

    pub fn store(&self, key: &str, response: &ChatCompletionResult) -> Result<()> {
        let entry = PromptCacheEntry {
            created_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock error")?
                .as_millis(),
            message: response.message.clone(),
            usage: response.usage.clone(),
        };

        let path = self.entry_path(key);
        let tmp_path = path.with_extension("json.tmp");
        create_dir_all(&self.dir).with_context(|| {
            format!("failed to create prompt cache dir: {}", self.dir.display())
        })?;
        if let Some(parent) = tmp_path.parent() {
            create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create prompt cache parent dir: {}",
                    parent.display()
                )
            })?;
        }
        let contents = serde_json::to_string_pretty(&entry)
            .context("failed to serialize prompt cache entry")?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| {
                format!(
                    "failed to create prompt cache temp file: {}",
                    tmp_path.display()
                )
            })?;
        file.write_all(contents.as_bytes()).with_context(|| {
            format!(
                "failed to write prompt cache temp file: {}",
                tmp_path.display()
            )
        })?;
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("failed to persist prompt cache entry: {}", path.display()))?;
        Ok(())
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.json", key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::openai::ChatCompletionResult;
    use crate::llm::usage::ModelUsage;
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn prompt_cache_roundtrip() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let base = std::env::temp_dir().join(format!("prompt_cache_test_{}", unique));
        let cache = PromptCache::new(&base).expect("construct cache");

        let request_key = "test-request";
        let response = ChatCompletionResult {
            message: json!({"role": "assistant", "content": "hello"}),
            usage: ModelUsage {
                input_tokens: 10,
                output_tokens: 5,
                prompt_cache_hit_tokens: 0,
                prompt_cache_miss_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                ..Default::default()
            },
            cached: false,
        };

        cache
            .store(request_key, &response)
            .expect("store cache entry");
        let loaded = cache
            .lookup(request_key)
            .expect("lookup cache entry")
            .expect("entry present");

        assert_eq!(loaded.message, response.message);
        assert_eq!(loaded.usage.input_tokens, response.usage.input_tokens);
        assert!(loaded.cached);

        fs::remove_dir_all(&base).expect("cleanup cache dir");
    }
}
