use crate::db::store::{EventSessionStateMeta, MetadataSessionStateMeta, UsageSessionStateMeta};

pub(crate) fn usage_state_is_current(
    required_parser_version: u32,
    state: Option<UsageSessionStateMeta>,
    source_updated_at: Option<i64>,
) -> bool {
    parser_state_is_current(
        required_parser_version,
        state.map(|s| (s.parser_version, s.source_updated_at)),
        source_updated_at,
    )
}

pub(crate) fn event_state_is_current(
    required_parser_version: u32,
    state: Option<EventSessionStateMeta>,
    source_updated_at: Option<i64>,
) -> bool {
    parser_state_is_current(
        required_parser_version,
        state.map(|s| (s.parser_version, s.source_updated_at)),
        source_updated_at,
    )
}

pub(crate) fn metadata_state_is_current(
    required_parser_version: u32,
    state: Option<MetadataSessionStateMeta>,
    source_updated_at: Option<i64>,
) -> bool {
    parser_state_is_current(
        required_parser_version,
        state.map(|s| (s.parser_version, s.source_updated_at)),
        source_updated_at,
    )
}

pub(crate) fn session_state_is_current(
    usage_parser_version: u32,
    event_parser_version: u32,
    usage_state: Option<UsageSessionStateMeta>,
    event_state: Option<EventSessionStateMeta>,
    source_updated_at: Option<i64>,
    include_events: bool,
) -> bool {
    usage_state_is_current(usage_parser_version, usage_state, source_updated_at)
        && (!include_events
            || event_state_is_current(event_parser_version, event_state, source_updated_at))
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

pub(crate) fn metadata_state_is_current_for_mtime(
    required_parser_version: Option<u32>,
    state: Option<MetadataSessionStateMeta>,
    mtime_ms: i64,
) -> bool {
    match required_parser_version {
        None => true,
        Some(required) => metadata_state_is_current(required, state, Some(mtime_ms)),
    }
}

fn parser_state_is_current(
    required_parser_version: u32,
    state: Option<(u32, Option<i64>)>,
    source_updated_at: Option<i64>,
) -> bool {
    state.is_some_and(|(parser_version, state_updated_at)| {
        parser_version >= required_parser_version && state_updated_at == source_updated_at
    })
}
