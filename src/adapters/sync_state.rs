use crate::db::store::{EventSessionStateMeta, UsageSessionStateMeta};

pub(crate) fn usage_state_is_current(
    required_parser_version: u32,
    state: Option<UsageSessionStateMeta>,
    source_updated_at: Option<i64>,
) -> bool {
    state.is_some_and(|state| {
        state.parser_version >= required_parser_version
            && state.source_updated_at == source_updated_at
    })
}

pub(crate) fn event_state_is_current(
    required_parser_version: u32,
    state: Option<EventSessionStateMeta>,
    source_updated_at: Option<i64>,
) -> bool {
    state.is_some_and(|state| {
        state.parser_version >= required_parser_version
            && state.source_updated_at == source_updated_at
    })
}

pub(crate) fn usage_state_is_current_for_mtime(
    required_parser_version: Option<u32>,
    state: Option<UsageSessionStateMeta>,
    mtime_ms: i64,
) -> bool {
    match required_parser_version {
        None => true,
        Some(required) => usage_state_is_current(required, state, Some(mtime_ms)),
    }
}

pub(crate) fn event_state_is_current_for_mtime(
    required_parser_version: Option<u32>,
    state: Option<EventSessionStateMeta>,
    mtime_ms: i64,
) -> bool {
    match required_parser_version {
        None => true,
        Some(required) => event_state_is_current(required, state, Some(mtime_ms)),
    }
}
