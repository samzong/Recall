use crate::types::{Message, Role, SessionUsageEventRecord};
use crate::usage::TokenTotals;

pub(crate) struct SanitizedLine {
    pub(crate) text: String,
    pub(crate) lower: String,
}

pub(crate) fn build_viewing_caches(msgs: &[Message]) -> Vec<Vec<SanitizedLine>> {
    msgs.iter()
        .map(|m| {
            m.content
                .lines()
                .map(|line| {
                    let text = crate::utils::sanitize_line(line);
                    let lower = text.to_lowercase();
                    SanitizedLine { text, lower }
                })
                .collect()
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ViewingSessionSummary {
    pub(crate) user_messages: usize,
    pub(crate) total_messages: usize,
    pub(crate) duration_minutes: Option<u32>,
    pub(crate) usage_events: usize,
    pub(crate) tokens: TokenTotals,
}

impl ViewingSessionSummary {
    pub(crate) fn from_session(
        messages: &[Message],
        duration_minutes: Option<u32>,
        usage_events: &[SessionUsageEventRecord],
    ) -> Self {
        let mut tokens = TokenTotals::default();
        for event in usage_events {
            tokens.input_tokens += event.input_tokens.max(0);
            tokens.output_tokens += event.output_tokens.max(0);
            tokens.cache_read_tokens += event.cache_read_tokens.max(0);
            tokens.cache_write_tokens += event.cache_write_tokens.max(0);
            tokens.reasoning_tokens += event.reasoning_tokens.max(0);
        }
        tokens.total_tokens = tokens.input_tokens
            + tokens.output_tokens
            + tokens.cache_read_tokens
            + tokens.cache_write_tokens
            + tokens.reasoning_tokens;

        Self {
            user_messages: messages.iter().filter(|msg| msg.role == Role::User).count(),
            total_messages: messages.len(),
            duration_minutes: duration_minutes.or_else(|| message_span_minutes(messages)),
            usage_events: usage_events.len(),
            tokens,
        }
    }
}

fn message_span_minutes(messages: &[Message]) -> Option<u32> {
    let mut timestamps = messages.iter().filter_map(|msg| msg.timestamp);
    let first = timestamps.next()?;
    let (min_ts, max_ts) =
        timestamps.fold((first, first), |(min_ts, max_ts), ts| (min_ts.min(ts), max_ts.max(ts)));
    Some(max_ts.saturating_sub(min_ts).div_euclid(60_000) as u32)
}
