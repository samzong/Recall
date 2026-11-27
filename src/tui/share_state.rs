use crate::adapters::ResumeCommand;
use crate::session::related::{RelatedRelation, RelatedSessionGraph, format_related_title};
use crate::share::{BundleDiff, MAX_BUNDLE_SESSIONS};
use crate::types::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingCommandAction {
    Resume,
    OpenApp,
    Handoff,
}

pub(crate) enum AppMode {
    Search,
    Usage,
    Viewing,
    SharePicker,
    ShareResult,
    ExportInput,
    Settings,
    Filters,
    HandoffTarget,
    Subagents,
    ConfirmResume,
}

#[derive(Clone, Copy)]
pub(crate) enum ResumeOrigin {
    Search,
    Viewing,
}

pub(crate) struct PendingResume {
    pub(crate) native_session_id: Option<String>,
    pub(crate) command: ResumeCommand,
    pub(crate) action: PendingCommandAction,
    pub(crate) source_label: String,
    pub(crate) session_title: String,
    pub(crate) cwd: Option<String>,
    pub(crate) origin: ResumeOrigin,
}

pub(crate) struct SharePopup {
    pub(crate) url: Option<String>,
    pub(crate) message: String,
    pub(crate) is_error: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SharePickerItem {
    pub(crate) session: Session,
    pub(crate) parent_id: Option<String>,
    pub(crate) depth: usize,
    pub(crate) relation: Option<RelatedRelation>,
    pub(crate) via: Option<String>,
    pub(crate) role_label: Option<String>,
    pub(crate) display_title: String,
    pub(crate) spawn_anchor: Option<u32>,
    pub(crate) checked: bool,
    pub(crate) is_root: bool,
}

pub(crate) struct SharePickerState {
    pub(crate) items: Vec<SharePickerItem>,
    pub(crate) selected: usize,
    pub(crate) local_preview: Option<tempfile::NamedTempFile>,
}

impl SharePickerState {
    pub(crate) fn from_graph(graph: RelatedSessionGraph) -> Self {
        let root_title = format_related_title(&graph.root, None, None);
        let mut items = Vec::with_capacity(graph.len());
        items.push(SharePickerItem {
            session: graph.root,
            parent_id: None,
            depth: 0,
            relation: None,
            via: None,
            role_label: None,
            display_title: root_title,
            spawn_anchor: None,
            checked: true,
            is_root: true,
        });
        let mut checked_count = 1usize;
        for node in graph.related {
            let can_check = node.session.message_count > 0 && checked_count < MAX_BUNDLE_SESSIONS;
            if can_check {
                checked_count += 1;
            }
            items.push(SharePickerItem {
                session: node.session,
                parent_id: Some(node.parent_id),
                depth: node.depth,
                relation: Some(node.relation),
                via: node.via,
                role_label: node.role_label,
                display_title: node.display_title,
                spawn_anchor: node.spawn_anchor,
                checked: can_check,
                is_root: false,
            });
        }
        Self { items, selected: 0, local_preview: None }
    }

    pub(crate) fn checked_count(&self) -> usize {
        self.items.iter().filter(|item| item.checked).count()
    }

    pub(crate) fn toggle_selected(&mut self) -> Result<(), &'static str> {
        let checked_count = self.checked_count();
        let Some(item) = self.items.get_mut(self.selected) else {
            return Ok(());
        };
        if item.is_root {
            item.checked = true;
            return Ok(());
        }
        if item.checked {
            item.checked = false;
            Ok(())
        } else if checked_count >= MAX_BUNDLE_SESSIONS {
            Err("Maximum 64 sessions per bundle")
        } else {
            item.checked = true;
            Ok(())
        }
    }

    pub(crate) fn toggle_all_non_empty(&mut self) {
        let any_non_empty_checked = self
            .items
            .iter()
            .any(|item| !item.is_root && item.checked && item.session.message_count > 0);
        if any_non_empty_checked {
            for item in &mut self.items {
                if !item.is_root {
                    item.checked = false;
                }
            }
            return;
        }
        let mut checked_count =
            self.items.iter().filter(|item| item.is_root && item.checked).count();
        for item in &mut self.items {
            if item.is_root {
                item.checked = true;
                continue;
            }
            if item.session.message_count > 0 && checked_count < MAX_BUNDLE_SESSIONS {
                item.checked = true;
                checked_count += 1;
            } else {
                item.checked = false;
            }
        }
    }
}

pub(crate) struct SharePublishOutcome {
    pub(crate) url: String,
    pub(crate) session_count: usize,
    pub(crate) bundle_diff: Option<BundleDiff>,
}

pub(crate) struct SharePublishResponse {
    pub(crate) request_id: u64,
    pub(crate) result: Result<SharePublishOutcome, String>,
}
