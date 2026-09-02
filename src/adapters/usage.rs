use serde_json::Value;

use crate::adapters::json_util::json_i64;
use crate::types::{RawUsageEvent, TokenSource};

impl RawUsageEvent {
    pub(crate) fn observed(
        event_key: String,
        event_seq: u32,
        timestamp: i64,
        parser_version: u32,
    ) -> Self {
        Self::with_source(event_key, event_seq, timestamp, parser_version, TokenSource::Observed)
    }

    pub(crate) fn derived(
        event_key: String,
        event_seq: u32,
        timestamp: i64,
        parser_version: u32,
    ) -> Self {
        Self::with_source(event_key, event_seq, timestamp, parser_version, TokenSource::Derived)
    }

    fn with_source(
        event_key: String,
        event_seq: u32,
        timestamp: i64,
        parser_version: u32,
        token_source: TokenSource,
    ) -> Self {
        Self {
            event_key,
            event_seq,
            message_seq: None,
            timestamp,
            model: "unknown".to_string(),
            provider: "unknown".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            token_source,
            parser_version,
            source_path: None,
            raw_usage_json: None,
        }
    }
}

pub(crate) fn usage_count(usage: &Value, keys: &[&str]) -> i64 {
    keys.iter().find_map(|key| json_i64(usage.get(*key))).unwrap_or(0).max(0)
}

pub(crate) fn disjoint_output_and_reasoning(
    usage: &Value,
    output_tokens: i64,
    reasoning_tokens: i64,
    other_tokens: i64,
) -> (i64, i64) {
    let reported_total = ["totalTokens", "total_tokens"]
        .iter()
        .find_map(|key| json_i64(usage.get(*key)))
        .filter(|total| *total >= 0);
    if reasoning_tokens > 0 && reported_total == Some(other_tokens.saturating_add(output_tokens)) {
        let disjoint_reasoning = reasoning_tokens.min(output_tokens);
        (output_tokens - disjoint_reasoning, disjoint_reasoning)
    } else {
        (output_tokens, reasoning_tokens)
    }
}
