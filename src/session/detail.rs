use super::*;
use crate::session::related::RelatedSessionGraph;

#[derive(serde::Serialize)]
struct TopologyChildSummary {
    id: String,
    source: String,
    source_id: String,
    relation: &'static str,
    title: String,
    message_count: u32,
    started_at: i64,
}

#[derive(serde::Serialize)]
struct RelatedSessionJson {
    id: String,
    source: String,
    source_id: String,
    parent_id: String,
    depth: usize,
    relation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role_label: Option<String>,
    title: String,
    message_count: u32,
    started_at: i64,
}

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
                max_rendered_bytes: None,
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
            let value = build_session_show_value(
                &store,
                session,
                messages,
                usage_events,
                events,
                revisions,
                paging,
                next_cursor,
                first_message_byte_offset,
            )?;
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

#[allow(clippy::too_many_arguments)]
fn build_session_show_value(
    store: &Store,
    session: Session,
    messages: Vec<crate::types::Message>,
    usage_events: Vec<crate::types::SessionUsageEventRecord>,
    events: Vec<crate::types::SessionEventRecord>,
    revisions: Vec<serde_json::Value>,
    paging: bool,
    next_cursor: Option<String>,
    first_message_byte_offset: Option<usize>,
) -> Result<serde_json::Value> {
    let target_id = session.id.clone();
    let topology = store.session_topology(&target_id)?;
    let children: Vec<TopologyChildSummary> = store
        .child_subagent_links(&target_id)?
        .into_iter()
        .map(|(relation, child)| TopologyChildSummary {
            id: child.id,
            source: child.source,
            source_id: child.source_id,
            relation: relation.as_str(),
            title: child.title,
            message_count: child.message_count,
            started_at: child.started_at,
        })
        .collect();
    let related_graph = RelatedSessionGraph::for_session(store, &session)?;
    let related: Vec<RelatedSessionJson> = related_graph
        .related
        .into_iter()
        .filter(|item| item.session.id != target_id)
        .map(|item| RelatedSessionJson {
            id: item.session.id,
            source: item.session.source,
            source_id: item.session.source_id,
            parent_id: item.parent_id,
            depth: item.depth,
            relation: item.relation.as_str(),
            via: item.via,
            role_label: item.role_label,
            title: item.display_title,
            message_count: item.session.message_count,
            started_at: item.session.started_at,
        })
        .collect();
    let locations = serde_json::to_value(&session.locations)?;
    let mut value =
        crate::export::session_record_value(session, topology, messages, usage_events, events)?;
    value["session"]["locations"] = locations;
    value["session"]["topology"]["children"] = serde_json::to_value(children)?;
    value["related"] = serde_json::to_value(related)?;
    if !revisions.is_empty() {
        value["revisions"] = serde_json::to_value(revisions)?;
    }
    if paging {
        value["next_cursor"] = serde_json::json!(next_cursor);
        value["truncated"] = serde_json::json!(next_cursor.is_some());
        value["first_message_byte_offset"] = serde_json::json!(first_message_byte_offset);
    }
    Ok(value)
}

fn parse_session_includes(
    include: Option<&str>,
    messages_flag: bool,
    format: SessionDetailFormat,
) -> Result<ExportIncludes> {
    let includes = ExportIncludes {
        messages: matches!(format, SessionDetailFormat::Text) || messages_flag,
        usage: false,
        usage_ownership: false,
        events: false,
    };
    let Some(include) = include else {
        return Ok(includes);
    };
    let includes = crate::export::apply_include_filter(includes, include)?;
    anyhow::ensure!(!includes.usage_ownership, "--include usage-ownership requires export");
    Ok(includes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::db::store::SessionTopologyWrite;
    use crate::types::{ParentLink, ParentRelation, Role, ThreadRole};

    #[test]
    fn show_json_preserves_topology_and_adds_children_and_related() {
        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let root = Session {
            id: "r1".into(),
            source: "codex".into(),
            source_id: "r1-src".into(),
            title: "Root".into(),
            message_count: 1,
            started_at: 100,
            ..crate::types::test_support::session("r1")
        };
        let child = Session {
            id: "c1".into(),
            source: "codex".into(),
            source_id: "c1-src".into(),
            title: "Child".into(),
            message_count: 1,
            started_at: 200,
            ..crate::types::test_support::session("c1")
        };
        store
            .persist_session_with_usage_and_events_with_topology(
                &root,
                &[crate::types::test_support::message("r1", Role::User, "hi", 0)],
                &[],
                None,
                &[],
                None,
                &SessionTopologyWrite {
                    thread_role: Some(ThreadRole::Primary),
                    parents: &[],
                    parser_version: Some(1),
                },
            )
            .unwrap();
        store
            .persist_session_with_usage_and_events_with_topology(
                &child,
                &[crate::types::test_support::message("c1", Role::User, "sub", 0)],
                &[],
                None,
                &[],
                None,
                &SessionTopologyWrite {
                    thread_role: Some(ThreadRole::Subagent),
                    parents: &[ParentLink {
                        relation: ParentRelation::Spawn,
                        source: "codex".into(),
                        source_id: "r1-src".into(),
                    }],
                    parser_version: Some(1),
                },
            )
            .unwrap();

        let root_json = build_session_show_value(
            &store,
            root,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(root_json["session"]["topology"]["thread_role"], "primary");
        assert_eq!(root_json["session"]["topology"]["parents"], serde_json::json!([]));
        assert_eq!(root_json["session"]["topology"]["children"][0]["id"], "c1");
        assert_eq!(root_json["session"]["topology"]["children"][0]["relation"], "spawn");
        assert_eq!(root_json["related"][0]["id"], "c1");
        assert_eq!(root_json["related"][0]["parent_id"], "r1");
        assert_eq!(root_json["related"][0]["depth"], 1);
        assert_eq!(root_json["related"][0]["relation"], "spawn");
        assert!(root_json["related"][0].get("via").is_none());

        let child_json = build_session_show_value(
            &store,
            child,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(child_json["session"]["topology"]["children"], serde_json::json!([]));
        assert_eq!(child_json["related"], serde_json::json!([]));
    }
}
