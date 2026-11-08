use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_session_show(
    selector: &SessionSelector,
    messages_flag: bool,
    include: Option<&str>,
    window: MessageWindow,
    max_chars: Option<u32>,
    cursor: Option<&str>,
    role: SessionRoleFilter,
    format: SessionDetailFormat,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, selector)?;
    let includes = parse_session_includes(include, messages_flag, format)?;
    let revisions = store.revision_summaries(&session.id)?;
    window.validate()?;
    let paging = window.around_seq.is_some() || max_chars.is_some() || cursor.is_some();
    anyhow::ensure!(
        !(window.is_selected() || paging) || includes.messages,
        "message selection requires --messages or --include messages"
    );
    let role = match role {
        SessionRoleFilter::All => None,
        SessionRoleFilter::User => Some("user"),
        SessionRoleFilter::Assistant => Some("assistant"),
    };
    let (messages, next_cursor, first_message_byte_offset) = if paging {
        let page = store.read_message_page(
            &session.id,
            &MessageRead {
                window: &window,
                role,
                max_messages: 1000,
                max_chars: max_chars.unwrap_or(6000) as usize,
                cursor,
            },
        )?;
        (page.messages, page.next_cursor, Some(page.first_message_byte_offset))
    } else if includes.messages {
        (store.get_messages_in_window(&session.id, &window, role, usize::MAX, false)?, None, None)
    } else {
        (Vec::new(), None, None)
    };
    let usage_events =
        if includes.usage { store.list_usage_events_for_session(&session.id)? } else { Vec::new() };
    let events = if includes.events {
        store.list_session_events_for_session(&session.id)?
    } else {
        Vec::new()
    };

    match format {
        SessionDetailFormat::Text => {
            for revision in &revisions {
                println!(
                    "Version: {}{}",
                    revision["digest"].as_str().unwrap_or_default(),
                    if revision["current"] == true { " (current)" } else { "" }
                );
            }
            println!("Host: {}", crate::host::label(&session.locations));
            for location in &session.locations {
                if let Some(path) = &location.source_file_path {
                    println!("  {}: {path}", location.host.name);
                }
            }
            print!("{}", transcript::render_plain(&session, &messages));
            if !usage_events.is_empty() {
                println!("Usage events: {}", usage_events.len());
            }
            if !events.is_empty() {
                println!("Session events: {}", events.len());
            }
            if let Some(cursor) = next_cursor {
                eprintln!("Continue with --cursor '{}'.", cursor.replace('\'', "'\\''"));
            }
        }
        SessionDetailFormat::Json | SessionDetailFormat::Jsonl => {
            let topology = store.session_topology(&session.id)?;
            let locations = serde_json::to_value(&session.locations)?;
            let mut value = crate::export::session_record_value(
                session,
                topology,
                messages,
                usage_events,
                events,
            )?;
            value["session"]["locations"] = locations;
            if !revisions.is_empty() {
                value["revisions"] = serde_json::to_value(revisions)?;
            }
            if paging {
                value["next_cursor"] = serde_json::json!(next_cursor);
                value["truncated"] = serde_json::json!(next_cursor.is_some());
                value["first_message_byte_offset"] = serde_json::json!(first_message_byte_offset);
            }
            let output = if matches!(format, SessionDetailFormat::Json) {
                serde_json::to_string_pretty(&value)?
            } else {
                serde_json::to_string(&value)?
            };
            println!("{output}");
        }
    }
    Ok(())
}

fn parse_session_includes(
    include: Option<&str>,
    messages_flag: bool,
    format: SessionDetailFormat,
) -> Result<ExportIncludes> {
    let includes = ExportIncludes {
        messages: matches!(format, SessionDetailFormat::Text) || messages_flag,
        usage: false,
        events: false,
    };
    let Some(include) = include else {
        return Ok(includes);
    };
    crate::export::apply_include_filter(includes, include)
}
