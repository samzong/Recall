use crate::db::store::Store;
use crate::tui::layout::MessagePane;
use crate::tui::text_layout::wrap_visual_rows;
use crate::types::{
    Message, ParentRelation, Role, Session, SessionEventRecord, SessionUsageEventRecord, ThreadRole,
};
use crate::usage::TokenTotals;

#[derive(Debug, Clone)]
pub(crate) struct ViewingFrame {
    pub(crate) session: Session,
    pub(crate) selected_msg: usize,
    pub(crate) scroll_offset: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ViewingParent {
    pub(crate) relation: ParentRelation,
    pub(crate) source: String,
    pub(crate) source_id: String,
    pub(crate) indexed: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ViewingLineage {
    pub(crate) role: Option<ThreadRole>,
    pub(crate) parents: Vec<ViewingParent>,
}

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

#[derive(Default)]
pub(crate) struct ViewingState {
    pub(crate) messages: Vec<Message>,
    pub(crate) events: Vec<SessionEventRecord>,
    pub(crate) selected_msg: usize,
    pub(crate) scroll_offset: usize,
    pub(crate) summary: Option<ViewingSessionSummary>,
    pub(crate) lineage: Option<ViewingLineage>,
    pub(crate) session: Option<Session>,
    pub(crate) children: Vec<Session>,
    pub(crate) stack: Vec<ViewingFrame>,
    pub(crate) child_selected: usize,
    pub(crate) search_query: String,
    pub(crate) search_input: Option<String>,
    pub(crate) search_cursor: usize,
    pub(crate) search_status: Option<String>,
    pub(crate) lines: Vec<Vec<SanitizedLine>>,
    pub(crate) matches: Vec<usize>,
    pub(crate) local_preview: Option<Result<tempfile::NamedTempFile, String>>,
}

impl ViewingState {
    pub(crate) fn anchor(&mut self, area: ratatui::layout::Rect) {
        self.scroll_offset = self.pane(area.width as usize).scroll_start(
            self.scroll_offset,
            self.selected_msg,
            area.height as usize,
        );
    }

    pub(crate) fn open_child(&mut self, store: &Store, area: ratatui::layout::Rect) -> bool {
        let Some(child) = self.children.get(self.child_selected).cloned() else {
            return false;
        };
        if let Some(session) = self.session.clone() {
            self.stack.push(ViewingFrame {
                session,
                selected_msg: self.selected_msg,
                scroll_offset: self.scroll_offset,
            });
        }
        if self.load(child, store) {
            if let Some(&first) = self.matches.first() {
                self.selected_msg = first;
            }
            self.anchor(area);
        }
        true
    }

    pub(crate) fn back(&mut self, store: &Store, area: ratatui::layout::Rect) -> bool {
        let Some(frame) = self.stack.pop() else {
            return false;
        };
        if self.load(frame.session, store) {
            self.selected_msg = frame.selected_msg.min(self.messages.len().saturating_sub(1));
            self.scroll_offset = frame.scroll_offset;
            self.anchor(area);
        }
        true
    }

    pub(crate) fn pane(&self, inner_width: usize) -> MessagePane {
        let mut rows = Vec::with_capacity(self.messages.len());
        let mut focus = Vec::with_capacity(self.messages.len());
        for index in 0..self.messages.len() {
            let lines = self.lines.get(index);
            let body: usize = lines
                .map(|lines| {
                    lines.iter().map(|line| wrap_visual_rows(&line.text, inner_width).len()).sum()
                })
                .unwrap_or(0);
            rows.push(body + 2);
            focus.push(1 + usize::from(lines.is_some_and(|lines| !lines.is_empty())));
        }
        MessagePane::new(rows, focus)
    }
    pub(crate) fn search_terms(&self) -> Vec<String> {
        self.search_query.split_whitespace().map(str::to_lowercase).collect()
    }
    pub(crate) fn recompute_matches(&mut self) {
        self.matches.clear();
        let terms = self.search_terms();
        if terms.is_empty() {
            return;
        }
        for (i, msg_lines) in self.lines.iter().enumerate() {
            if msg_lines.iter().any(|l| terms.iter().any(|t| l.lower.contains(t.as_str()))) {
                self.matches.push(i);
            }
        }
    }
    pub(crate) fn jump_match(&mut self, forward: bool) {
        if self.search_query.is_empty() || self.matches.is_empty() {
            if !self.search_query.is_empty() {
                self.search_status = Some("No match".to_string());
            }
            return;
        }
        let current = self.selected_msg;
        let next = if forward {
            self.matches
                .iter()
                .find(|&&i| i > current)
                .copied()
                .or_else(|| self.matches.first().copied())
        } else {
            self.matches
                .iter()
                .rev()
                .find(|&&i| i < current)
                .copied()
                .or_else(|| self.matches.last().copied())
        };
        if let Some(idx) = next {
            self.selected_msg = idx;
            self.search_status = None;
        }
    }
    pub(crate) fn load(&mut self, session: Session, store: &Store) -> bool {
        let Ok(msgs) = store.get_messages(&session.id) else {
            return false;
        };
        let usage_events = store.list_usage_events_for_session(&session.id).unwrap_or_default();
        let events = store.list_session_events_for_session(&session.id).unwrap_or_default();
        self.summary = Some(ViewingSessionSummary::from_session(
            &msgs,
            session.duration_minutes,
            &usage_events,
        ));
        let topology = store.session_topology(&session.id).unwrap_or_default();
        let parents = topology
            .parents
            .iter()
            .map(|parent| {
                let indexed = store
                    .resolve_parent(&session.id, parent)
                    .map(|found| found.is_some())
                    .unwrap_or(false);
                ViewingParent {
                    relation: parent.relation,
                    source: parent.source.clone(),
                    source_id: parent.source_id.clone(),
                    indexed,
                }
            })
            .collect();
        self.lineage = Some(ViewingLineage { role: topology.thread_role, parents });
        self.children = store.child_subagents(&session.id).unwrap_or_default();
        self.lines = build_viewing_caches(&msgs);
        self.local_preview = Some(
            crate::share::create_session_preview(&session, &msgs, &events, &usage_events)
                .map_err(|error| error.to_string()),
        );
        self.messages = msgs;
        self.events = events;
        self.session = Some(session);
        self.selected_msg = 0;
        self.scroll_offset = 0;
        self.search_input = None;
        self.search_cursor = 0;
        self.search_status = None;
        self.recompute_matches();
        true
    }
    pub(crate) fn clear(&mut self) {
        *self = Self {
            local_preview: self.local_preview.take(),
            child_selected: self.child_selected,
            search_input: self.search_input.take(),
            search_cursor: self.search_cursor,
            ..Self::default()
        };
    }
}
