use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use crate::db::store::Store;
use crate::types::{ParentRelation, Session, SessionEventRecord, ThreadRole};

mod commands;
mod confer;
#[cfg(test)]
mod tests;

use confer::resolve_confer_peers_for_host;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RelatedRelation {
    Spawn,
    Fork,
    Peer,
}

impl RelatedRelation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Fork => "fork",
            Self::Peer => "peer",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RelatedSession {
    pub(crate) session: Session,
    pub(crate) parent_id: String,
    pub(crate) depth: usize,
    pub(crate) relation: RelatedRelation,
    pub(crate) via: Option<String>,
    pub(crate) role_label: Option<String>,
    pub(crate) display_title: String,
    pub(crate) spawn_anchor: Option<u32>,
}

#[derive(Debug, Clone)]
pub(crate) struct RelatedSessionGraph {
    pub(crate) root: Session,
    pub(crate) related: Vec<RelatedSession>,
}

impl RelatedSessionGraph {
    pub(crate) fn len(&self) -> usize {
        1 + self.related.len()
    }

    pub(crate) fn for_session(store: &Store, session: &Session) -> Result<Self> {
        let confer_path = default_confer_rooms_path();
        Self::for_session_with_confer_path(store, session, confer_path.as_deref())
    }

    pub(crate) fn for_session_with_confer_path(
        store: &Store,
        session: &Session,
        confer_rooms_path: Option<&Path>,
    ) -> Result<Self> {
        let root = resolve_root_session(store, session)?;
        let mut visited = HashSet::new();
        visited.insert(root.id.clone());

        let mut tree_hosts: Vec<(Session, usize)> = vec![(root.clone(), 0)];
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((root.id.clone(), 0));

        let mut raw_nodes: Vec<RawRelatedNode> = Vec::new();
        while let Some((parent_id, depth)) = queue.pop_front() {
            for (relation, child) in store.child_subagent_links(&parent_id)? {
                if !visited.insert(child.id.clone()) {
                    continue;
                }
                let child_depth = depth + 1;
                let rel = match relation {
                    ParentRelation::Spawn => RelatedRelation::Spawn,
                    ParentRelation::Fork | ParentRelation::Resume => RelatedRelation::Fork,
                };
                tree_hosts.push((child.clone(), child_depth));
                queue.push_back((child.id.clone(), child_depth));
                raw_nodes.push(RawRelatedNode {
                    session: child,
                    parent_id: parent_id.clone(),
                    depth: child_depth,
                    relation: rel,
                    via: None,
                    role_label: None,
                    room_id: None,
                    room_name: None,
                });
            }
        }

        let mut host_events_cache: HashMap<String, Vec<SessionEventRecord>> = HashMap::new();
        for (host, host_depth) in &tree_hosts {
            let events = store.list_session_events_for_session(&host.id)?;
            let peers = resolve_confer_peers_for_host(
                store,
                host,
                *host_depth,
                &events,
                confer_rooms_path,
                &mut visited,
            )?;
            raw_nodes.extend(peers);
            host_events_cache.insert(host.id.clone(), events);
        }

        let mut child_events_cache: HashMap<String, Vec<SessionEventRecord>> = HashMap::new();
        let mut related = Vec::with_capacity(raw_nodes.len());
        for node in raw_nodes {
            let parent_events = match host_events_cache.get(&node.parent_id) {
                Some(events) => events.as_slice(),
                None => &[],
            };
            let child_events = if node.relation == RelatedRelation::Peer {
                Vec::new()
            } else if let Some(cached) = host_events_cache.get(&node.session.id) {
                cached.clone()
            } else if let Some(cached) = child_events_cache.get(&node.session.id) {
                cached.clone()
            } else {
                let loaded = store.list_session_events_for_session(&node.session.id)?;
                child_events_cache.insert(node.session.id.clone(), loaded.clone());
                loaded
            };
            let spawn_anchor = resolve_spawn_anchors(
                parent_events,
                &node.session,
                &child_events,
                node.room_id.as_deref(),
            );
            let display_title = format_related_title(
                &node.session,
                node.role_label.as_deref(),
                node.room_name.as_deref(),
            );
            related.push(RelatedSession {
                session: node.session,
                parent_id: node.parent_id,
                depth: node.depth,
                relation: node.relation,
                via: node.via,
                role_label: node.role_label,
                display_title,
                spawn_anchor,
            });
        }

        Ok(Self { root, related })
    }
}

struct RawRelatedNode {
    session: Session,
    parent_id: String,
    depth: usize,
    relation: RelatedRelation,
    via: Option<String>,
    role_label: Option<String>,
    room_id: Option<String>,
    room_name: Option<String>,
}

pub(crate) fn resolve_root_session(store: &Store, session: &Session) -> Result<Session> {
    let mut current = session.clone();
    let mut visited = HashSet::new();
    visited.insert(current.id.clone());

    loop {
        let topology = store.session_topology(&current.id)?;
        let mut next_parent: Option<Session> = None;

        for link in topology.parents.iter().filter(|l| l.relation == ParentRelation::Spawn) {
            if let Some(parent) = store.resolve_parent(&current.id, link)?
                && visited.insert(parent.id.clone())
            {
                next_parent = Some(parent);
                break;
            }
        }

        if next_parent.is_none() && topology.thread_role == Some(ThreadRole::Subagent) {
            for link in topology.parents.iter().filter(|l| l.relation == ParentRelation::Fork) {
                if let Some(parent) = store.resolve_parent(&current.id, link)?
                    && visited.insert(parent.id.clone())
                {
                    next_parent = Some(parent);
                    break;
                }
            }
        }

        let Some(parent) = next_parent else {
            break;
        };
        current = parent;
    }

    Ok(current)
}

fn default_confer_rooms_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let home = dirs::home_dir()?;
    Some(home.join(".local").join("state").join("confer").join("rooms.json"))
}

pub(crate) fn format_related_title(
    session: &Session,
    role_label: Option<&str>,
    room_name: Option<&str>,
) -> String {
    let raw = session.custom_title.as_deref().unwrap_or(&session.title).trim();
    if let (Some(seat), Some(room)) = (role_label, room_name)
        && !seat.trim().is_empty()
        && !room.trim().is_empty()
        && is_generic_or_confer_title(raw)
    {
        return format!("[{}] {}", seat.trim(), room.trim());
    }
    if raw.is_empty() { "Untitled".to_string() } else { raw.to_string() }
}

fn is_generic_or_confer_title(title: &str) -> bool {
    let lower = title.trim().to_ascii_lowercase();
    lower.is_empty()
        || lower == "untitled"
        || lower == "new session"
        || lower.starts_with("# new session")
        || lower.starts_with("confer")
        || lower.starts_with("[confer")
        || lower.starts_with("do not call confer mcp tools")
        || lower.starts_with("you are participating in a confer")
}

pub(crate) fn resolve_spawn_anchors(
    parent_events: &[SessionEventRecord],
    child_session: &Session,
    child_events: &[SessionEventRecord],
    child_room_id: Option<&str>,
) -> Option<u32> {
    let mut subagent_tool_call_ids = Vec::new();
    let mut child_agent_paths = Vec::new();
    for event in child_events {
        if event.kind == "subagent_meta" || event.name.as_deref() == Some("subagent_meta") {
            if let Some(call_id) =
                event.tool_call_id.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                subagent_tool_call_ids.push(call_id.to_string());
            }
            if let Some(attrs) = event.attrs_json.as_deref()
                && let Ok(val) = serde_json::from_str::<Value>(attrs)
            {
                if let Some(call_id) = val.get("tool_call_id").and_then(Value::as_str)
                    && !call_id.trim().is_empty()
                {
                    subagent_tool_call_ids.push(call_id.trim().to_string());
                }
                if let Some(path) = val.get("agent_path").and_then(Value::as_str)
                    && !path.trim().is_empty()
                {
                    child_agent_paths.push(path.trim().to_string());
                }
            }
        }
    }
    if let Some(path) = child_session.source_file_path.as_deref().filter(|p| !p.trim().is_empty()) {
        child_agent_paths.push(path.trim().to_string());
    }

    let stripped_agent_id = child_session
        .source_id
        .strip_prefix("agent-")
        .or_else(|| child_session.source_id.rsplit_once(":agent-").map(|(_, tail)| tail))
        .filter(|s| !s.is_empty());

    let contains_child_id = |text: &str| -> bool {
        text.contains(&child_session.source_id)
            || stripped_agent_id.is_some_and(|stripped| text.contains(stripped))
    };

    for call_id in &subagent_tool_call_ids {
        if let Some(seq) = parent_events.iter().find_map(|event| {
            event.message_seq.filter(|_| event.tool_call_id.as_deref() == Some(call_id.as_str()))
        }) {
            return Some(seq);
        }
    }

    let call_seq_by_id: HashMap<&str, u32> = parent_events
        .iter()
        .filter_map(|event| {
            if event.kind != "tool_result" {
                Some((event.tool_call_id.as_deref()?, event.message_seq?))
            } else {
                None
            }
        })
        .collect();

    for event in parent_events {
        if event.kind == "tool_result" && event_text_matches(event, contains_child_id) {
            if let Some(seq) = event.message_seq {
                return Some(seq);
            }
            if let Some(call_id) = event.tool_call_id.as_deref()
                && let Some(&seq) = call_seq_by_id.get(call_id)
            {
                return Some(seq);
            }
        }
    }

    for event in parent_events {
        let is_spawn = event.name.as_deref() == Some("spawn_agent")
            || event.kind == "spawn_agent"
            || event.kind == "tool_result";
        if !is_spawn {
            continue;
        }
        let matches = event_text_matches(event, |text| {
            text.contains(&child_session.source_id)
                || child_agent_paths.iter().any(|path| text.contains(path.as_str()))
        });
        if matches {
            if let Some(seq) = event.message_seq {
                return Some(seq);
            }
            if let Some(call_id) = event.tool_call_id.as_deref()
                && let Some(&seq) = call_seq_by_id.get(call_id)
            {
                return Some(seq);
            }
        }
    }

    for (idx, event) in parent_events.iter().enumerate() {
        let is_invoke =
            event.name.as_deref() == Some("invoke_subagent") || event.kind == "invoke_subagent";
        let is_native = event.kind == "native_record";
        if !is_invoke && !is_native {
            continue;
        }
        if event_text_matches(event, |text| text.contains(&child_session.source_id)) {
            if let Some(seq) = event.message_seq {
                return Some(seq);
            }
            if is_native {
                for offset in [1isize, -1, 2, -2] {
                    let neighbor_idx = idx as isize + offset;
                    if neighbor_idx >= 0
                        && let Some(neighbor) = parent_events.get(neighbor_idx as usize)
                        && (neighbor.name.as_deref() == Some("invoke_subagent")
                            || neighbor.kind == "invoke_subagent")
                        && let Some(seq) = neighbor.message_seq
                    {
                        return Some(seq);
                    }
                }
            }
        }
    }

    if child_room_id.is_some() || !subagent_tool_call_ids.is_empty() || child_events.is_empty() {
        for event in parent_events {
            let matches = event_text_matches(event, |text| {
                child_room_id.is_some_and(|rid| text.contains(rid))
                    || text.contains(&child_session.source_id)
            });
            if matches {
                if let Some(seq) = event.message_seq {
                    return Some(seq);
                }
                if event.kind == "tool_result"
                    && let Some(call_id) = event.tool_call_id.as_deref()
                    && let Some(&seq) = call_seq_by_id.get(call_id)
                {
                    return Some(seq);
                }
            }
        }
    }

    None
}

fn event_text_matches(event: &SessionEventRecord, pred: impl Fn(&str) -> bool) -> bool {
    event.attrs_json.as_deref().is_some_and(&pred)
        || event.summary.as_deref().is_some_and(&pred)
        || event.target.as_deref().is_some_and(&pred)
}
