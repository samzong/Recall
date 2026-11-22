use super::*;

pub(super) const GET_MAX_MESSAGES_DEFAULT: u32 = 50;
pub(super) const GET_MESSAGE_CHAR_CAP: usize = 2_000;
pub(super) const GET_RESPONSE_CHAR_CAP: usize = 32_000;
pub(super) const GET_EVENT_LIMIT: usize = 50;
pub(super) const GET_EVENT_FIELD_CHAR_CAP: usize = 200;
pub(super) const GET_EVENT_TEXT_CHAR_CAP: usize = 10_000;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(super) struct GetSessionArgs {
    #[serde(default, deserialize_with = "deserialize_opt_event_ref")]
    pub(super) event_ref: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_evidence_part")]
    pub(super) evidence_part: Option<crate::event_evidence::EvidencePart>,
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(
        description = "UTF-8 byte budget for each event_ref evidence page. Defaults to 8800, range 1024 to 65536.",
        range(min = 1024, max = 65536)
    )]
    pub(super) max_bytes: Option<u32>,
    #[schemars(description = "Recall session id from search_sessions or list_recent_sessions.")]
    #[serde(deserialize_with = "deserialize_required_string")]
    pub(super) session_id: String,
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(
        description = "Maximum messages to return. Defaults to 50. Reads from the start unless tail is true."
    )]
    pub(super) max_messages: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_bool")]
    #[schemars(
        description = "Return the newest messages instead of the oldest, preserving sequence order."
    )]
    pub(super) tail: bool,
    #[serde(default, deserialize_with = "deserialize_bool")]
    #[schemars(
        description = "Include up to 50 structured events anchored to the returned message range, plus unanchored events. Each event string field is capped at 200 characters and all event string fields at 10000 characters total. Defaults to false."
    )]
    pub(super) include_events: bool,
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(
        description = "First message sequence to read; incompatible with around_seq and cursor."
    )]
    pub(super) from_seq: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(description = "Last message sequence to read, inclusive.")]
    pub(super) to_seq: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(
        description = "Read this message and its neighbors. Missing or ambiguous sequences are errors. Defaults to three messages before and after."
    )]
    pub(super) around_seq: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(description = "Actual messages before around_seq, excluding the anchor.")]
    pub(super) before: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(description = "Actual messages after around_seq, excluding the anchor.")]
    pub(super) after: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(
        description = "Unicode character budget for paged message content, default 6000, maximum 6000. Paged responses also stay within an 8800-byte UTF-8 budget. Setting max_chars enables paging without a sequence selector.",
        range(min = 1, max = 6000)
    )]
    pub(super) max_chars: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_cursor")]
    #[schemars(
        description = "Opaque next_cursor copied from the previous page. Use with session_id, without sequence selectors or tail. Reindexing invalidates cursors."
    )]
    pub(super) cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(super) struct SessionDetail {
    pub(super) message: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) source_session_id: Option<String>,
    pub(super) source: Option<String>,
    pub(super) project: Option<String>,
    pub(super) title: Option<String>,
    pub(super) summary: Option<String>,
    pub(super) timestamp: Option<String>,
    pub(super) message_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) locations: Vec<crate::host::Location>,
    #[serde(default, skip_serializing_if = "crate::db::remote_store::is_zero")]
    pub(super) alternative_versions: u32,
    pub(super) returned_messages: usize,
    pub(super) first_message_seq: Option<u32>,
    pub(super) last_message_seq: Option<u32>,
    pub(super) truncated: bool,
    pub(super) messages: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) first_message_byte_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) events: Option<Vec<SessionEventDetail>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) returned_events: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) events_truncated: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(super) struct SessionEventDetail {
    pub(super) event_seq: u32,
    pub(super) timestamp: Option<String>,
    pub(super) kind: String,
    pub(super) actor: String,
    pub(super) name: Option<String>,
    pub(super) status: Option<String>,
    pub(super) target: Option<String>,
    pub(super) message_seq: Option<u32>,
    pub(super) source_event_id: Option<String>,
    pub(super) tool_call_id: Option<String>,
    pub(super) is_meta: Option<bool>,
    pub(super) visibility: Option<EvidenceVisibility>,
    pub(super) summary: Option<String>,
}

pub(super) fn get_ready(
    store: &Store,
    args: &GetSessionArgs,
) -> std::result::Result<SessionDetail, String> {
    if args.max_bytes.is_some() || args.evidence_part.is_some() {
        return Err("evidence parameters require event_ref".into());
    }
    let Some(session) =
        store.get_session_by_id(&args.session_id).map_err(|error| error.to_string())?
    else {
        let message = if store_is_empty(store) {
            EMPTY_INDEX.to_string()
        } else {
            format!("{SESSION_NOT_FOUND} {}", args.session_id)
        };
        return Err(message);
    };
    let window = MessageWindow {
        from_seq: args.from_seq,
        to_seq: args.to_seq,
        around_seq: args.around_seq,
        before: args.before,
        after: args.after,
    };
    window.validate().map_err(|e| e.to_string())?;
    let paging = window.is_selected() || args.cursor.is_some() || args.max_chars.is_some();
    let mut title = session_title(&session);
    let summary = session
        .summary
        .as_deref()
        .map(|text| truncate_chars(&sanitize_lossy_text(text), EXCERPT_CHAR_CAP).0);
    let timestamp = iso8601(session.started_at);
    let mut preloaded_events = None;
    let mut next_cursor = None;
    let mut first_message_byte_offset = None;
    let (text, returned, truncated, first_message_seq, last_message_seq) = if paging {
        if args.tail {
            return Err("tail cannot be combined with message selection or cursor".to_string());
        }
        let max_chars = args.max_chars.unwrap_or(6000);
        if !(1..=6000).contains(&max_chars) {
            return Err("max_chars must be between 1 and 6000".to_string());
        }
        if args.include_events {
            preloaded_events = Some(
                store
                    .list_session_events_for_session(&session.id)
                    .map_err(|error| error.to_string())?,
            );
        }
        let msg_budget = paged_message_byte_budget(
            &session,
            &mut title,
            summary.as_deref(),
            &timestamp,
            preloaded_events.as_ref().is_some_and(|records| !records.is_empty()),
            args.include_events,
        );
        let page = store
            .read_message_page(
                &session.id,
                &MessageRead {
                    window: &window,
                    role: None,
                    max_messages: clamp_limit(args.max_messages, GET_MAX_MESSAGES_DEFAULT, 50),
                    max_chars: max_chars as usize,
                    max_rendered_bytes: Some(msg_budget),
                    cursor: args.cursor.as_deref(),
                },
            )
            .map_err(|e| e.to_string())?;
        first_message_byte_offset = Some(page.first_message_byte_offset);
        let first = page.messages.first().map(|m| m.seq);
        let last = page.messages.last().map(|m| m.seq);
        let text = page
            .messages
            .iter()
            .map(|m| format!("[{}][{}] {}", m.seq, m.role.as_str(), m.content))
            .collect::<Vec<_>>()
            .join("\n\n");
        let truncated = page.next_cursor.is_some();
        next_cursor = page.next_cursor;
        (text, page.messages.len(), truncated, first, last)
    } else {
        let max_messages = clamp_limit(args.max_messages, GET_MAX_MESSAGES_DEFAULT, u32::MAX);
        let messages = store
            .get_messages_in_window(
                &session.id,
                &window,
                None,
                max_messages.min(GET_RESPONSE_CHAR_CAP / 7 + 1),
                args.tail,
            )
            .map_err(|e| e.to_string())?;
        let total: usize = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [&session.id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let (text, returned, truncated, first, last) =
            render_messages(&messages, max_messages, args.tail);
        (text, returned, truncated || total > messages.len(), first, last)
    };
    let (events, returned_events, events_truncated) = if args.include_events {
        let records = match preloaded_events {
            Some(records) => records,
            None => store
                .list_session_events_for_session(&session.id)
                .map_err(|error| error.to_string())?,
        };
        let (events, truncated) =
            render_session_events(records, first_message_seq, last_message_seq, args.tail);
        let returned = events.len();
        (Some(events), Some(returned), Some(truncated))
    } else {
        (None, None, None)
    };
    let mut detail = SessionDetail {
        message: None,
        session_id: Some(session.id.clone()),
        source_session_id: Some(session.source_id.clone()),
        source: Some(session.source.clone()),
        project: session.directory.clone(),
        title: Some(title),
        summary,
        timestamp: Some(timestamp),
        message_count: Some(session.message_count),
        locations: session.locations.clone(),
        alternative_versions: session.alternative_versions,
        returned_messages: returned,
        first_message_seq,
        last_message_seq,
        truncated,
        messages: text,
        next_cursor,
        first_message_byte_offset,
        events,
        returned_events,
        events_truncated,
    };
    if paging && detail.events.is_some() {
        while serde_json::to_vec(&detail).map_or(0, |bytes| bytes.len()) > MCP_INLINE_MAX_BYTES {
            let Some(events) = detail.events.as_mut() else {
                break;
            };
            if events.pop().is_none() {
                break;
            }
            detail.returned_events = Some(events.len());
            detail.events_truncated = Some(true);
        }
    }
    Ok(detail)
}

fn paged_message_byte_budget(
    session: &Session,
    title: &mut String,
    summary: Option<&str>,
    timestamp: &str,
    has_events: bool,
    include_events: bool,
) -> usize {
    let worst_cursor = serde_json::json!({
        "version": 1,
        "session_id": session.id,
        "snapshot": [i64::MAX, u64::MAX],
        "from_seq": u32::MAX,
        "to_seq": u32::MAX,
        "role": "assistant",
        "next_seq": u32::MAX,
        "next_id": i64::MAX,
        "byte_offset": usize::MAX,
    })
    .to_string();
    let mut dummy = SessionDetail {
        message: None,
        session_id: Some(session.id.clone()),
        source_session_id: Some(session.source_id.clone()),
        source: Some(session.source.clone()),
        project: session.directory.clone(),
        title: Some(title.clone()),
        summary: summary.map(str::to_string),
        timestamp: Some(timestamp.to_string()),
        message_count: Some(session.message_count),
        locations: session.locations.clone(),
        alternative_versions: session.alternative_versions,
        returned_messages: 50,
        first_message_seq: Some(u32::MAX),
        last_message_seq: Some(u32::MAX),
        truncated: false,
        messages: String::new(),
        next_cursor: Some(worst_cursor),
        first_message_byte_offset: Some(usize::MAX),
        events: include_events.then(Vec::new),
        returned_events: include_events.then_some(GET_EVENT_LIMIT),
        events_truncated: include_events.then_some(false),
    };
    let mut skeleton = serde_json::to_vec(&dummy).map_or(MCP_INLINE_MAX_BYTES, |bytes| bytes.len());
    if skeleton > MCP_INLINE_MAX_BYTES / 2 {
        *title = truncate_json_bytes(&sanitize_lossy_text(title), 180);
        dummy.title = Some(title.clone());
        skeleton = serde_json::to_vec(&dummy).map_or(MCP_INLINE_MAX_BYTES, |bytes| bytes.len());
    }
    let available = MCP_INLINE_MAX_BYTES.saturating_sub(skeleton);
    if has_events { available.saturating_sub((available / 3).min(2_400)) } else { available }
}

pub(super) fn empty_detail(message: Option<String>) -> SessionDetail {
    SessionDetail { message, ..Default::default() }
}

pub(super) fn render_session_events(
    records: Vec<SessionEventRecord>,
    first_message_seq: Option<u32>,
    last_message_seq: Option<u32>,
    tail: bool,
) -> (Vec<SessionEventDetail>, bool) {
    let total_records = records.len();
    let filtered = records
        .iter()
        .filter(|event| match event.message_seq {
            None => true,
            Some(message_seq) => first_message_seq
                .zip(last_message_seq)
                .is_some_and(|(first, last)| message_seq >= first && message_seq <= last),
        })
        .collect::<Vec<_>>();
    let mut truncated = filtered.len() != total_records;
    let selected = if tail {
        filtered.iter().rev().take(GET_EVENT_LIMIT).copied().collect::<Vec<_>>()
    } else {
        filtered.iter().take(GET_EVENT_LIMIT).copied().collect::<Vec<_>>()
    };
    if selected.len() != filtered.len() {
        truncated = true;
    }

    let mut details = Vec::new();
    let mut text_chars = 0;
    for record in selected {
        let (detail, fields_truncated) = session_event_detail(record);
        let detail_chars = session_event_text_chars(&detail);
        if text_chars + detail_chars > GET_EVENT_TEXT_CHAR_CAP {
            truncated = true;
            break;
        }
        text_chars += detail_chars;
        truncated |= fields_truncated;
        details.push(detail);
    }
    details.sort_by_key(|event| match event.message_seq {
        Some(message_seq) => (0_u8, message_seq, event.event_seq),
        None => (1_u8, 0, event.event_seq),
    });
    (details, truncated)
}

pub(super) fn session_event_detail(record: &SessionEventRecord) -> (SessionEventDetail, bool) {
    let mut truncated = false;
    let mut field = |value: &str| {
        let (text, cut) = truncate_chars(value, GET_EVENT_FIELD_CHAR_CAP);
        truncated |= cut;
        text
    };
    let detail = SessionEventDetail {
        event_seq: record.event_seq,
        timestamp: record.timestamp.map(iso8601),
        kind: field(&record.kind),
        actor: field(&record.actor),
        name: record.name.as_deref().map(&mut field),
        status: record.status.as_deref().map(&mut field),
        target: record.target.as_deref().map(&mut field),
        message_seq: record.message_seq,
        source_event_id: record.source_event_id.as_deref().map(&mut field),
        tool_call_id: record.tool_call_id.as_deref().map(&mut field),
        is_meta: record.is_meta,
        visibility: record.visibility,
        summary: record.summary.as_deref().map(|value| {
            let clean = sanitize_lossy_text(value);
            field(&clean)
        }),
    };
    (detail, truncated)
}

pub(super) fn session_event_text_chars(event: &SessionEventDetail) -> usize {
    event.kind.chars().count()
        + event.actor.chars().count()
        + optional_text_chars(event.timestamp.as_deref())
        + optional_text_chars(event.name.as_deref())
        + optional_text_chars(event.status.as_deref())
        + optional_text_chars(event.target.as_deref())
        + optional_text_chars(event.source_event_id.as_deref())
        + optional_text_chars(event.tool_call_id.as_deref())
        + event.visibility.map(|value| value.as_str().chars().count()).unwrap_or(0)
        + optional_text_chars(event.summary.as_deref())
}

pub(super) fn optional_text_chars(value: Option<&str>) -> usize {
    value.map(|value| value.chars().count()).unwrap_or(0)
}

pub(super) fn render_messages(
    messages: &[Message],
    max_messages: usize,
    tail: bool,
) -> (String, usize, bool, Option<u32>, Option<u32>) {
    let selected = if tail {
        messages.iter().rev().take(max_messages).collect::<Vec<_>>()
    } else {
        messages.iter().take(max_messages).collect::<Vec<_>>()
    };
    let mut blocks = Vec::new();
    let mut bytes = 0;
    let mut truncated = messages.len() > selected.len();
    for message in selected {
        let (body, cut) = truncate_chars(&message.content, GET_MESSAGE_CHAR_CAP);
        if cut {
            truncated = true;
        }
        let block = format!("[{}] {body}", message.role.as_str());
        let extra = if blocks.is_empty() { block.len() } else { block.len() + 2 };
        if bytes + extra > GET_RESPONSE_CHAR_CAP {
            truncated = true;
            break;
        }
        bytes += extra;
        blocks.push((message.seq, block));
    }
    if tail {
        blocks.reverse();
    }
    let returned = blocks.len();
    let first_message_seq = blocks.first().map(|(seq, _)| *seq);
    let last_message_seq = blocks.last().map(|(seq, _)| *seq);
    let text = blocks.into_iter().map(|(_, block)| block).collect::<Vec<_>>().join("\n\n");
    (text, returned, truncated, first_message_seq, last_message_seq)
}
