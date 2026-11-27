use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::db::store::Store;
use crate::types::{Session, SessionEventRecord};

use super::commands::parse_resume_commands_in_text;
use super::{RawRelatedNode, RelatedRelation};

#[derive(Deserialize)]
struct ConferRoomsFile {
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    rooms: Vec<ConferRoom>,
}

#[derive(Deserialize)]
struct ConferRoom {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    seats: Vec<ConferSeat>,
}

#[derive(Deserialize)]
struct ConferSeat {
    name: String,
    agent: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    native_session_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ConferSeatBinding {
    source: String,
    native_session_id: String,
    room_id: Option<String>,
    room_name: Option<String>,
    seat_name: Option<String>,
    from_event: bool,
}

#[derive(Default)]
struct ConferEventSignals {
    room_ids: Vec<String>,
    room_names: HashMap<String, String>,
    bindings: Vec<ConferSeatBinding>,
}

pub(super) fn resolve_confer_peers_for_host(
    store: &Store,
    host: &Session,
    host_depth: usize,
    events: &[SessionEventRecord],
    confer_rooms_path: Option<&Path>,
    visited: &mut HashSet<String>,
) -> anyhow::Result<Vec<RawRelatedNode>> {
    let signals = extract_confer_signals(events);
    if signals.room_ids.is_empty() && signals.bindings.is_empty() {
        return Ok(Vec::new());
    }

    let mut merged = signals.bindings;
    if !signals.room_ids.is_empty()
        && let Some(path) = confer_rooms_path
        && let Some(rooms_file) = read_confer_rooms_file(path)
    {
        let room_set: HashSet<&str> = signals.room_ids.iter().map(String::as_str).collect();
        for room in rooms_file.rooms {
            if !room_set.contains(room.id.as_str()) {
                continue;
            }
            let room_name =
                room.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(String::from);
            for seat in room.seats {
                let Some(native_id) =
                    seat.native_session_id.as_deref().map(str::trim).filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let _ = &seat.model;
                let source = map_confer_agent_to_source(&seat.agent);
                let seat_name = seat.name.trim().to_string();

                let existing_by_seat = merged.iter_mut().find(|b| {
                    b.from_event
                        && (b.room_id.is_none() || b.room_id.as_deref() == Some(room.id.as_str()))
                        && b.seat_name
                            .as_deref()
                            .is_some_and(|s| s.eq_ignore_ascii_case(&seat_name))
                });
                if let Some(existing) = existing_by_seat {
                    if existing.room_id.is_none() {
                        existing.room_id = Some(room.id.clone());
                    }
                    if existing.source != source || existing.native_session_id != native_id {
                        eprintln!(
                            "Warning: confer binding conflict for room {} seat {}: event {}:{} overrides rooms.json {}:{}",
                            room.id,
                            seat_name,
                            existing.source,
                            existing.native_session_id,
                            source,
                            native_id
                        );
                    }
                    if existing.room_name.is_none() {
                        existing.room_name = room_name.clone();
                    }
                    continue;
                }

                let existing_by_id = merged
                    .iter_mut()
                    .find(|b| b.source == source && b.native_session_id == native_id);
                if let Some(existing) = existing_by_id {
                    if existing.room_id.is_none() {
                        existing.room_id = Some(room.id.clone());
                    }
                    if existing.room_name.is_none() {
                        existing.room_name = room_name.clone();
                    }
                    if existing.seat_name.is_none() && !seat_name.is_empty() {
                        existing.seat_name = Some(seat_name);
                    }
                    continue;
                }

                merged.push(ConferSeatBinding {
                    source: source.to_string(),
                    native_session_id: native_id.to_string(),
                    room_id: Some(room.id.clone()),
                    room_name: room_name.clone(),
                    seat_name: (!seat_name.is_empty()).then_some(seat_name),
                    from_event: false,
                });
            }
        }
    }

    let mut peers = Vec::new();
    for mut binding in merged {
        if binding.room_name.is_none()
            && let Some(room_id) = binding.room_id.as_deref()
        {
            binding.room_name = signals.room_names.get(room_id).cloned();
        }
        let Some(peer_session) =
            resolve_indexed_session(store, &binding.source, &binding.native_session_id)?
        else {
            continue;
        };
        if !visited.insert(peer_session.id.clone()) {
            continue;
        }
        peers.push(RawRelatedNode {
            session: peer_session,
            parent_id: host.id.clone(),
            depth: host_depth + 1,
            relation: RelatedRelation::Peer,
            via: Some("confer".to_string()),
            role_label: binding.seat_name,
            room_id: binding.room_id,
            room_name: binding.room_name,
        });
    }
    Ok(peers)
}

fn read_confer_rooms_file(path: &Path) -> Option<ConferRoomsFile> {
    let raw = fs::read_to_string(path).ok()?;
    let parsed: ConferRoomsFile = serde_json::from_str(&raw).ok()?;
    let schema = parsed.version.or(parsed.schema_version);
    if !matches!(schema, None | Some(1..=3)) {
        return None;
    }
    Some(parsed)
}

fn resolve_indexed_session(
    store: &Store,
    source: &str,
    source_id: &str,
) -> anyhow::Result<Option<Session>> {
    if let Some(session) = store.get_native_session(source, source_id)? {
        return Ok(Some(session));
    }
    Ok(store.get_session_by_source_id(source, source_id).ok().flatten())
}

fn map_confer_agent_to_source(agent: &str) -> &str {
    match agent.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude-code" => "claude-code",
        "codex" => "codex",
        "cursor" | "cursor-agent" => "cursor",
        "grok" => "grok",
        "agy" | "antigravity" | "antigravity-cli" => "antigravity-cli",
        "copilot" | "copilot-cli" => "copilot-cli",
        "kimi" | "kimi-code" => "kimi-code",
        "devin" => "devin",
        "opencode" => "opencode",
        "rx" | "rx-agent" => "rx-agent",
        _ => agent.trim(),
    }
}

fn extract_confer_signals(events: &[SessionEventRecord]) -> ConferEventSignals {
    let mut signals = ConferEventSignals::default();
    for event in events {
        let is_confer_event = event
            .name
            .as_deref()
            .is_some_and(|n| n.contains("confer") || n.starts_with("mcp__confer__"))
            || event
                .attrs_json
                .as_deref()
                .is_some_and(|s| s.contains("confer") || s.contains("room_id"))
            || event
                .summary
                .as_deref()
                .is_some_and(|s| s.contains("confer") || s.contains("room_id"));

        if let Some(attrs) = event.attrs_json.as_deref() {
            if let Ok(value) = serde_json::from_str::<Value>(attrs) {
                scan_json_value(&value, is_confer_event, None, None, &mut signals);
            } else {
                scan_text_signals(attrs, is_confer_event, None, None, &mut signals);
            }
        }
        if let Some(summary) = event.summary.as_deref() {
            scan_text_signals(summary, is_confer_event, None, None, &mut signals);
        }
    }
    if signals.room_ids.len() == 1 {
        let only_room_id = &signals.room_ids[0];
        for binding in &mut signals.bindings {
            if binding.room_id.is_none() {
                binding.room_id = Some(only_room_id.clone());
            }
        }
    }
    signals
}

fn scan_json_value(
    value: &Value,
    is_confer_event: bool,
    inherited_room_id: Option<&str>,
    inherited_room_name: Option<&str>,
    signals: &mut ConferEventSignals,
) {
    match value {
        Value::Object(map) => {
            let own_room_id = map
                .get("room_id")
                .or_else(|| map.get("roomId"))
                .and_then(Value::as_str)
                .filter(|s| is_uuid_36(s))
                .map(str::to_string);
            let own_room_name = map
                .get("room_name")
                .or_else(|| map.get("roomName"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let effective_room_id = own_room_id.as_deref().or(inherited_room_id);
            let effective_room_name = own_room_name.as_deref().or(inherited_room_name);
            if let Some(rid) = effective_room_id {
                push_unique_str(&mut signals.room_ids, rid);
                if let Some(rname) = effective_room_name {
                    signals.room_names.entry(rid.to_string()).or_insert_with(|| rname.to_string());
                }
            }
            let seat_name = map
                .get("seat")
                .or_else(|| map.get("seat_name"))
                .or_else(|| map.get("seatName"))
                .or_else(|| map.get("role"))
                .or_else(|| {
                    map.get("name").filter(|v| {
                        v.as_str().is_some_and(|n| {
                            !n.starts_with("mcp__confer__") && !n.starts_with("confer_")
                        })
                    })
                })
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            if let Some(cmd) = map
                .get("resume_command")
                .or_else(|| map.get("resumeCommand"))
                .and_then(Value::as_str)
            {
                for (source, native_id) in parse_resume_commands_in_text(cmd) {
                    push_binding(
                        &mut signals.bindings,
                        ConferSeatBinding {
                            source,
                            native_session_id: native_id,
                            room_id: effective_room_id.map(str::to_string),
                            room_name: effective_room_name.map(str::to_string),
                            seat_name: seat_name.clone(),
                            from_event: true,
                        },
                    );
                }
            }

            for child in map.values() {
                scan_json_value(
                    child,
                    is_confer_event,
                    effective_room_id,
                    effective_room_name,
                    signals,
                );
            }
        }
        Value::Array(items) => {
            for item in items {
                scan_json_value(
                    item,
                    is_confer_event,
                    inherited_room_id,
                    inherited_room_name,
                    signals,
                );
            }
        }
        Value::String(text) => {
            let trimmed = text.trim();
            if (trimmed.starts_with('{') || trimmed.starts_with('['))
                && let Ok(nested) = serde_json::from_str::<Value>(trimmed)
            {
                scan_json_value(
                    &nested,
                    is_confer_event,
                    inherited_room_id,
                    inherited_room_name,
                    signals,
                );
                return;
            }
            scan_text_signals(
                text,
                is_confer_event,
                inherited_room_id,
                inherited_room_name,
                signals,
            );
        }
        _ => {}
    }
}

fn scan_text_signals(
    text: &str,
    is_confer_event: bool,
    inherited_room_id: Option<&str>,
    inherited_room_name: Option<&str>,
    signals: &mut ConferEventSignals,
) {
    let unescaped = unescape_json_text(text);
    for candidate in [text, unescaped.as_str()] {
        let trimmed = candidate.trim();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && let Ok(nested) = serde_json::from_str::<Value>(trimmed)
        {
            scan_json_value(
                &nested,
                is_confer_event,
                inherited_room_id,
                inherited_room_name,
                signals,
            );
        }
        for rid in extract_room_uuids_from_text(candidate, is_confer_event) {
            push_unique_str(&mut signals.room_ids, &rid);
        }
        for (source, native_id) in parse_resume_commands_in_text(candidate) {
            push_binding(
                &mut signals.bindings,
                ConferSeatBinding {
                    source,
                    native_session_id: native_id,
                    room_id: inherited_room_id.map(str::to_string),
                    room_name: inherited_room_name.map(str::to_string),
                    seat_name: None,
                    from_event: true,
                },
            );
        }
    }
}

fn unescape_json_text(input: &str) -> String {
    input
        .replace("\\\"", "\"")
        .replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\t", "\t")
        .replace("\\\\", "\\")
}

fn extract_room_uuids_from_text(text: &str, is_confer_event: bool) -> Vec<String> {
    let mut out = Vec::new();
    let lower = text.to_ascii_lowercase();
    for key in ["room_id", "roomid"] {
        let mut search_from = 0usize;
        while let Some(rel) = lower[search_from..].find(key) {
            let after_key = search_from + rel + key.len();
            let window_end = (after_key + 64).min(text.len());
            let window = &text[after_key..window_end];
            if let Some(uuid) = find_first_uuid_36(window) {
                push_unique_str(&mut out, &uuid);
            }
            search_from = after_key;
        }
    }
    if is_confer_event && (lower.contains("mcp__confer__") || lower.contains("confer")) {
        let bytes = text.as_bytes();
        let mut i = 0usize;
        while i + 36 <= bytes.len() {
            if (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && let Ok(slice) = std::str::from_utf8(&bytes[i..i + 36])
                && is_uuid_36(slice)
                && (i + 36 == bytes.len() || !bytes[i + 36].is_ascii_alphanumeric())
            {
                push_unique_str(&mut out, slice);
                i += 36;
                continue;
            }
            i += 1;
        }
    }
    out
}

fn find_first_uuid_36(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i + 36 <= bytes.len() {
        if (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && let Ok(slice) = std::str::from_utf8(&bytes[i..i + 36])
            && is_uuid_36(slice)
            && (i + 36 == bytes.len() || !bytes[i + 36].is_ascii_alphanumeric())
        {
            return Some(slice.to_string());
        }
        i += 1;
    }
    None
}

fn is_uuid_36(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let bytes = s.as_bytes();
    for (idx, b) in bytes.iter().enumerate() {
        if matches!(idx, 8 | 13 | 18 | 23) {
            if *b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn push_unique_str(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|item| item == value) {
        list.push(value.to_string());
    }
}

fn push_binding(list: &mut Vec<ConferSeatBinding>, candidate: ConferSeatBinding) {
    if let Some(existing) = list.iter_mut().find(|item| {
        item.source == candidate.source && item.native_session_id == candidate.native_session_id
    }) {
        if existing.room_id.is_none() {
            existing.room_id = candidate.room_id;
        }
        if existing.room_name.is_none() {
            existing.room_name = candidate.room_name;
        }
        if existing.seat_name.is_none() {
            existing.seat_name = candidate.seat_name;
        }
        existing.from_event |= candidate.from_event;
    } else {
        list.push(candidate);
    }
}
