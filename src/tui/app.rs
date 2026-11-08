use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};

use crate::adapters::ResumeCommand;
use crate::config::AppConfig;
use crate::db::search::{RepoFilter, SearchFilters, TimeRange};
use crate::db::store::{ProjectDirectory, Store};
use crate::handoff;
use crate::project_scope::ProjectScope;
use crate::session_action;
use crate::transcript;
use crate::tui::event::{cursor_next, cursor_prev, edit_text};
use crate::tui::layout::{
    MessagePane, SearchLayout, ViewingLayout, search_layout, vertical_scrollbar_position,
    vertical_scrollbar_row_position, viewing_layout,
};
use crate::tui::search_state::{
    FilterFocus, FilterState, FilterValues, PanelFocus, ProjectPickerRow, SearchMouseTarget,
    SortOrder, SourcePickerRow,
};
use crate::tui::search_worker::{SearchPhase, SearchRequest, SearchResponse, SearchWorker};
use crate::tui::share_state::{
    AppMode, PendingCommandAction, PendingResume, ResumeOrigin, SharePopup,
};
use crate::tui::sync_worker::SyncRequest;
use crate::tui::text_layout::wrap_visual_rows;
use crate::tui::usage_state::{UsageState, UsageTab};
use crate::tui::usage_worker::{UsageRequest, UsageResponse};
use crate::tui::viewing_state::ViewingState;
use crate::types::{MatchSource, Message, SearchResult, SemanticProgress};

const SEARCH_DEBOUNCE_MS: u64 = 250;

#[derive(Clone, Copy)]
enum MouseDragTarget {
    SearchList,
    SearchPreview,
    Viewing,
}

pub(crate) struct App {
    pub(crate) filters: FilterState,
    pub(crate) viewing: ViewingState,
    terminal_area: Rect,
    pub(crate) mode: AppMode,
    pub(crate) panel_focus: PanelFocus,
    pub(crate) query: String,
    pub(crate) cursor_pos: usize,
    pub(crate) results: Vec<SearchResult>,
    pub(crate) selected_index: usize,
    pub(crate) result_scroll_offset: usize,
    pub(crate) preview_messages: Vec<Message>,
    pub(crate) preview_selected_msg: usize,
    pub(crate) preview_scroll_offset: usize,
    mouse_drag_target: Option<MouseDragTarget>,
    pub(crate) all_sources: Vec<(String, String)>,
    pub(crate) config: AppConfig,
    pub(crate) should_quit: bool,
    pub(crate) last_keystroke: Instant,
    pub(crate) search_pending: bool,
    pub(crate) search_request_id: u64,
    pub(crate) active_search_id: u64,
    pub(crate) search_in_flight: bool,
    pub(crate) search_feedback: Option<String>,
    pub(crate) sync_requested: bool,
    pub(crate) sync_in_flight: bool,
    pub(crate) embedding_unavailable: bool,
    pub(crate) status_message: Option<String>,
    pub(crate) export_path: String,
    pub(crate) export_cursor: usize,
    pub(crate) total_sessions: u64,
    pub(crate) total_messages: u64,
    pub(crate) semantic_progress: SemanticProgress,
    pub(crate) semantic_last_refresh: Instant,
    pub(crate) settings_selected: usize,
    pub(crate) pending_resume: Option<PendingResume>,
    pub(crate) handoff_targets: Vec<handoff::HandoffTarget>,
    pub(crate) handoff_target_selected: usize,
    pub(crate) share_popup: Option<SharePopup>,
    pub(crate) share_publish_rx: Option<mpsc::Receiver<Result<String, String>>>,
    pub(crate) exec_on_exit: Option<(ResumeCommand, Option<String>)>,
    pub(crate) project_directories: Vec<ProjectDirectory>,
    pub(crate) usage: UsageState,
}

mod actions;
mod background;
mod export;
mod filter_input;
mod filters;
mod input;
mod mouse;
mod scroll;
mod search;
mod settings;
mod viewing;

impl App {
    pub(crate) fn new(
        store: &Store,
        all_sources: Vec<(String, String)>,
        config: AppConfig,
    ) -> Self {
        let (total_sessions, total_messages) = store.stats().unwrap_or((0, 0));
        let semantic_progress = store.semantic_progress().unwrap_or_default();
        let scope = crate::project_scope::auto_scope();

        let mut app = Self::empty(all_sources, config, scope);
        app.total_sessions = total_sessions;
        app.total_messages = total_messages;
        app.semantic_progress = semantic_progress;
        app.project_directories = store.list_project_directories().unwrap_or_default();
        app.reset_search_defaults();
        app.update_scope_metrics(store);
        app.load_recent(store);
        app
    }

    fn empty(all_sources: Vec<(String, String)>, config: AppConfig, scope: ProjectScope) -> Self {
        Self {
            filters: FilterState::new(scope),
            viewing: ViewingState::default(),
            terminal_area: Rect::new(0, 0, 80, 24),
            mode: AppMode::Search,
            panel_focus: PanelFocus::SessionList,
            query: String::new(),
            cursor_pos: 0,
            results: Vec::new(),
            selected_index: 0,
            result_scroll_offset: 0,
            preview_messages: Vec::new(),
            preview_selected_msg: 0,
            preview_scroll_offset: 0,
            mouse_drag_target: None,
            all_sources,
            config,
            should_quit: false,
            last_keystroke: Instant::now(),
            search_pending: false,
            search_request_id: 0,
            active_search_id: 0,
            search_in_flight: false,
            search_feedback: None,
            sync_requested: false,
            sync_in_flight: false,
            embedding_unavailable: false,
            status_message: None,
            export_path: String::new(),
            export_cursor: 0,
            total_sessions: 0,
            total_messages: 0,
            semantic_progress: SemanticProgress::default(),
            semantic_last_refresh: Instant::now(),
            settings_selected: 0,
            pending_resume: None,
            handoff_targets: Vec::new(),
            handoff_target_selected: 0,
            share_popup: None,
            share_publish_rx: None,
            exec_on_exit: None,
            project_directories: Vec::new(),
            usage: UsageState::default(),
        }
    }
}

fn short_project_label(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    match parts.len() {
        0 => path.to_string(),
        1 => parts[0].to_string(),
        len => format!("{}/{}", parts[len - 2], parts[len - 1]),
    }
}

fn project_matches_query(path: &str, query: &str) -> bool {
    let path = path.to_lowercase();
    query.split_whitespace().all(|part| path.contains(part))
}

fn scope_label(scope: &ProjectScope) -> String {
    match scope {
        ProjectScope::Global => "All projects".to_string(),
        ProjectScope::Directory(directory) => short_project_label(directory),
        ProjectScope::Repository { filter, .. } => repo_filter_label(filter),
    }
}

fn repo_filter_label(repo: &RepoFilter) -> String {
    match repo {
        RepoFilter::Remote(remote) => short_project_label(remote),
        RepoFilter::Slug(slug) => slug.clone(),
        RepoFilter::Name(name) => name.clone(),
    }
}

#[cfg(test)]
mod tests;
