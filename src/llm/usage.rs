use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u64,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u64,
}

impl ModelUsage {
    pub fn cache_hit_tokens(&self) -> u64 {
        self.cache_read_input_tokens + self.prompt_cache_hit_tokens
    }

    pub fn cache_miss_tokens(&self) -> u64 {
        self.cache_creation_input_tokens + self.prompt_cache_miss_tokens
    }

    pub fn has_cache_telemetry(&self) -> bool {
        self.cache_hit_tokens() > 0 || self.cache_miss_tokens() > 0
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptCacheStats {
    #[serde(default)]
    pub total_model_calls: u64,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_input_tokens: u64,
    #[serde(default)]
    pub total_cache_read_input_tokens: u64,
    #[serde(default)]
    pub total_prompt_cache_hit_tokens: u64,
    #[serde(default)]
    pub total_prompt_cache_miss_tokens: u64,
    #[serde(default)]
    pub total_local_cache_hits: u64,
    #[serde(default)]
    pub last_local_cache_hit: bool,
    #[serde(default)]
    pub last_usage: ModelUsage,
}

impl PromptCacheStats {
    pub fn record_usage(&mut self, usage: &ModelUsage) {
        self.total_model_calls += 1;
        self.total_input_tokens += usage.input_tokens + usage.prompt_tokens;
        self.total_output_tokens += usage.output_tokens + usage.completion_tokens;
        self.total_cache_creation_input_tokens += usage.cache_creation_input_tokens;
        self.total_cache_read_input_tokens += usage.cache_read_input_tokens;
        self.total_prompt_cache_hit_tokens += usage.prompt_cache_hit_tokens;
        self.total_prompt_cache_miss_tokens += usage.prompt_cache_miss_tokens;
        self.last_local_cache_hit = false;
        self.last_usage = usage.clone();
    }

    pub fn record_local_cache_hit(&mut self) {
        self.total_local_cache_hits += 1;
        self.last_local_cache_hit = true;
    }

    pub fn last_hit_tokens(&self) -> u64 {
        self.last_usage.cache_hit_tokens()
    }

    pub fn last_miss_tokens(&self) -> u64 {
        self.last_usage.cache_miss_tokens()
    }

    pub fn total_hit_tokens(&self) -> u64 {
        self.total_cache_read_input_tokens + self.total_prompt_cache_hit_tokens
    }

    pub fn total_miss_tokens(&self) -> u64 {
        self.total_cache_creation_input_tokens + self.total_prompt_cache_miss_tokens
    }

    pub fn last_hit_rate(&self) -> Option<f64> {
        ratio(self.last_hit_tokens(), self.last_miss_tokens())
    }

    pub fn total_hit_rate(&self) -> Option<f64> {
        ratio(self.total_hit_tokens(), self.total_miss_tokens())
    }

    pub fn summary_line(&self) -> String {
        let last_hit = self.last_hit_tokens();
        let last_miss = self.last_miss_tokens();
        let total_hit = self.total_hit_tokens();
        let total_miss = self.total_miss_tokens();

        let last_rate = self
            .last_hit_rate()
            .map(format_percent)
            .unwrap_or_else(|| "n/a".to_string());
        let total_rate = self
            .total_hit_rate()
            .map(format_percent)
            .unwrap_or_else(|| "n/a".to_string());

        let local_summary = if self.total_local_cache_hits > 0 {
            format!(
                " local_cache_hits={} last_local_cache_hit={}",
                self.total_local_cache_hits, self.last_local_cache_hit
            )
        } else {
            String::new()
        };

        if self.last_usage.has_cache_telemetry()
            || total_hit > 0
            || total_miss > 0
            || self.total_local_cache_hits > 0
        {
            format!(
                "info: prompt cache stats: call_hit_tokens={last_hit} call_miss_tokens={last_miss} call_hit_rate={last_rate} total_hit_tokens={total_hit} total_miss_tokens={total_miss} total_hit_rate={total_rate} total_model_calls={}{}{local_summary}",
                self.total_model_calls, local_summary
            )
        } else {
            format!(
                "info: prompt cache stats: model_calls={} (no cache telemetry returned yet)",
                self.total_model_calls
            )
        }
    }
}

fn ratio(hit_tokens: u64, miss_tokens: u64) -> Option<f64> {
    let total = hit_tokens + miss_tokens;
    if total == 0 {
        return None;
    }

    Some(hit_tokens as f64 / total as f64)
}

fn format_percent(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}
