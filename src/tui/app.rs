use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};

use crate::adapters::ResumeCommand;
use crate::config::AppConfig;
use crate::db::search::{RepoFilter, SearchFilters, TimeRange};
use crate::db::store::{ProjectDirectory, Store};
use crate::handoff;
use crate::repo_identity::RepoIdentityCache;
use crate::session_action;
use crate::skill_audit::{self, SkillAuditFilters, SkillAuditReport};
use crate::transcript;
use crate::tui::layout::{
    MessagePane, SearchLayout, ViewingLayout, search_layout, vertical_scrollbar_position,
    viewing_layout,
};
use crate::tui::search_state::{
    FilterFocus, PanelFocus, ProjectPickerRow, SearchMouseTarget, SortOrder, SourcePickerRow,
};
use crate::tui::search_worker::{SearchPhase, SearchRequest, SearchResponse, SearchWorker};
use crate::tui::share_state::{
    AppMode, PendingCommandAction, PendingResume, ResumeOrigin, SharePopup,
};
use crate::tui::text_layout::wrap_visual_rows;
use crate::tui::usage_state::UsageTab;
use crate::tui::viewing_state::{SanitizedLine, ViewingSessionSummary, build_viewing_caches};
use crate::types::{BackgroundJobStatus, MatchSource, Message, SearchResult, SemanticProgress};
use crate::usage::{self, UsageFilters, UsageReport};

const USAGE_LOADING_MIN_MS: u128 = 75;
const SEARCH_DEBOUNCE_MS: u64 = 250;

pub(crate) struct App {
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
    pub(crate) viewing_messages: Vec<Message>,
    pub(crate) viewing_selected_msg: usize,
    pub(crate) viewing_scroll_offset: usize,
    pub(crate) viewing_session_summary: Option<ViewingSessionSummary>,
    pub(crate) all_sources: Vec<(String, String)>,
    pub(crate) config: AppConfig,
    pub(crate) source_filter_selection: Vec<String>,
    pub(crate) time_filter: TimeRange,
    pub(crate) filter_focus: FilterFocus,
    pub(crate) filters_dirty: bool,
    pub(crate) draft_source_filter_selection: Vec<String>,
    pub(crate) draft_project_filter: Option<String>,
    pub(crate) draft_repo_filter: Option<RepoFilter>,
    pub(crate) draft_time_filter: TimeRange,
    pub(crate) draft_sort_order: SortOrder,
    pub(crate) should_quit: bool,
    pub(crate) last_keystroke: Instant,
    pub(crate) search_pending: bool,
    pub(crate) search_request_id: u64,
    pub(crate) active_search_id: u64,
    pub(crate) search_in_flight: bool,
    pub(crate) search_feedback: Option<String>,
    pub(crate) embedding_unavailable: bool,
    pub(crate) status_message: Option<String>,
    pub(crate) sort_order: SortOrder,
    pub(crate) export_path: String,
    pub(crate) export_cursor: usize,
    pub(crate) total_sessions: u64,
    pub(crate) total_messages: u64,
    pub(crate) semantic_progress: SemanticProgress,
    pub(crate) background_status: BackgroundJobStatus,
    pub(crate) semantic_last_refresh: Instant,
    pub(crate) settings_selected: usize,
    pub(crate) pending_resume: Option<PendingResume>,
    pub(crate) handoff_target_selected: usize,
    pub(crate) share_popup: Option<SharePopup>,
    pub(crate) share_publish_rx: Option<mpsc::Receiver<Result<String, String>>>,
    pub(crate) exec_on_exit: Option<(ResumeCommand, Option<String>)>,
    pub(crate) viewing_search_query: String,
    pub(crate) viewing_search_input: Option<String>,
    pub(crate) viewing_search_input_cursor: usize,
    pub(crate) viewing_search_status: Option<String>,
    pub(crate) viewing_sanitized_lines: Vec<Vec<SanitizedLine>>,
    pub(crate) viewing_match_cache: Vec<usize>,
    pub(crate) source_picker_query: String,
    pub(crate) source_picker_cursor: usize,
    pub(crate) source_picker_selected: usize,
    pub(crate) source_picker_selection: Vec<String>,
    pub(crate) source_picker_dirty: bool,
    pub(crate) source_picker_typing: bool,
    pub(crate) filters_editing_source: bool,
    pub(crate) project_directories: Vec<ProjectDirectory>,
    pub(crate) project_filter: Option<String>,
    pub(crate) repo_filter: Option<RepoFilter>,
    pub(crate) project_picker_query: String,
    pub(crate) project_picker_cursor: usize,
    pub(crate) project_picker_selected: usize,
    pub(crate) project_picker_selection: Option<String>,
    pub(crate) project_picker_dirty: bool,
    pub(crate) project_picker_typing: bool,
    pub(crate) filters_editing_project: bool,
    pub(crate) usage_report: Option<UsageReport>,
    pub(crate) usage_year_report: Option<UsageReport>,
    pub(crate) usage_error: Option<String>,
    pub(crate) usage_time_filter: TimeRange,
    pub(crate) usage_refresh_requested_at: Option<Instant>,
    pub(crate) usage_breakdown_scroll: u16,
    pub(crate) usage_tab: UsageTab,
    pub(crate) skill_audit_report: Option<SkillAuditReport>,
    pub(crate) skill_audit_error: Option<String>,
    pub(crate) skill_audit_selected: usize,
}

impl App {
    pub(crate) fn new(
        store: &Store,
        all_sources: Vec<(String, String)>,
        mut config: AppConfig,
    ) -> Self {
        config.normalize_sources(&all_sources);

        let (total_sessions, total_messages) = store.stats().unwrap_or((0, 0));
        let semantic_progress = store.semantic_progress().unwrap_or_default();
        let background_status = store.background_job_status("pipeline").unwrap_or_default();
        let repo_filter = default_repo_filter(&config);

        let mut app = Self {
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
            viewing_messages: Vec::new(),
            viewing_selected_msg: 0,
            viewing_scroll_offset: 0,
            viewing_session_summary: None,
            all_sources,
            config,
            source_filter_selection: Vec::new(),
            time_filter: TimeRange::All,
            filter_focus: FilterFocus::Source,
            filters_dirty: false,
            draft_source_filter_selection: Vec::new(),
            draft_project_filter: None,
            draft_repo_filter: None,
            draft_time_filter: TimeRange::All,
            draft_sort_order: SortOrder::Newest,
            should_quit: false,
            last_keystroke: Instant::now(),
            search_pending: false,
            search_request_id: 0,
            active_search_id: 0,
            search_in_flight: false,
            search_feedback: None,
            embedding_unavailable: false,
            status_message: None,
            sort_order: SortOrder::Newest,
            export_path: String::new(),
            export_cursor: 0,
            total_sessions,
            total_messages,
            semantic_progress,
            background_status,
            semantic_last_refresh: Instant::now(),
            settings_selected: 0,
            pending_resume: None,
            handoff_target_selected: 0,
            share_popup: None,
            share_publish_rx: None,
            exec_on_exit: None,
            viewing_search_query: String::new(),
            viewing_search_input: None,
            viewing_search_input_cursor: 0,
            viewing_search_status: None,
            viewing_sanitized_lines: Vec::new(),
            viewing_match_cache: Vec::new(),
            source_picker_query: String::new(),
            source_picker_cursor: 0,
            source_picker_selected: 0,
            source_picker_selection: Vec::new(),
            source_picker_dirty: false,
            source_picker_typing: false,
            filters_editing_source: false,
            project_directories: store.list_project_directories().unwrap_or_default(),
            project_filter: None,
            repo_filter,
            project_picker_query: String::new(),
            project_picker_cursor: 0,
            project_picker_selected: 0,
            project_picker_selection: None,
            project_picker_dirty: false,
            project_picker_typing: false,
            filters_editing_project: false,
            usage_report: None,
            usage_year_report: None,
            usage_error: None,
            usage_time_filter: TimeRange::All,
            usage_refresh_requested_at: None,
            usage_breakdown_scroll: 0,
            usage_tab: UsageTab::Tokens,
            skill_audit_report: None,
            skill_audit_error: None,
            skill_audit_selected: 0,
        };
        app.reset_search_defaults();
        app.update_scope_metrics(store);
        app.load_recent(store);
        app
    }

    pub(crate) fn source_filter_ids(&self) -> Option<Vec<String>> {
        let explicit = self.normalized_source_selection(&self.source_filter_selection);
        if !explicit.is_empty() {
            return Some(explicit);
        }

        let enabled = self.enabled_sources();
        if enabled.is_empty() {
            return None;
        }

        if enabled.len() == self.all_sources.len() {
            None
        } else {
            Some(enabled.into_iter().map(|(id, _)| id.clone()).collect())
        }
    }

    pub(crate) fn source_filter_label(&self) -> String {
        self.source_filter_label_for_selection(&self.source_filter_selection)
    }

    pub(crate) fn draft_source_filter_label(&self) -> String {
        self.source_filter_label_for_selection(&self.draft_source_filter_selection)
    }

    fn source_filter_label_for_selection(&self, selection: &[String]) -> String {
        let explicit = self.normalized_source_selection(selection);
        if explicit.is_empty() {
            return if self.enabled_sources().len() == self.all_sources.len() {
                "ALL".to_string()
            } else {
                "DEFAULT".to_string()
            };
        }

        if explicit.len() == 1 {
            return self.source_label_for(&explicit[0]).to_string();
        }

        let labels: Vec<&str> =
            explicit.iter().map(|id| self.source_label_for(id)).take(2).collect();
        if explicit.len() == 2 {
            labels.join(", ")
        } else {
            format!("{}, +{}", labels.join(", "), explicit.len() - labels.len())
        }
    }

    pub(crate) fn time_filter_label(&self) -> &'static str {
        Self::time_range_label(self.time_filter)
    }

    pub(crate) fn draft_time_filter_label(&self) -> &'static str {
        Self::time_range_label(self.draft_time_filter)
    }

    fn time_range_label(time_range: TimeRange) -> &'static str {
        match time_range {
            TimeRange::Today => "Today",
            TimeRange::Week => "7d",
            TimeRange::Month => "30d",
            TimeRange::All => "All",
        }
    }

    pub(crate) fn usage_time_label(&self) -> &'static str {
        match self.usage_time_filter {
            TimeRange::Today => "Today",
            TimeRange::Week => "7d",
            TimeRange::Month => "30d",
            TimeRange::All => "All day",
        }
    }

    pub(crate) fn sort_label(&self) -> &'static str {
        Self::sort_order_label(self.sort_order)
    }

    pub(crate) fn draft_sort_label(&self) -> &'static str {
        Self::sort_order_label(self.draft_sort_order)
    }

    fn sort_order_label(sort_order: SortOrder) -> &'static str {
        match sort_order {
            SortOrder::Relevance => "Relevance",
            SortOrder::Newest => "Newest",
        }
    }

    pub(crate) fn project_filter_label(&self) -> String {
        self.project_filter
            .as_deref()
            .map(short_project_label)
            .or_else(|| self.repo_filter.as_ref().map(repo_filter_label))
            .unwrap_or_else(|| "All projects".to_string())
    }

    pub(crate) fn draft_project_filter_label(&self) -> String {
        self.draft_project_filter
            .as_deref()
            .map(short_project_label)
            .or_else(|| self.draft_repo_filter.as_ref().map(repo_filter_label))
            .unwrap_or_else(|| "All projects".to_string())
    }

    pub(crate) fn source_label_for<'a>(&'a self, source_id: &'a str) -> &'a str {
        self.all_sources
            .iter()
            .find(|(id, _)| id == source_id)
            .map(|(_, label)| label.as_str())
            .unwrap_or(source_id)
    }

    pub(crate) fn load_recent(&mut self, store: &Store) {
        let source_ids = self.source_filter_ids();
        let recent = store
            .list_recent_sessions_for_search_scope(
                source_ids.as_deref(),
                self.time_filter,
                self.project_filter.as_deref(),
                self.repo_filter.as_ref(),
                200,
            )
            .unwrap_or_default();
        self.results = recent
            .into_iter()
            .map(|session| SearchResult { session, match_source: MatchSource::Fts, snippet: None })
            .collect();
        self.selected_index = 0;
        self.result_scroll_offset = 0;
        self.panel_focus = PanelFocus::SessionList;
        self.search_pending = false;
        self.search_in_flight = false;
        self.load_preview(store);
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent, store: &Store) {
        self.status_message = None;

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match self.mode {
            AppMode::Search => self.handle_search_key(key, store),
            AppMode::Usage => self.handle_usage_key(key, store),
            AppMode::Viewing => {
                let before = self.viewing_selected_msg;
                self.handle_viewing_key(key);
                if matches!(self.mode, AppMode::Viewing) && self.viewing_selected_msg != before {
                    self.anchor_viewing_scroll();
                }
            }
            AppMode::ShareResult => self.handle_share_result_key(key),
            AppMode::ExportInput => self.handle_export_key(key),
            AppMode::Settings => self.handle_settings_key(key, store),
            AppMode::Filters => self.handle_filters_key(key, store),
            AppMode::HandoffTarget => self.handle_handoff_target_key(key),
            AppMode::ConfirmResume => self.handle_confirm_resume_key(key),
        }
    }

    pub(crate) fn set_terminal_size(&mut self, width: u16, height: u16) {
        self.terminal_area = Rect::new(0, 0, width, height);
    }

    pub(crate) fn poll_share_publish(&mut self) {
        let Some(rx) = self.share_publish_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.share_popup = Some(match result {
                    Ok(url) => SharePopup {
                        url: Some(url),
                        message: "Session shared".to_string(),
                        is_error: false,
                    },
                    Err(message) => SharePopup { url: None, message, is_error: true },
                });
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.share_publish_rx = Some(rx);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.share_popup = Some(SharePopup {
                    url: None,
                    message: "Share worker stopped before publishing finished".to_string(),
                    is_error: true,
                });
            }
        }
    }

    pub(crate) fn handle_scroll_up(&mut self, store: &Store) {
        match self.mode {
            AppMode::Search => match self.panel_focus {
                PanelFocus::SessionList => self.scroll_result_list_up(store),
                PanelFocus::Preview => self.move_preview_selection(true),
            },
            AppMode::Viewing if self.viewing_selected_msg > 0 => {
                self.viewing_selected_msg -= 1;
                self.anchor_viewing_scroll();
            }
            AppMode::Settings if self.settings_selected > 0 => {
                self.settings_selected -= 1;
            }
            AppMode::Filters if self.filters_editing_source && self.source_picker_selected > 0 => {
                self.source_picker_selected -= 1;
            }
            AppMode::Filters
                if self.filters_editing_project && self.project_picker_selected > 0 =>
            {
                self.project_picker_selected -= 1;
            }
            AppMode::Filters if !self.filters_editing_source && !self.filters_editing_project => {
                self.filter_focus = self.filter_focus.previous();
            }
            AppMode::Usage
                if self.usage_tab == UsageTab::Tokens && self.usage_breakdown_scroll > 0 =>
            {
                self.usage_breakdown_scroll -= 1;
            }
            AppMode::Usage
                if self.usage_tab == UsageTab::Skills && self.skill_audit_selected > 0 =>
            {
                self.skill_audit_selected -= 1;
            }
            _ => {}
        }
    }

    pub(crate) fn handle_scroll_down(&mut self, store: &Store) {
        match self.mode {
            AppMode::Search => match self.panel_focus {
                PanelFocus::SessionList => self.scroll_result_list_down(store),
                PanelFocus::Preview => self.move_preview_selection(false),
            },
            AppMode::Viewing if self.viewing_selected_msg + 1 < self.viewing_messages.len() => {
                self.viewing_selected_msg += 1;
                self.anchor_viewing_scroll();
            }
            AppMode::Settings if self.settings_selected + 1 < self.settings_row_count() => {
                self.settings_selected += 1;
            }
            AppMode::Filters
                if self.filters_editing_source
                    && self.source_picker_selected + 1 < self.source_picker_rows().len() =>
            {
                self.source_picker_selected += 1;
            }
            AppMode::Filters
                if self.filters_editing_project
                    && self.project_picker_selected + 1 < self.project_picker_rows().len() =>
            {
                self.project_picker_selected += 1;
            }
            AppMode::Filters if !self.filters_editing_source && !self.filters_editing_project => {
                self.filter_focus = self.filter_focus.next();
            }
            AppMode::Usage if self.usage_tab == UsageTab::Tokens => {
                self.usage_breakdown_scroll = self.usage_breakdown_scroll.saturating_add(1);
            }
            AppMode::Usage
                if self.usage_tab == UsageTab::Skills
                    && self.skill_audit_selected + 1 < self.skill_audit_entry_count() =>
            {
                self.skill_audit_selected += 1;
            }
            _ => {}
        }
    }

    fn scroll_result_list_up(&mut self, store: &Store) {
        if self.results.is_empty() || self.selected_index == 0 {
            return;
        }

        let visible_rows = search_layout(self.terminal_area).list_inner().height as usize;
        let start = self.result_list_start(visible_rows);
        self.selected_index -= 1;
        if visible_rows > 0 && self.selected_index < start {
            self.result_scroll_offset = self.selected_index;
        } else {
            self.result_scroll_offset = start;
        }
        self.load_preview(store);
    }

    fn scroll_result_list_down(&mut self, store: &Store) {
        if self.selected_index + 1 >= self.results.len() {
            return;
        }

        let visible_rows = search_layout(self.terminal_area).list_inner().height as usize;
        let start = self.result_list_start(visible_rows);
        self.selected_index += 1;
        if visible_rows > 0 && self.selected_index >= start + visible_rows {
            self.result_scroll_offset = self.selected_index + 1 - visible_rows;
        } else {
            self.result_scroll_offset = start;
        }
        self.load_preview(store);
    }

    pub(crate) fn result_list_start(&self, visible_rows: usize) -> usize {
        if visible_rows == 0 || self.results.is_empty() {
            return 0;
        }

        let selected_index = self.selected_index.min(self.results.len().saturating_sub(1));
        let max_start = self.results.len().saturating_sub(visible_rows);
        let start = self.result_scroll_offset.min(max_start);
        if selected_index < start {
            selected_index
        } else if selected_index >= start + visible_rows {
            selected_index + 1 - visible_rows
        } else {
            start
        }
    }

    fn move_preview_selection(&mut self, up: bool) {
        let can_move = if up {
            self.preview_selected_msg > 0
        } else {
            self.preview_selected_msg + 1 < self.preview_messages.len()
        };
        if !can_move {
            return;
        }

        let inner = search_layout(self.terminal_area).preview_inner();
        let pane = self.preview_pane(inner.width as usize);
        let viewport = inner.height as usize;
        self.preview_scroll_offset =
            pane.scroll_start(self.preview_scroll_offset, self.preview_selected_msg, viewport);
        if up {
            self.preview_selected_msg -= 1;
        } else {
            self.preview_selected_msg += 1;
        }
        self.preview_scroll_offset =
            pane.scroll_start(self.preview_scroll_offset, self.preview_selected_msg, viewport);
    }

    fn anchor_viewing_scroll(&mut self) {
        let messages = viewing_layout(self.terminal_area).messages;
        let pane = self.viewing_pane(messages.width as usize);
        self.viewing_scroll_offset = pane.scroll_start(
            self.viewing_scroll_offset,
            self.viewing_selected_msg,
            messages.height as usize,
        );
    }

    pub(crate) fn preview_pane(&self, inner_width: usize) -> MessagePane {
        let mut rows = Vec::with_capacity(self.preview_messages.len());
        let mut focus = Vec::with_capacity(self.preview_messages.len());
        for msg in &self.preview_messages {
            let text: String = msg.content.chars().take(300).collect();
            let body: usize = text
                .lines()
                .take(6)
                .map(|line| {
                    let line = crate::utils::sanitize_line(line);
                    wrap_visual_rows(&format!("  {line}"), inner_width).len()
                })
                .sum();
            rows.push(body + 2);
            focus.push(1 + usize::from(text.lines().next().is_some()));
        }
        MessagePane::new(rows, focus)
    }

    pub(crate) fn viewing_pane(&self, inner_width: usize) -> MessagePane {
        let mut rows = Vec::with_capacity(self.viewing_messages.len());
        let mut focus = Vec::with_capacity(self.viewing_messages.len());
        for index in 0..self.viewing_messages.len() {
            let lines = self.viewing_sanitized_lines.get(index);
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

    fn preview_message_at_row(&self, row: u16, layout: &SearchLayout) -> Option<(usize, usize)> {
        let inner = layout.preview_inner();
        if row < inner.y || row >= inner.bottom() {
            return None;
        }

        let pane = self.preview_pane(inner.width as usize);
        let start = pane.scroll_start(
            self.preview_scroll_offset,
            self.preview_selected_msg,
            inner.height as usize,
        );
        pane.index_at(start + usize::from(row - inner.y)).map(|index| (index, start))
    }

    fn viewing_message_at_row(&self, row: u16, layout: &ViewingLayout) -> Option<(usize, usize)> {
        let messages = layout.messages;
        if row < messages.y || row >= messages.bottom() {
            return None;
        }

        let pane = self.viewing_pane(messages.width as usize);
        let start = pane.scroll_start(
            self.viewing_scroll_offset,
            self.viewing_selected_msg,
            messages.height as usize,
        );
        pane.index_at(start + usize::from(row - messages.y)).map(|index| (index, start))
    }

    pub(crate) fn handle_mouse_down(&mut self, column: u16, row: u16, store: &Store) {
        if matches!(self.mode, AppMode::Viewing) {
            let layout = viewing_layout(self.terminal_area);
            if self.handle_viewing_scrollbar_down(column, row, &layout) {
                return;
            }
            if let Some((index, start)) = self.viewing_message_at_row(row, &layout) {
                self.viewing_selected_msg = index;
                self.viewing_scroll_offset = start;
            }
            return;
        }

        if !matches!(self.mode, AppMode::Search) {
            return;
        }

        let layout = search_layout(self.terminal_area);
        if self.handle_search_scrollbar_down(column, row, &layout, store) {
            return;
        }

        match self.search_mouse_target(column, row, &layout) {
            Some(SearchMouseTarget::SessionList(index)) => {
                self.panel_focus = PanelFocus::SessionList;
                if let Some(index) = index {
                    self.result_scroll_offset =
                        self.result_list_start(layout.list_inner().height as usize);
                    self.selected_index = index;
                    self.load_preview(store);
                }
            }
            Some(SearchMouseTarget::Preview) if !self.preview_messages.is_empty() => {
                self.panel_focus = PanelFocus::Preview;
                if let Some((index, start)) = self.preview_message_at_row(row, &layout) {
                    self.preview_selected_msg = index;
                    self.preview_scroll_offset = start;
                }
            }
            _ => {}
        }
    }

    fn handle_search_scrollbar_down(
        &mut self,
        column: u16,
        row: u16,
        layout: &SearchLayout,
        store: &Store,
    ) -> bool {
        let list_viewport = layout.list_inner().height as usize;
        if let Some(position) =
            vertical_scrollbar_position(column, row, layout.list, self.results.len(), list_viewport)
        {
            self.panel_focus = PanelFocus::SessionList;
            self.result_scroll_offset = position;
            self.selected_index = position.min(self.results.len().saturating_sub(1));
            self.load_preview(store);
            return true;
        }

        if self.preview_messages.is_empty() {
            return false;
        }

        let inner = layout.preview_inner();
        let pane = self.preview_pane(inner.width as usize);
        if let Some(position) = vertical_scrollbar_position(
            column,
            row,
            layout.preview,
            pane.total_rows(),
            inner.height as usize,
        ) {
            self.panel_focus = PanelFocus::Preview;
            self.preview_scroll_offset = position;
            if let Some(index) = pane.index_at(position) {
                self.preview_selected_msg = index;
            }
            return true;
        }

        false
    }

    fn handle_viewing_scrollbar_down(
        &mut self,
        column: u16,
        row: u16,
        layout: &ViewingLayout,
    ) -> bool {
        let messages = layout.messages;
        let pane = self.viewing_pane(messages.width as usize);
        if let Some(position) = vertical_scrollbar_position(
            column,
            row,
            layout.scrollbar_area(),
            pane.total_rows(),
            messages.height as usize,
        ) {
            self.viewing_scroll_offset = position;
            if let Some(index) = pane.index_at(position) {
                self.viewing_selected_msg = index;
            }
            return true;
        }

        false
    }

    pub(crate) fn handle_mouse_scroll_up(&mut self, column: u16, row: u16, store: &Store) {
        self.handle_mouse_scroll(column, row, true, store);
    }

    pub(crate) fn handle_mouse_scroll_down(&mut self, column: u16, row: u16, store: &Store) {
        self.handle_mouse_scroll(column, row, false, store);
    }

    fn handle_mouse_scroll(&mut self, column: u16, row: u16, up: bool, store: &Store) {
        if matches!(self.mode, AppMode::Search) {
            let layout = search_layout(self.terminal_area);
            match self.search_mouse_target(column, row, &layout) {
                Some(SearchMouseTarget::SessionList(_)) => {
                    self.panel_focus = PanelFocus::SessionList;
                    if up {
                        self.scroll_result_list_up(store);
                    } else {
                        self.scroll_result_list_down(store);
                    }
                    return;
                }
                Some(SearchMouseTarget::Preview) if !self.preview_messages.is_empty() => {
                    self.panel_focus = PanelFocus::Preview;
                    self.move_preview_selection(up);
                    return;
                }
                _ => {}
            }
        }

        if up { self.handle_scroll_up(store) } else { self.handle_scroll_down(store) }
    }

    fn search_mouse_target(
        &self,
        column: u16,
        row: u16,
        layout: &SearchLayout,
    ) -> Option<SearchMouseTarget> {
        let pos = Position { x: column, y: row };
        if layout.list.contains(pos) {
            let inner = layout.list_inner();
            if !inner.contains(pos) {
                return Some(SearchMouseTarget::SessionList(None));
            }

            let start = self.result_list_start(inner.height as usize);
            let index = start + usize::from(row - inner.y);
            return Some(SearchMouseTarget::SessionList(
                (index < self.results.len()).then_some(index),
            ));
        }

        if layout.preview.contains(pos) {
            return Some(SearchMouseTarget::Preview);
        }

        None
    }

    fn handle_search_key(&mut self, key: KeyEvent, store: &Store) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            self.open_filters();
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.mode = AppMode::Settings;
            self.settings_selected = 0;
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.start_resume_confirmation(ResumeOrigin::Search);
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.start_app_open_confirmation(ResumeOrigin::Search);
            return;
        }

        match key.code {
            KeyCode::Char('q')
                if self.query.is_empty() && self.panel_focus == PanelFocus::SessionList =>
            {
                self.should_quit = true;
            }
            KeyCode::Esc => {
                if self.panel_focus == PanelFocus::Preview {
                    self.panel_focus = PanelFocus::SessionList;
                } else if !self.query.is_empty() {
                    self.query.clear();
                    self.cursor_pos = 0;
                    self.queue_search();
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char(c) if self.panel_focus == PanelFocus::SessionList => {
                self.query.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.queue_search();
            }
            KeyCode::Backspace
                if self.panel_focus == PanelFocus::SessionList && self.cursor_pos > 0 =>
            {
                let prev = self.query[..self.cursor_pos]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.query.replace_range(prev..self.cursor_pos, "");
                self.cursor_pos = prev;
                self.queue_search();
            }
            KeyCode::Left => {
                if self.panel_focus == PanelFocus::Preview {
                    self.panel_focus = PanelFocus::SessionList;
                } else if self.cursor_pos > 0 {
                    self.cursor_pos = self.query[..self.cursor_pos]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            KeyCode::Right if self.panel_focus == PanelFocus::SessionList => {
                if self.cursor_pos < self.query.len() {
                    self.cursor_pos = self.query[self.cursor_pos..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.cursor_pos + i)
                        .unwrap_or(self.query.len());
                } else if !self.preview_messages.is_empty() {
                    self.panel_focus = PanelFocus::Preview;
                    self.preview_selected_msg = 0;
                    self.preview_scroll_offset = 0;
                }
            }
            KeyCode::Up => {
                self.handle_scroll_up(store);
            }
            KeyCode::Down => {
                self.handle_scroll_down(store);
            }
            KeyCode::Enter if !self.results.is_empty() => {
                self.enter_viewing(store);
            }
            KeyCode::Tab => {
                self.open_filters();
            }
            _ => {}
        }
    }

    fn handle_usage_key(&mut self, key: KeyEvent, store: &Store) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                self.usage_tab = match self.usage_tab {
                    UsageTab::Tokens => UsageTab::Skills,
                    UsageTab::Skills => UsageTab::Tokens,
                };
                self.usage_breakdown_scroll = 0;
                self.skill_audit_selected = 0;
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                self.cycle_usage_time(false);
                self.request_usage_refresh();
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.cycle_usage_source(false);
                self.request_usage_refresh();
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.reset_usage_dashboard();
                self.request_usage_refresh();
            }
            KeyCode::Enter if self.usage_tab == UsageTab::Skills => {
                self.open_skill_sessions(store);
            }
            KeyCode::Up | KeyCode::Char('k') => self.handle_scroll_up(store),
            KeyCode::Down | KeyCode::Char('j') => self.handle_scroll_down(store),
            _ => {}
        }
    }

    fn handle_viewing_key(&mut self, key: KeyEvent) {
        if self.viewing_search_input.is_some() {
            self.handle_viewing_search_input(key);
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.start_resume_confirmation(ResumeOrigin::Viewing);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.start_app_open_confirmation(ResumeOrigin::Viewing);
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Search;
                self.viewing_messages.clear();
                self.viewing_selected_msg = 0;
                self.viewing_scroll_offset = 0;
                self.viewing_session_summary = None;
                self.viewing_search_query.clear();
                self.viewing_search_status = None;
                self.viewing_sanitized_lines.clear();
                self.viewing_match_cache.clear();
                self.share_popup = None;
            }
            KeyCode::Up | KeyCode::Char('k') if self.viewing_selected_msg > 0 => {
                self.viewing_selected_msg -= 1;
            }
            KeyCode::Down | KeyCode::Char('j')
                if self.viewing_selected_msg + 1 < self.viewing_messages.len() =>
            {
                self.viewing_selected_msg += 1;
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.viewing_selected_msg = 0;
            }
            KeyCode::End | KeyCode::Char('G') if !self.viewing_messages.is_empty() => {
                self.viewing_selected_msg = self.viewing_messages.len() - 1;
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.copy_current_message();
            }
            KeyCode::Char('e') => {
                self.start_export();
            }
            KeyCode::Char('s') => {
                self.share_current_session();
            }
            KeyCode::Char('v') => {
                self.preview_current_session();
            }
            KeyCode::Char('h') => {
                self.open_handoff_target_picker();
            }
            KeyCode::Char('/') => {
                self.viewing_search_input = Some(String::new());
                self.viewing_search_input_cursor = 0;
                self.viewing_search_status = None;
            }
            KeyCode::Char('n') => {
                self.jump_viewing_match(true);
            }
            KeyCode::Char('N') => {
                self.jump_viewing_match(false);
            }
            _ => {}
        }
    }

    fn handle_share_result_key(&mut self, key: KeyEvent) {
        if self.share_publish_rx.is_some() {
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                self.share_popup = None;
                self.mode = AppMode::Viewing;
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                if let Some(url) = self.share_popup.as_ref().and_then(|popup| popup.url.clone()) {
                    match crate::utils::open_url_in_default_browser(&url) {
                        Ok(()) => {
                            if let Some(popup) = self.share_popup.as_mut() {
                                popup.message = "Opened share URL".to_string();
                            }
                        }
                        Err(error) => {
                            self.status_message = Some(format!("Open failed: {error}"));
                        }
                    }
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                if let Some(url) = self.share_popup.as_ref().and_then(|popup| popup.url.clone()) {
                    self.copy_to_clipboard(&url);
                    if let Some(popup) = self.share_popup.as_mut() {
                        popup.message = "Copied share URL".to_string();
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_viewing_search_input(&mut self, key: KeyEvent) {
        let Some(input) = self.viewing_search_input.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.viewing_search_input = None;
                self.viewing_search_input_cursor = 0;
            }
            KeyCode::Enter => {
                let query = input.clone();
                self.viewing_search_input = None;
                self.viewing_search_input_cursor = 0;
                if query.is_empty() {
                    self.viewing_search_query.clear();
                    self.viewing_search_status = None;
                    self.viewing_match_cache.clear();
                    return;
                }
                self.viewing_search_query = query;
                self.recompute_viewing_matches();
                if self.viewing_match_cache.is_empty() {
                    self.viewing_search_status = Some("No match".to_string());
                    return;
                }
                self.viewing_search_status = None;
                self.jump_viewing_match(true);
            }
            KeyCode::Backspace if self.viewing_search_input_cursor > 0 => {
                let prev = input[..self.viewing_search_input_cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                input.replace_range(prev..self.viewing_search_input_cursor, "");
                self.viewing_search_input_cursor = prev;
            }
            KeyCode::Left if self.viewing_search_input_cursor > 0 => {
                let prev = input[..self.viewing_search_input_cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.viewing_search_input_cursor = prev;
            }
            KeyCode::Right if self.viewing_search_input_cursor < input.len() => {
                let next = input[self.viewing_search_input_cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| self.viewing_search_input_cursor + i)
                    .unwrap_or(input.len());
                self.viewing_search_input_cursor = next;
            }
            KeyCode::Home => {
                self.viewing_search_input_cursor = 0;
            }
            KeyCode::End => {
                self.viewing_search_input_cursor = input.len();
            }
            KeyCode::Char(c) => {
                input.insert(self.viewing_search_input_cursor, c);
                self.viewing_search_input_cursor += c.len_utf8();
            }
            _ => {}
        }
    }

    pub(crate) fn viewing_search_terms(&self) -> Vec<String> {
        self.viewing_search_query.split_whitespace().map(str::to_lowercase).collect()
    }

    fn recompute_viewing_matches(&mut self) {
        self.viewing_match_cache.clear();
        let terms = self.viewing_search_terms();
        if terms.is_empty() {
            return;
        }
        for (i, msg_lines) in self.viewing_sanitized_lines.iter().enumerate() {
            if msg_lines.iter().any(|l| terms.iter().any(|t| l.lower.contains(t.as_str()))) {
                self.viewing_match_cache.push(i);
            }
        }
    }

    pub(crate) fn viewing_match_indices(&self) -> &[usize] {
        &self.viewing_match_cache
    }

    fn jump_viewing_match(&mut self, forward: bool) {
        if self.viewing_search_query.is_empty() || self.viewing_match_cache.is_empty() {
            if !self.viewing_search_query.is_empty() {
                self.viewing_search_status = Some("No match".to_string());
            }
            return;
        }
        let current = self.viewing_selected_msg;
        let next = if forward {
            self.viewing_match_cache
                .iter()
                .find(|&&i| i > current)
                .copied()
                .or_else(|| self.viewing_match_cache.first().copied())
        } else {
            self.viewing_match_cache
                .iter()
                .rev()
                .find(|&&i| i < current)
                .copied()
                .or_else(|| self.viewing_match_cache.last().copied())
        };
        if let Some(idx) = next {
            self.viewing_selected_msg = idx;
            self.viewing_search_status = None;
        }
    }

    fn start_resume_confirmation(&mut self, origin: ResumeOrigin) {
        self.start_source_command_confirmation(
            origin,
            session_action::SessionAction::Resume,
            PendingCommandAction::Resume,
        );
    }

    fn start_app_open_confirmation(&mut self, origin: ResumeOrigin) {
        self.start_source_command_confirmation(
            origin,
            session_action::SessionAction::OpenApp,
            PendingCommandAction::OpenApp,
        );
    }

    fn start_source_command_confirmation(
        &mut self,
        origin: ResumeOrigin,
        source_action: session_action::SessionAction,
        action: PendingCommandAction,
    ) {
        let Some(result) = self.results.get(self.selected_index) else {
            return;
        };
        let session = &result.session;
        if session.is_import {
            self.status_message =
                Some("Imported session: not resumable on this machine".to_string());
            return;
        }
        let Some(command) =
            session_action::command_for(source_action, &session.source, &session.source_id)
        else {
            self.status_message =
                Some(format!("{} not supported for {}", source_action.title(), session.source));
            return;
        };
        self.pending_resume = Some(PendingResume {
            command,
            action,
            source_label: self.source_label_for(&session.source).to_string(),
            session_title: session.title.clone(),
            cwd: session.directory.clone(),
            origin,
        });
        self.mode = AppMode::ConfirmResume;
    }

    fn open_handoff_target_picker(&mut self) {
        self.handoff_target_selected = 0;
        self.mode = AppMode::HandoffTarget;
    }

    fn handle_handoff_target_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Viewing;
            }
            KeyCode::Up | KeyCode::Char('k') if self.handoff_target_selected > 0 => {
                self.handoff_target_selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j')
                if self.handoff_target_selected + 1 < handoff::TARGETS.len() =>
            {
                self.handoff_target_selected += 1;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let target = &handoff::TARGETS[self.handoff_target_selected];
                self.start_handoff_confirmation(target);
            }
            _ => {}
        }
    }

    fn start_handoff_confirmation(&mut self, target: &handoff::HandoffTarget) {
        let Some(result) = self.results.get(self.selected_index) else {
            return;
        };
        let session = &result.session;
        let prompt = handoff::build_prompt(session, &self.viewing_messages);
        let command = handoff::command_for_target(target, prompt);
        self.pending_resume = Some(PendingResume {
            command,
            action: PendingCommandAction::Handoff,
            source_label: target.label.to_string(),
            session_title: session.title.clone(),
            cwd: session.directory.clone(),
            origin: ResumeOrigin::Viewing,
        });
        self.mode = AppMode::ConfirmResume;
    }

    fn handle_confirm_resume_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if let Some(pending) = self.pending_resume.take() {
                    self.exec_on_exit = Some((pending.command, pending.cwd));
                    self.should_quit = true;
                } else {
                    self.mode = AppMode::Search;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                let origin =
                    self.pending_resume.take().map(|p| p.origin).unwrap_or(ResumeOrigin::Search);
                self.mode = match origin {
                    ResumeOrigin::Search => AppMode::Search,
                    ResumeOrigin::Viewing => AppMode::Viewing,
                };
            }
            _ => {}
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent, store: &Store) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Search;
            }
            KeyCode::Up => self.handle_scroll_up(store),
            KeyCode::Down => self.handle_scroll_down(store),
            KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => {
                self.update_setting(store);
            }
            _ => {}
        }
    }

    fn handle_filters_key(&mut self, key: KeyEvent, store: &Store) {
        if self.filters_editing_source {
            self.handle_source_picker_key(key, store);
            return;
        }
        if self.filters_editing_project {
            self.handle_project_picker_key(key, store);
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.close_filters();
            }
            KeyCode::Up => self.handle_scroll_up(store),
            KeyCode::Down => self.handle_scroll_down(store),
            KeyCode::Left => {
                self.adjust_filter_value(false);
            }
            KeyCode::Right => {
                self.adjust_filter_value(true);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.activate_filter_row(store);
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.clear_filters();
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.set_time_filter(TimeRange::Today);
            }
            KeyCode::Char('w') | KeyCode::Char('W') => {
                self.set_time_filter(TimeRange::Week);
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                self.set_time_filter(TimeRange::Month);
            }
            KeyCode::Char('l') | KeyCode::Char('L') => {
                self.set_time_filter(TimeRange::All);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.set_sort_order(SortOrder::Relevance);
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.set_sort_order(SortOrder::Newest);
            }
            _ => {}
        }
    }

    fn activate_filter_row(&mut self, store: &Store) {
        match self.filter_focus {
            FilterFocus::Source => {
                self.open_source_picker();
            }
            FilterFocus::Project => {
                self.open_project_picker(store);
            }
            FilterFocus::Time | FilterFocus::Sort => {}
        }
    }

    fn close_filters(&mut self) {
        self.mode = AppMode::Search;
        if self.filters_dirty {
            self.source_filter_selection = self.draft_source_filter_selection.clone();
            self.project_filter = self.draft_project_filter.clone();
            self.repo_filter = self.draft_repo_filter.clone();
            self.time_filter = self.draft_time_filter;
            self.sort_order = self.draft_sort_order;
            self.filters_dirty = false;
            self.invalidate_active_search();
            self.queue_search_with_feedback("Filters queued...");
        }
    }

    fn mark_filters_dirty(&mut self) {
        self.filters_dirty = true;
    }

    fn adjust_filter_value(&mut self, forward: bool) {
        match self.filter_focus {
            FilterFocus::Time => self.cycle_time_filter(forward),
            FilterFocus::Sort => self.cycle_sort_order(),
            FilterFocus::Source | FilterFocus::Project => {}
        }
    }

    fn set_time_filter(&mut self, time_filter: TimeRange) {
        if self.draft_time_filter != time_filter {
            self.draft_time_filter = time_filter;
            self.mark_filters_dirty();
        }
    }

    fn cycle_time_filter(&mut self, forward: bool) {
        let next = match (self.draft_time_filter, forward) {
            (TimeRange::All, true) => TimeRange::Today,
            (TimeRange::Today, true) => TimeRange::Week,
            (TimeRange::Week, true) => TimeRange::Month,
            (TimeRange::Month, true) => TimeRange::All,
            (TimeRange::All, false) => TimeRange::Month,
            (TimeRange::Month, false) => TimeRange::Week,
            (TimeRange::Week, false) => TimeRange::Today,
            (TimeRange::Today, false) => TimeRange::All,
        };
        self.set_time_filter(next);
    }

    fn set_sort_order(&mut self, sort_order: SortOrder) {
        if self.draft_sort_order != sort_order {
            self.draft_sort_order = sort_order;
            self.mark_filters_dirty();
        }
    }

    fn cycle_sort_order(&mut self) {
        let next = match self.draft_sort_order {
            SortOrder::Relevance => SortOrder::Newest,
            SortOrder::Newest => SortOrder::Relevance,
        };
        self.set_sort_order(next);
    }

    fn handle_source_picker_key(&mut self, key: KeyEvent, store: &Store) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            self.source_picker_query.clear();
            self.source_picker_cursor = 0;
            self.source_picker_selected = 0;
            self.source_picker_typing = true;
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.source_picker_selection.clear();
            self.source_picker_dirty = true;
            return;
        }

        match key.code {
            KeyCode::Esc => {
                if self.source_picker_typing {
                    self.source_picker_typing = false;
                } else {
                    self.close_source_picker();
                }
            }
            KeyCode::Enter => {
                self.apply_source_picker();
            }
            KeyCode::Char(' ') => {
                self.toggle_source_picker_row();
            }
            KeyCode::Char('/') if !self.source_picker_typing => {
                self.source_picker_typing = true;
            }
            KeyCode::Up => self.handle_scroll_up(store),
            KeyCode::Down => self.handle_scroll_down(store),
            KeyCode::Backspace if self.source_picker_typing && self.source_picker_cursor > 0 => {
                let prev = self.source_picker_query[..self.source_picker_cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.source_picker_query.replace_range(prev..self.source_picker_cursor, "");
                self.source_picker_cursor = prev;
                self.source_picker_selected = 0;
                self.clamp_source_picker_selected();
            }
            KeyCode::Left if self.source_picker_typing && self.source_picker_cursor > 0 => {
                let prev = self.source_picker_query[..self.source_picker_cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.source_picker_cursor = prev;
            }
            KeyCode::Right
                if self.source_picker_typing
                    && self.source_picker_cursor < self.source_picker_query.len() =>
            {
                let next = self.source_picker_query[self.source_picker_cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| self.source_picker_cursor + i)
                    .unwrap_or(self.source_picker_query.len());
                self.source_picker_cursor = next;
            }
            KeyCode::Home if self.source_picker_typing => {
                self.source_picker_cursor = 0;
            }
            KeyCode::End if self.source_picker_typing => {
                self.source_picker_cursor = self.source_picker_query.len();
            }
            KeyCode::Char(c) => {
                self.source_picker_typing = true;
                self.source_picker_query.insert(self.source_picker_cursor, c);
                self.source_picker_cursor += c.len_utf8();
                self.source_picker_selected = 0;
                self.clamp_source_picker_selected();
            }
            _ => {}
        }
    }

    fn open_filters(&mut self) {
        self.mode = AppMode::Filters;
        self.filters_editing_source = false;
        self.filters_editing_project = false;
        self.draft_source_filter_selection = self.source_filter_selection.clone();
        self.draft_project_filter = self.project_filter.clone();
        self.draft_repo_filter = self.repo_filter.clone();
        self.draft_time_filter = self.time_filter;
        self.draft_sort_order = self.sort_order;
        self.filters_dirty = false;
    }

    fn open_source_picker(&mut self) {
        self.mode = AppMode::Filters;
        self.filters_editing_source = true;
        self.filters_editing_project = false;
        self.source_picker_query.clear();
        self.source_picker_cursor = 0;
        self.source_picker_selected = 0;
        self.source_picker_typing = false;
        self.source_picker_selection =
            self.normalized_source_selection(&self.draft_source_filter_selection);
        if let Some(selected_source) = self.source_picker_selection.first() {
            self.source_picker_selected = self
                .source_picker_rows()
                .iter()
                .position(|row| match row {
                    SourcePickerRow::All => false,
                    SourcePickerRow::Source(index) => self
                        .all_sources
                        .get(*index)
                        .map(|(source_id, _)| source_id == selected_source)
                        .unwrap_or(false),
                })
                .unwrap_or(0);
        }
        self.source_picker_dirty = false;
    }

    fn close_source_picker(&mut self) {
        self.filters_editing_source = false;
        self.source_picker_query.clear();
        self.source_picker_cursor = 0;
        self.source_picker_selected = 0;
        self.source_picker_typing = false;
        self.source_picker_selection.clear();
        self.source_picker_dirty = false;
    }

    fn handle_project_picker_key(&mut self, key: KeyEvent, store: &Store) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            self.project_picker_query.clear();
            self.project_picker_cursor = 0;
            self.project_picker_selected = 0;
            self.project_picker_typing = true;
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.project_picker_selection = None;
            self.project_picker_dirty = true;
            return;
        }

        match key.code {
            KeyCode::Esc => {
                if self.project_picker_typing {
                    self.project_picker_typing = false;
                } else {
                    self.close_project_picker();
                }
            }
            KeyCode::Enter => {
                self.apply_project_picker();
            }
            KeyCode::Char(' ') => {
                self.toggle_project_picker_row();
            }
            KeyCode::Char('/') if !self.project_picker_typing => {
                self.project_picker_typing = true;
            }
            KeyCode::Up => self.handle_scroll_up(store),
            KeyCode::Down => self.handle_scroll_down(store),
            KeyCode::Backspace if self.project_picker_typing && self.project_picker_cursor > 0 => {
                let prev = self.project_picker_query[..self.project_picker_cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.project_picker_query.replace_range(prev..self.project_picker_cursor, "");
                self.project_picker_cursor = prev;
                self.project_picker_selected = 0;
                self.clamp_project_picker_selected();
            }
            KeyCode::Left if self.project_picker_typing && self.project_picker_cursor > 0 => {
                let prev = self.project_picker_query[..self.project_picker_cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.project_picker_cursor = prev;
            }
            KeyCode::Right
                if self.project_picker_typing
                    && self.project_picker_cursor < self.project_picker_query.len() =>
            {
                let next = self.project_picker_query[self.project_picker_cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| self.project_picker_cursor + i)
                    .unwrap_or(self.project_picker_query.len());
                self.project_picker_cursor = next;
            }
            KeyCode::Home if self.project_picker_typing => {
                self.project_picker_cursor = 0;
            }
            KeyCode::End if self.project_picker_typing => {
                self.project_picker_cursor = self.project_picker_query.len();
            }
            KeyCode::Char(c) => {
                self.project_picker_typing = true;
                self.project_picker_query.insert(self.project_picker_cursor, c);
                self.project_picker_cursor += c.len_utf8();
                self.project_picker_selected = 0;
                self.clamp_project_picker_selected();
            }
            _ => {}
        }
    }

    fn open_project_picker(&mut self, store: &Store) {
        self.project_directories = store.list_project_directories().unwrap_or_default();
        self.mode = AppMode::Filters;
        self.filters_editing_source = false;
        self.filters_editing_project = true;
        self.project_picker_query.clear();
        self.project_picker_cursor = 0;
        self.project_picker_selected = 0;
        self.project_picker_selection = self.draft_project_filter.clone();
        self.project_picker_dirty = false;
        self.project_picker_typing = false;
        if let Some(selected_project) = self.draft_project_filter.as_ref() {
            self.project_picker_selected = self
                .project_picker_rows()
                .iter()
                .position(|row| match row {
                    ProjectPickerRow::All => false,
                    ProjectPickerRow::Project(index) => self
                        .project_directories
                        .get(*index)
                        .map(|project| &project.directory == selected_project)
                        .unwrap_or(false),
                })
                .unwrap_or(0);
        }
    }

    fn close_project_picker(&mut self) {
        self.filters_editing_project = false;
        self.project_picker_query.clear();
        self.project_picker_cursor = 0;
        self.project_picker_selected = 0;
        self.project_picker_selection = None;
        self.project_picker_dirty = false;
        self.project_picker_typing = false;
    }

    fn apply_project_picker(&mut self) {
        let previous_project = self.draft_project_filter.clone();
        let previous_repo = self.draft_repo_filter.clone();
        self.commit_project_picker_filter();
        self.close_project_picker();
        self.mode = AppMode::Filters;
        if self.draft_project_filter != previous_project || self.draft_repo_filter != previous_repo
        {
            self.mark_filters_dirty();
        }
    }

    fn commit_project_picker_filter(&mut self) {
        if self.project_picker_dirty {
            self.draft_project_filter = self.project_picker_selection.clone();
            self.draft_repo_filter = None;
        } else if let Some(row) = self.project_picker_rows().get(self.project_picker_selected) {
            match *row {
                ProjectPickerRow::All => {
                    self.draft_project_filter = None;
                    self.draft_repo_filter = None;
                }
                ProjectPickerRow::Project(index) => {
                    if let Some(project) = self.project_directories.get(index) {
                        self.draft_project_filter = Some(project.directory.clone());
                        self.draft_repo_filter = None;
                    }
                }
            }
        }
    }

    fn toggle_project_picker_row(&mut self) {
        let Some(row) = self.project_picker_rows().get(self.project_picker_selected).copied()
        else {
            return;
        };

        match row {
            ProjectPickerRow::All => {
                self.project_picker_selection = None;
            }
            ProjectPickerRow::Project(index) => {
                let Some(project) = self.project_directories.get(index) else {
                    return;
                };
                if self.project_picker_selection.as_deref() == Some(project.directory.as_str()) {
                    self.project_picker_selection = None;
                } else {
                    self.project_picker_selection = Some(project.directory.clone());
                }
            }
        }
        self.project_picker_dirty = true;
    }

    fn apply_source_picker(&mut self) {
        let previous = self.draft_source_filter_selection.clone();
        self.commit_source_picker_filter();

        self.source_picker_query.clear();
        self.source_picker_cursor = 0;
        self.source_picker_selected = 0;
        self.source_picker_typing = false;
        self.source_picker_selection.clear();
        self.source_picker_dirty = false;
        self.filters_editing_source = false;
        self.mode = AppMode::Filters;
        if self.draft_source_filter_selection != previous {
            self.mark_filters_dirty();
        }
    }

    fn commit_source_picker_filter(&mut self) {
        let confirming_existing_multi_selection = !self.source_picker_dirty
            && self.source_picker_query.trim().is_empty()
            && self.source_picker_selection.len() > 1;

        if self.source_picker_dirty || confirming_existing_multi_selection {
            self.draft_source_filter_selection =
                self.normalized_source_selection(&self.source_picker_selection);
        } else if let Some(row) = self.source_picker_rows().get(self.source_picker_selected) {
            match *row {
                SourcePickerRow::All => {
                    self.draft_source_filter_selection.clear();
                }
                SourcePickerRow::Source(index) => {
                    if let Some((source_id, _)) = self.all_sources.get(index) {
                        self.draft_source_filter_selection = vec![source_id.clone()];
                    }
                }
            }
        }
    }

    fn toggle_source_picker_row(&mut self) {
        let Some(row) = self.source_picker_rows().get(self.source_picker_selected).copied() else {
            return;
        };

        match row {
            SourcePickerRow::All => {
                self.source_picker_selection.clear();
            }
            SourcePickerRow::Source(index) => {
                let Some((source_id, _)) = self.all_sources.get(index) else {
                    return;
                };
                if let Some(pos) =
                    self.source_picker_selection.iter().position(|id| id == source_id)
                {
                    self.source_picker_selection.remove(pos);
                } else {
                    self.source_picker_selection.push(source_id.clone());
                }
                self.source_picker_selection =
                    self.normalized_source_selection(&self.source_picker_selection);
            }
        }
        self.source_picker_dirty = true;
    }

    fn clear_filters(&mut self) {
        let was_filtered = !self.draft_source_filter_selection.is_empty()
            || self.draft_project_filter.is_some()
            || self.draft_repo_filter.is_some()
            || self.draft_time_filter != TimeRange::All
            || self.draft_sort_order != SortOrder::Newest;
        self.draft_source_filter_selection.clear();
        self.draft_project_filter = None;
        self.draft_repo_filter = None;
        self.draft_time_filter = TimeRange::All;
        self.draft_sort_order = SortOrder::Newest;
        self.filter_focus = FilterFocus::Source;
        if was_filtered {
            self.mark_filters_dirty();
        }
    }

    pub(crate) fn refresh_usage(&mut self, store: &Store) {
        self.usage_refresh_requested_at = None;
        self.usage_error = None;
        self.skill_audit_error = None;
        self.usage_breakdown_scroll = 0;
        self.skill_audit_selected = 0;
        let sources = self.source_filter_ids();
        let current_filters =
            UsageFilters { sources: sources.clone(), time_range: self.usage_time_filter };
        match usage::build_usage_report(store, &current_filters) {
            Ok(report) => {
                self.usage_report = Some(report);
            }
            Err(err) => {
                self.usage_report = None;
                self.usage_error = Some(format!("Usage unavailable: {err}"));
            }
        }

        let year_filters = UsageFilters { sources, time_range: TimeRange::All };
        match usage::build_usage_report(store, &year_filters) {
            Ok(report) => {
                self.usage_year_report = Some(report);
            }
            Err(err) => {
                self.usage_year_report = None;
                if self.usage_error.is_none() {
                    self.usage_error = Some(format!("Usage unavailable: {err}"));
                }
            }
        }

        let skill_filters = SkillAuditFilters {
            sources: self.source_filter_ids(),
            time_range: self.usage_time_filter,
        };
        match skill_audit::build_skill_audit_report(store, &skill_filters) {
            Ok(report) => {
                self.skill_audit_report = Some(report);
            }
            Err(err) => {
                self.skill_audit_report = None;
                self.skill_audit_error = Some(format!("Skill audit unavailable: {err}"));
            }
        }
    }

    pub(crate) fn request_usage_refresh(&mut self) {
        self.usage_error = None;
        self.usage_refresh_requested_at = Some(Instant::now());
    }

    pub(crate) fn usage_is_loading(&self) -> bool {
        self.usage_refresh_requested_at.is_some()
    }

    pub(crate) fn usage_refresh_is_due(&self) -> bool {
        self.usage_refresh_requested_at
            .map(|requested_at| requested_at.elapsed().as_millis() >= USAGE_LOADING_MIN_MS)
            .unwrap_or(false)
    }

    pub(crate) fn fail_usage_refresh(&mut self, error: impl std::fmt::Display) {
        self.usage_refresh_requested_at = None;
        self.usage_report = None;
        self.usage_year_report = None;
        self.skill_audit_report = None;
        self.usage_error = Some(format!("Usage unavailable: {error}"));
        self.skill_audit_error = Some(format!("Skill audit unavailable: {error}"));
    }

    pub(crate) fn try_search(&mut self, store: &Store, worker: &SearchWorker) {
        self.refresh_semantic_progress(store);
        if !self.search_pending {
            return;
        }
        if self.last_keystroke.elapsed() < Duration::from_millis(SEARCH_DEBOUNCE_MS) {
            return;
        }
        self.search_pending = false;
        let query = self.query.trim().to_string();

        let request_id = self.next_search_request_id();
        let request = SearchRequest {
            id: request_id,
            query,
            filters: self.search_filters(),
            semantic_ready: self.semantic_ready(),
        };

        if worker.search(request) {
            self.active_search_id = request_id;
            self.search_in_flight = true;
            self.search_feedback = Some("Searching...".to_string());
        } else {
            self.search_in_flight = false;
            self.search_feedback = None;
            self.status_message = Some("Search worker unavailable".to_string());
        }
    }

    pub(crate) fn apply_search_response(&mut self, store: &Store, response: SearchResponse) {
        if response.id != self.active_search_id || response.query != self.query.trim() {
            return;
        }

        match response.result {
            Ok(mut results) => {
                self.apply_sort(&mut results);
                self.results = results;
                self.selected_index = 0;
                self.result_scroll_offset = 0;
                self.panel_focus = PanelFocus::SessionList;
                self.load_preview(store);

                if response.phase == SearchPhase::Text
                    && !response.query.is_empty()
                    && self.semantic_ready()
                {
                    self.search_in_flight = true;
                    self.search_feedback = Some("Refining semantic results...".to_string());
                } else {
                    self.search_in_flight = false;
                    self.search_feedback = None;
                }
            }
            Err(error) => {
                if response.phase == SearchPhase::Text {
                    self.results.clear();
                    self.preview_messages.clear();
                } else {
                    self.embedding_unavailable = true;
                }
                self.search_in_flight = false;
                self.search_feedback = None;
                self.status_message = Some(error);
            }
        }
    }

    fn next_search_request_id(&mut self) -> u64 {
        self.search_request_id = self.search_request_id.saturating_add(1);
        self.search_request_id
    }

    fn invalidate_active_search(&mut self) {
        self.active_search_id = self.next_search_request_id();
        self.search_in_flight = false;
    }

    fn search_filters(&self) -> SearchFilters {
        SearchFilters {
            sources: self.source_filter_ids(),
            time_range: self.time_filter,
            directory: self.project_filter.clone(),
            repo: self.repo_filter.clone(),
        }
    }

    fn queue_search(&mut self) {
        self.queue_search_with_feedback("Search queued...");
    }

    fn queue_search_now(&mut self) {
        let now = Instant::now();
        self.last_keystroke =
            now.checked_sub(Duration::from_millis(SEARCH_DEBOUNCE_MS)).unwrap_or(now);
        self.search_pending = true;
        self.search_feedback = Some("Search queued...".to_string());
        self.panel_focus = PanelFocus::SessionList;
    }

    fn queue_search_with_feedback(&mut self, feedback: &str) {
        self.last_keystroke = Instant::now();
        self.search_pending = true;
        self.search_feedback = Some(feedback.to_string());
    }

    fn semantic_ready(&self) -> bool {
        !self.embedding_unavailable
            && (self.semantic_progress.done_sessions > 0
                || self.semantic_progress.processing_sessions > 0)
    }

    fn refresh_semantic_progress(&mut self, store: &Store) {
        if self.semantic_last_refresh.elapsed().as_millis() < 750 {
            return;
        }
        self.update_scope_metrics(store);
        self.semantic_last_refresh = Instant::now();
    }

    fn update_scope_metrics(&mut self, store: &Store) {
        if let Ok((sessions, messages)) = store.stats_for_search_scope(
            self.source_filter_ids().as_deref(),
            self.time_filter,
            self.project_filter.as_deref(),
            self.repo_filter.as_ref(),
        ) {
            self.total_sessions = sessions;
            self.total_messages = messages;
        }
        if let Ok(progress) = store.semantic_progress_for_search_scope(
            self.source_filter_ids().as_deref(),
            self.time_filter,
            self.project_filter.as_deref(),
            self.repo_filter.as_ref(),
        ) {
            self.semantic_progress = progress;
        }
        if let Ok(status) = store.background_job_status("pipeline") {
            self.background_status = status;
        }
    }

    pub(crate) fn enabled_sources(&self) -> Vec<&(String, String)> {
        self.all_sources.iter().filter(|(id, _)| self.config.is_source_enabled(id)).collect()
    }

    pub(crate) fn source_is_selected_in_picker(&self, source_id: &str) -> bool {
        self.normalized_source_selection(&self.source_picker_selection)
            .iter()
            .any(|id| id == source_id)
    }

    pub(crate) fn source_picker_rows(&self) -> Vec<SourcePickerRow> {
        let query = self.source_picker_query.trim().to_lowercase();
        let mut rows = Vec::new();
        if query.is_empty() {
            rows.push(SourcePickerRow::All);
        }

        for (index, (source_id, label)) in self.all_sources.iter().enumerate() {
            if !self.config.is_source_enabled(source_id) {
                continue;
            }

            if query.is_empty()
                || source_id.to_lowercase().contains(&query)
                || label.to_lowercase().contains(&query)
            {
                rows.push(SourcePickerRow::Source(index));
            }
        }

        rows
    }

    pub(crate) fn project_picker_rows(&self) -> Vec<ProjectPickerRow> {
        let query = self.project_picker_query.trim().to_lowercase();
        let mut rows = Vec::new();
        if query.is_empty() {
            rows.push(ProjectPickerRow::All);
        }

        for (index, project) in self.project_directories.iter().enumerate() {
            if query.is_empty() || project_matches_query(&project.directory, &query) {
                rows.push(ProjectPickerRow::Project(index));
            }
        }

        rows
    }

    fn clamp_source_picker_selected(&mut self) {
        let row_count = self.source_picker_rows().len();
        if row_count == 0 {
            self.source_picker_selected = 0;
        } else if self.source_picker_selected >= row_count {
            self.source_picker_selected = row_count - 1;
        }
    }

    fn clamp_project_picker_selected(&mut self) {
        let row_count = self.project_picker_rows().len();
        if row_count == 0 {
            self.project_picker_selected = 0;
        } else if self.project_picker_selected >= row_count {
            self.project_picker_selected = row_count - 1;
        }
    }

    fn normalized_source_selection(&self, selection: &[String]) -> Vec<String> {
        self.all_sources
            .iter()
            .filter(|(source_id, _)| {
                self.config.is_source_enabled(source_id)
                    && selection.iter().any(|selected| selected == source_id)
            })
            .map(|(source_id, _)| source_id.clone())
            .collect()
    }

    fn reset_search_defaults(&mut self) {
        self.source_filter_selection.clear();
        self.project_filter = None;
        self.repo_filter = default_repo_filter(&self.config);
        self.time_filter = self.config.sync_window.to_time_range();
        self.sort_order = SortOrder::Newest;
        self.draft_source_filter_selection = self.source_filter_selection.clone();
        self.draft_project_filter = self.project_filter.clone();
        self.draft_repo_filter = self.repo_filter.clone();
        self.draft_time_filter = self.time_filter;
        self.draft_sort_order = self.sort_order;
        self.filters_dirty = false;
    }

    fn reset_usage_dashboard(&mut self) {
        self.source_filter_selection.clear();
        self.usage_time_filter = TimeRange::All;
        self.usage_breakdown_scroll = 0;
        self.skill_audit_selected = 0;
    }

    pub(crate) fn usage_tab_label(&self) -> &'static str {
        match self.usage_tab {
            UsageTab::Tokens => "tokens",
            UsageTab::Skills => "skills",
        }
    }

    pub(crate) fn skill_audit_entry_count(&self) -> usize {
        let Some(report) = self.skill_audit_report.as_ref() else {
            return 0;
        };
        report.core.len() + report.occasional.len() + report.dormant.len()
    }

    fn open_skill_sessions(&mut self, store: &Store) {
        let Some(report) = self.skill_audit_report.as_ref() else {
            return;
        };
        let entries: Vec<_> =
            report.core.iter().chain(&report.occasional).chain(&report.dormant).collect();
        let Some(entry) = entries.get(self.skill_audit_selected) else {
            return;
        };
        let skill_id = entry.id.clone();
        let session_ids = entry.session_ids.clone();
        if session_ids.is_empty() {
            self.status_message =
                Some(format!("No indexed sessions for skill '{skill_id}' in this range"));
            return;
        }
        match store.list_sessions_by_ids(&session_ids) {
            Ok(sessions) if sessions.is_empty() => {
                self.status_message =
                    Some(format!("Sessions for '{skill_id}' are no longer in index"));
            }
            Ok(sessions) => {
                let count = sessions.len();
                self.results = sessions
                    .into_iter()
                    .map(|session| SearchResult {
                        session,
                        match_source: MatchSource::Fts,
                        snippet: None,
                    })
                    .collect();
                self.selected_index = 0;
                self.result_scroll_offset = 0;
                self.panel_focus = PanelFocus::SessionList;
                self.mode = AppMode::Search;
                self.query = format!("skill:{skill_id}");
                self.cursor_pos = self.query.len();
                self.load_preview(store);
                self.status_message = Some(format!("{count} sessions used skill '{skill_id}'"));
            }
            Err(err) => {
                self.status_message = Some(format!("Failed to load sessions: {err}"));
            }
        }
    }

    fn cycle_usage_time(&mut self, reverse: bool) {
        self.usage_time_filter = if reverse {
            match self.usage_time_filter {
                TimeRange::Today => TimeRange::All,
                TimeRange::Week => TimeRange::Today,
                TimeRange::Month => TimeRange::Week,
                TimeRange::All => TimeRange::Month,
            }
        } else {
            match self.usage_time_filter {
                TimeRange::Today => TimeRange::Week,
                TimeRange::Week => TimeRange::Month,
                TimeRange::Month => TimeRange::All,
                TimeRange::All => TimeRange::Today,
            }
        };
    }

    fn cycle_usage_source(&mut self, reverse: bool) {
        let enabled: Vec<String> =
            self.enabled_sources().into_iter().map(|(id, _)| id.clone()).collect();
        if enabled.is_empty() {
            self.source_filter_selection.clear();
            return;
        }

        let current = self.normalized_source_selection(&self.source_filter_selection);
        let next = if current.len() == 1 {
            let current_index = enabled.iter().position(|id| id == &current[0]);
            match (current_index, reverse) {
                (Some(0), true) => None,
                (Some(index), true) => enabled.get(index - 1).cloned(),
                (Some(index), false) if index + 1 < enabled.len() => {
                    enabled.get(index + 1).cloned()
                }
                (Some(_), false) => None,
                (None, true) => enabled.last().cloned(),
                (None, false) => enabled.first().cloned(),
            }
        } else if reverse {
            enabled.last().cloned()
        } else {
            enabled.first().cloned()
        };

        self.source_filter_selection = next.into_iter().collect();
    }

    fn settings_row_count(&self) -> usize {
        2 + self.all_sources.len()
    }

    fn update_setting(&mut self, store: &Store) {
        if self.settings_selected == 0 {
            self.config.sync_window = self.config.sync_window.next();
        } else if self.settings_selected == 1 {
            self.config.default_current_repo_scope = !self.config.default_current_repo_scope;
        } else if let Some((source_id, _)) = self.all_sources.get(self.settings_selected - 2) {
            if self.config.is_source_enabled(source_id) {
                let enabled_count =
                    self.all_sources.len().saturating_sub(self.config.disabled_sources.len());
                if enabled_count <= 1 {
                    self.status_message = Some("At least one source must stay enabled".to_string());
                    return;
                }
                self.config.disabled_sources.push(source_id.clone());
                self.config.disabled_sources.sort();
                self.config.disabled_sources.dedup();
            } else {
                self.config.disabled_sources.retain(|id| id != source_id);
            }
        }

        if let Err(e) = self.config.save() {
            self.status_message = Some(format!("Failed to save settings: {e}"));
            return;
        }

        self.reset_search_defaults();
        self.update_scope_metrics(store);
        self.status_message = Some("Settings saved".to_string());
        if self.query.is_empty() {
            self.load_recent(store);
        } else {
            self.queue_search_now();
        }
    }

    fn load_preview(&mut self, store: &Store) {
        self.preview_selected_msg = 0;
        self.preview_scroll_offset = 0;
        if let Some(result) = self.results.get(self.selected_index) {
            match store.get_messages(&result.session.id) {
                Ok(msgs) => {
                    self.preview_messages = msgs.into_iter().take(30).collect();
                }
                Err(_) => {
                    self.preview_messages.clear();
                }
            }
        } else {
            self.preview_messages.clear();
        }
    }

    fn enter_viewing(&mut self, store: &Store) {
        if let Some(result) = self.results.get(self.selected_index)
            && let Ok(msgs) = store.get_messages(&result.session.id)
        {
            let usage_events =
                store.list_usage_events_for_session(&result.session.id).unwrap_or_default();
            self.viewing_session_summary = Some(ViewingSessionSummary::from_session(
                &msgs,
                result.session.duration_minutes,
                &usage_events,
            ));
            self.viewing_sanitized_lines = build_viewing_caches(&msgs);
            self.viewing_messages = msgs;
            self.viewing_selected_msg = 0;
            self.viewing_scroll_offset = 0;
            self.viewing_search_query = self
                .query
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            self.viewing_search_input = None;
            self.viewing_search_input_cursor = 0;
            self.viewing_search_status = None;
            self.recompute_viewing_matches();
            if let Some(&first) = self.viewing_match_cache.first() {
                self.viewing_selected_msg = first;
            }
            self.mode = AppMode::Viewing;
        }
    }

    fn copy_current_message(&mut self) {
        let text = self.viewing_messages.get(self.viewing_selected_msg).map(|m| m.content.clone());
        if let Some(text) = text {
            self.copy_to_clipboard(&text);
        }
    }

    fn preview_current_session(&mut self) {
        let Some(result) = self.results.get(self.selected_index) else {
            return;
        };
        let session = result.session.clone();
        let messages = self.viewing_messages.clone();
        let session_id = session.id.clone();
        let usage_events = crate::db::store::Store::open()
            .and_then(|store| store.list_usage_events_for_session(&session_id))
            .unwrap_or_default();
        match crate::share::open_session_preview(&session, &messages, &usage_events) {
            Ok(path) => {
                self.viewing_search_status = None;
                self.status_message = Some(format!("Opened preview: {}", path.display()));
            }
            Err(error) => {
                self.viewing_search_status = None;
                self.status_message = Some(format!("Preview failed: {error}"));
            }
        }
    }

    fn share_current_session(&mut self) {
        let Some(result) = self.results.get(self.selected_index) else {
            return;
        };
        if self.share_publish_rx.is_some() {
            return;
        }
        let config = self.config.clone();
        let session = result.session.clone();
        let messages = self.viewing_messages.clone();
        let session_id = session.id.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let usage_events = crate::db::store::Store::open()
                .and_then(|store| store.list_usage_events_for_session(&session_id))
                .unwrap_or_default();
            let result = crate::share::publish_session(&config, &session, &messages, &usage_events)
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        self.share_publish_rx = Some(rx);
        self.share_popup = Some(SharePopup {
            url: None,
            message: "Sharing session...".to_string(),
            is_error: false,
        });
        self.mode = AppMode::ShareResult;
    }

    fn copy_to_clipboard(&mut self, text: &str) {
        let (cmd, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
            ("pbcopy", &[])
        } else if cfg!(target_os = "windows") {
            ("clip.exe", &[])
        } else {
            ("xclip", &["-selection", "clipboard"])
        };

        match Command::new(cmd).args(args).stdin(Stdio::piped()).spawn() {
            Ok(mut child) => {
                if let Some(ref mut stdin) = child.stdin {
                    let _ = stdin.write_all(text.as_bytes());
                }
                let _ = child.wait();
                self.status_message = Some("Copied to clipboard".to_string());
            }
            Err(_) => {
                self.status_message = Some(format!("Failed to copy ({cmd} not found)"));
            }
        }
    }

    fn start_export(&mut self) {
        let session = match self.results.get(self.selected_index) {
            Some(r) => &r.session,
            None => return,
        };

        let safe_title: String = session
            .title
            .chars()
            .take(40)
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let source = self.source_label_for(&session.source);

        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
            .or_else(|| dirs::home_dir().map(|h| h.display().to_string()))
            .unwrap_or_default();
        self.export_path = format!("{cwd}/recall-{source}-{safe_title}.txt");
        self.export_cursor = self.export_path.len();
        self.mode = AppMode::ExportInput;
    }

    fn handle_export_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Viewing;
                self.export_path.clear();
            }
            KeyCode::Enter => {
                let path = self.export_path.clone();
                self.mode = AppMode::Viewing;
                self.do_export(&path);
                self.export_path.clear();
            }
            KeyCode::Char(c) => {
                self.export_path.insert(self.export_cursor, c);
                self.export_cursor += c.len_utf8();
            }
            KeyCode::Backspace if self.export_cursor > 0 => {
                let prev = self.export_path[..self.export_cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.export_path.replace_range(prev..self.export_cursor, "");
                self.export_cursor = prev;
            }
            KeyCode::Left if self.export_cursor > 0 => {
                self.export_cursor = self.export_path[..self.export_cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
            KeyCode::Right if self.export_cursor < self.export_path.len() => {
                self.export_cursor = self.export_path[self.export_cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| self.export_cursor + i)
                    .unwrap_or(self.export_path.len());
            }
            _ => {}
        }
    }

    fn do_export(&mut self, path: &str) {
        let session = match self.results.get(self.selected_index) {
            Some(r) => &r.session,
            None => return,
        };

        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            let _ = std::fs::create_dir_all(parent);
        }

        let content = transcript::render_plain(session, &self.viewing_messages);

        match std::fs::write(path, &content) {
            Ok(_) => {
                self.status_message = Some(format!("Exported to {path}"));
            }
            Err(e) => {
                self.status_message = Some(format!("Export failed: {e}"));
            }
        }
    }

    fn apply_sort(&self, results: &mut [SearchResult]) {
        if self.sort_order == SortOrder::Newest {
            results.sort_by_key(|b| {
                std::cmp::Reverse((
                    b.session.updated_at.unwrap_or(b.session.started_at),
                    b.session.started_at,
                ))
            });
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

fn default_repo_filter(config: &AppConfig) -> Option<RepoFilter> {
    if !config.default_current_repo_scope {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    repo_filter_for_dir(&cwd)
}

fn repo_filter_for_dir(dir: &Path) -> Option<RepoFilter> {
    let mut cache = RepoIdentityCache::default();
    let identity = cache.resolve(dir.to_str())?;
    Some(RepoFilter::Remote(identity.remote))
}

fn repo_filter_label(repo: &RepoFilter) -> String {
    match repo {
        RepoFilter::Remote(remote) => short_project_label(remote),
        RepoFilter::Slug(slug) => slug.clone(),
        RepoFilter::Name(name) => name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Role, SessionUsageEventRecord};

    fn source(id: &str, label: &str) -> (String, String) {
        (id.to_string(), label.to_string())
    }

    fn app_with_sources() -> App {
        App {
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
            viewing_messages: Vec::new(),
            viewing_selected_msg: 0,
            viewing_scroll_offset: 0,
            viewing_session_summary: None,
            all_sources: vec![
                source("claude", "Claude"),
                source("cursor", "Cursor"),
                source("codex", "Codex"),
            ],
            config: AppConfig::default(),
            source_filter_selection: Vec::new(),
            project_directories: Vec::new(),
            project_filter: None,
            repo_filter: None,
            time_filter: TimeRange::All,
            filter_focus: FilterFocus::Source,
            filters_dirty: false,
            draft_source_filter_selection: Vec::new(),
            draft_project_filter: None,
            draft_repo_filter: None,
            draft_time_filter: TimeRange::All,
            draft_sort_order: SortOrder::Newest,
            should_quit: false,
            last_keystroke: Instant::now(),
            search_pending: false,
            search_request_id: 0,
            active_search_id: 0,
            search_in_flight: false,
            search_feedback: None,
            embedding_unavailable: false,
            status_message: None,
            sort_order: SortOrder::Newest,
            export_path: String::new(),
            export_cursor: 0,
            total_sessions: 0,
            total_messages: 0,
            semantic_progress: SemanticProgress::default(),
            background_status: BackgroundJobStatus::default(),
            semantic_last_refresh: Instant::now(),
            settings_selected: 0,
            pending_resume: None,
            handoff_target_selected: 0,
            share_popup: None,
            share_publish_rx: None,
            exec_on_exit: None,
            viewing_search_query: String::new(),
            viewing_search_input: None,
            viewing_search_input_cursor: 0,
            viewing_search_status: None,
            viewing_sanitized_lines: Vec::new(),
            viewing_match_cache: Vec::new(),
            source_picker_query: String::new(),
            source_picker_cursor: 0,
            source_picker_selected: 0,
            source_picker_selection: Vec::new(),
            source_picker_dirty: false,
            source_picker_typing: false,
            filters_editing_source: false,
            project_picker_query: String::new(),
            project_picker_cursor: 0,
            project_picker_selected: 0,
            project_picker_selection: None,
            project_picker_dirty: false,
            project_picker_typing: false,
            filters_editing_project: false,
            usage_report: None,
            usage_year_report: None,
            usage_error: None,
            usage_time_filter: TimeRange::All,
            usage_refresh_requested_at: None,
            usage_breakdown_scroll: 0,
            usage_tab: UsageTab::Tokens,
            skill_audit_report: None,
            skill_audit_error: None,
            skill_audit_selected: 0,
        }
    }

    fn codex_search_result() -> SearchResult {
        SearchResult {
            session: crate::types::Session {
                id: "session1".to_string(),
                source: "codex".to_string(),
                source_id: "019e6d8d-588b-7fd2-a326-c525469ed120".to_string(),
                title: "Codex thread".to_string(),
                directory: Some("/tmp/project".to_string()),
                repo_remote: None,
                repo_slug: None,
                repo_name: None,
                started_at: 0,
                updated_at: None,
                message_count: 1,
                entrypoint: None,
                custom_title: None,
                summary: None,
                duration_minutes: None,
                source_file_path: None,
                is_import: false,
            },
            match_source: MatchSource::Fts,
            snippet: None,
        }
    }

    fn search_result_with_times(
        source_id: &str,
        started_at: i64,
        updated_at: Option<i64>,
    ) -> SearchResult {
        let mut result = codex_search_result();
        result.session.source_id = source_id.to_string();
        result.session.started_at = started_at;
        result.session.updated_at = updated_at;
        result
    }

    fn message(role: Role, timestamp: Option<i64>, seq: u32) -> Message {
        Message {
            session_id: "session1".to_string(),
            role,
            content: "hello".to_string(),
            timestamp,
            seq,
        }
    }

    fn usage_event(input_tokens: i64, output_tokens: i64) -> SessionUsageEventRecord {
        SessionUsageEventRecord {
            event_key: "event1".to_string(),
            event_seq: 0,
            message_seq: None,
            timestamp: 120_000,
            model: "gpt".to_string(),
            provider: "openai".to_string(),
            input_tokens,
            output_tokens,
            cache_read_tokens: 3,
            cache_write_tokens: 2,
            reasoning_tokens: 1,
            token_source: "derived".to_string(),
            parser_version: 1,
            source_path: None,
            raw_usage_json: None,
        }
    }

    fn numbered_results(count: usize) -> Vec<SearchResult> {
        (0..count)
            .map(|n| {
                let mut result = codex_search_result();
                result.session.id = format!("session{n}");
                result.session.title = format!("Session {n}");
                result
            })
            .collect()
    }

    #[test]
    fn list_up_keeps_viewport_while_selection_remains_visible() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.set_terminal_size(80, 12);
        app.results = numbered_results(6);
        app.selected_index = 5;
        app.result_scroll_offset = 1;

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        app.handle_key(up, &store);
        assert_eq!(app.selected_index, 4);
        assert_eq!(app.result_list_start(5), 1);

        for _ in 0..3 {
            app.handle_key(up, &store);
        }
        assert_eq!(app.selected_index, 1);
        assert_eq!(app.result_list_start(5), 1);

        app.handle_key(up, &store);
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.result_list_start(5), 0);
    }

    #[test]
    fn mouse_click_selects_visible_session_row() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.set_terminal_size(22, 12);
        app.results = numbered_results(6);

        app.handle_mouse_down(2, 6, &store);

        assert!(app.panel_focus == PanelFocus::SessionList);
        assert_eq!(app.selected_index, 1);
    }

    #[test]
    fn mouse_click_selects_preview_message() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.set_terminal_size(80, 12);
        app.results = numbered_results(1);
        app.preview_messages =
            vec![message(Role::User, None, 0), message(Role::Assistant, None, 1)];

        app.handle_mouse_down(40, 8, &store);

        assert!(app.panel_focus == PanelFocus::Preview);
        assert_eq!(app.preview_selected_msg, 1);
    }

    #[test]
    fn viewing_selection_anchors_scroll_offset() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.set_terminal_size(80, 10);
        app.mode = AppMode::Viewing;
        app.viewing_messages = (0..5).map(|n| message(Role::User, None, n)).collect();
        app.viewing_sanitized_lines = build_viewing_caches(&app.viewing_messages);

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);

        app.handle_key(down, &store);
        assert_eq!((app.viewing_selected_msg, app.viewing_scroll_offset), (1, 0));

        app.handle_key(down, &store);
        assert_eq!((app.viewing_selected_msg, app.viewing_scroll_offset), (2, 2));

        app.handle_key(down, &store);
        assert_eq!((app.viewing_selected_msg, app.viewing_scroll_offset), (3, 5));

        app.handle_key(up, &store);
        assert_eq!((app.viewing_selected_msg, app.viewing_scroll_offset), (2, 5));
    }

    #[test]
    fn enter_viewing_seeds_search_query_and_jumps_to_first_match() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        let result = codex_search_result();
        store.insert_session(&result.session).unwrap();
        let mut first = message(Role::User, None, 0);
        first.content = "intro".to_string();
        let mut second = message(Role::Assistant, None, 1);
        second.content = "Deploy finished".to_string();
        store.insert_messages(&[first, second]).unwrap();
        app.results = vec![result];
        app.query = "deploy missing".to_string();

        app.enter_viewing(&store);

        assert!(matches!(app.mode, AppMode::Viewing));
        assert_eq!(app.viewing_search_query, "deploy missing");
        assert_eq!(app.viewing_match_indices(), &[1]);
        assert_eq!(app.viewing_selected_msg, 1);
    }

    #[test]
    fn enter_viewing_strips_punctuation_from_seeded_query_like_fts() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        let result = codex_search_result();
        store.insert_session(&result.session).unwrap();
        let mut first = message(Role::User, None, 0);
        first.content = "intro".to_string();
        let mut second = message(Role::Assistant, None, 1);
        second.content = "run deploy(now) with config.yaml".to_string();
        store.insert_messages(&[first, second]).unwrap();
        app.results = vec![result];
        app.query = "deploy() config.yaml".to_string();

        app.enter_viewing(&store);

        assert_eq!(app.viewing_search_query, "deploy config yaml");
        assert_eq!(app.viewing_match_indices(), &[1]);
        assert_eq!(app.viewing_selected_msg, 1);
    }

    #[test]
    fn enter_viewing_with_blank_query_starts_at_first_message() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        let result = codex_search_result();
        store.insert_session(&result.session).unwrap();
        store.insert_messages(&[message(Role::User, None, 0)]).unwrap();
        app.results = vec![result];
        app.query = "   ".to_string();

        app.enter_viewing(&store);

        assert!(matches!(app.mode, AppMode::Viewing));
        assert!(app.viewing_search_query.is_empty());
        assert!(app.viewing_match_indices().is_empty());
        assert_eq!(app.viewing_selected_msg, 0);
    }

    #[test]
    fn viewing_session_summary_counts_messages_duration_and_tokens() {
        let messages = vec![
            message(Role::User, Some(0), 0),
            message(Role::Assistant, Some(120_000), 1),
            message(Role::User, None, 2),
        ];
        let usage_events = vec![usage_event(10, 5), usage_event(-1, 4)];

        let summary = ViewingSessionSummary::from_session(&messages, None, &usage_events);

        assert_eq!(summary.user_messages, 2);
        assert_eq!(summary.total_messages, 3);
        assert_eq!(summary.duration_minutes, Some(2));
        assert_eq!(summary.usage_events, 2);
        assert_eq!(summary.tokens.input_tokens, 10);
        assert_eq!(summary.tokens.output_tokens, 9);
        assert_eq!(summary.tokens.cache_read_tokens, 6);
        assert_eq!(summary.tokens.cache_write_tokens, 4);
        assert_eq!(summary.tokens.reasoning_tokens, 2);
        assert_eq!(summary.tokens.total_tokens, 31);
    }

    #[test]
    fn ctrl_o_from_search_confirms_codex_app_open() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.results = vec![codex_search_result()];

        app.handle_search_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), &store);

        assert!(matches!(app.mode, AppMode::ConfirmResume));
        let pending = app.pending_resume.as_ref().unwrap();
        assert!(matches!(pending.action, PendingCommandAction::OpenApp));
        assert!(
            pending
                .command
                .args
                .iter()
                .any(|arg| arg == "codex://threads/019e6d8d-588b-7fd2-a326-c525469ed120")
        );
    }

    #[test]
    fn imported_session_suppresses_resume_and_app_open() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        let mut result = codex_search_result();
        result.session.is_import = true;
        app.results = vec![result];

        for key_char in ['r', 'o'] {
            app.status_message = None;
            app.handle_search_key(
                KeyEvent::new(KeyCode::Char(key_char), KeyModifiers::CONTROL),
                &store,
            );

            assert!(
                !matches!(app.mode, AppMode::ConfirmResume),
                "ctrl+{key_char} must not open confirmation for imported session"
            );
            assert!(app.pending_resume.is_none());
            assert!(
                app.status_message.as_deref().unwrap_or_default().contains("Imported"),
                "status message must explain why nothing happened"
            );
        }
    }

    #[test]
    fn imported_session_can_handoff_from_detail_view() {
        let mut app = app_with_sources();
        let mut result = codex_search_result();
        result.session.is_import = true;
        app.results = vec![result];
        app.viewing_messages = vec![message(Role::User, None, 0)];
        app.mode = AppMode::Viewing;

        app.handle_viewing_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(matches!(app.mode, AppMode::HandoffTarget));

        app.handle_handoff_target_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(app.mode, AppMode::ConfirmResume));
        let pending = app.pending_resume.as_ref().unwrap();
        assert_eq!(pending.action, PendingCommandAction::Handoff);
        assert_eq!(pending.command.program, "codex");
        assert!(pending.command.args[0].contains("This is a handoff, not a native resume."));
    }

    #[test]
    fn confirming_source_picker_preserves_existing_multi_source_selection() {
        let mut app = app_with_sources();
        app.source_filter_selection = vec!["claude".to_string(), "cursor".to_string()];

        app.open_source_picker();
        app.commit_source_picker_filter();

        assert_eq!(app.source_filter_selection, vec!["claude".to_string(), "cursor".to_string()]);
    }

    #[test]
    fn project_picker_filters_by_path_tokens() {
        let mut app = app_with_sources();
        app.project_directories = vec![
            ProjectDirectory {
                directory: "/Users/x/git/samzong/Recall".to_string(),
                sessions: 10,
                last_seen: 2,
            },
            ProjectDirectory {
                directory: "/Users/x/git/openclaw".to_string(),
                sessions: 20,
                last_seen: 1,
            },
        ];
        app.project_picker_query = "sam recall".to_string();

        let rows = app.project_picker_rows();

        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0], ProjectPickerRow::Project(0)));
    }

    #[test]
    fn repo_filter_for_dir_resolves_remote_repo() {
        let root =
            std::env::temp_dir().join(format!("recall-repo-filter-{}", uuid::Uuid::new_v4()));
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        Command::new("git").arg("init").current_dir(&root).output().unwrap();
        Command::new("git")
            .args(["remote", "add", "origin", "git@github.com:samzong/Recall.git"])
            .current_dir(&root)
            .output()
            .unwrap();

        let resolved = repo_filter_for_dir(&nested);

        assert_eq!(resolved, Some(RepoFilter::Remote("github.com/samzong/Recall".to_string())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_filters_include_repo_filter() {
        let mut app = app_with_sources();
        app.repo_filter = Some(RepoFilter::Remote("github.com/samzong/Recall".to_string()));

        let filters = app.search_filters();

        assert_eq!(filters.directory, None);
        assert_eq!(filters.repo, app.repo_filter);
    }

    #[test]
    fn project_picker_space_toggles_pending_selection_without_committing() {
        let mut app = app_with_sources();
        app.project_directories = vec![ProjectDirectory {
            directory: "/Users/x/git/samzong/Recall".to_string(),
            sessions: 10,
            last_seen: 2,
        }];
        app.project_picker_query = "recall".to_string();

        app.toggle_project_picker_row();

        assert_eq!(app.project_picker_selection, Some("/Users/x/git/samzong/Recall".to_string()));
        assert!(app.project_picker_dirty);
        assert_eq!(app.project_filter, None);
    }

    #[test]
    fn source_picker_space_toggles_while_filtering() {
        let mut app = app_with_sources();
        app.source_picker_query = "cod".to_string();
        app.source_picker_typing = true;

        app.toggle_source_picker_row();
        app.commit_source_picker_filter();

        assert_eq!(app.draft_source_filter_selection, vec!["codex".to_string()]);
        assert!(app.source_filter_selection.is_empty());
    }

    #[test]
    fn app_defaults_to_newest_sort() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let app = App::new(&store, vec![source("codex", "Codex")], AppConfig::default());

        assert_eq!(app.sort_order, SortOrder::Newest);
    }

    #[test]
    fn clear_filters_restores_newest_sort() {
        let mut app = app_with_sources();
        app.sort_order = SortOrder::Relevance;
        app.open_filters();

        app.clear_filters();

        assert_eq!(app.sort_order, SortOrder::Relevance);
        assert_eq!(app.draft_sort_order, SortOrder::Newest);
        assert!(app.filters_dirty);
    }

    #[test]
    fn newest_sort_uses_latest_activity() {
        let app = app_with_sources();
        let mut results = vec![
            search_result_with_times("stale-newer-start", 900, Some(900)),
            search_result_with_times("active-older-start", 100, Some(1000)),
        ];

        app.apply_sort(&mut results);

        assert_eq!(results[0].session.source_id, "active-older-start");
        assert_eq!(results[1].session.source_id, "stale-newer-start");
    }

    #[test]
    fn stale_search_response_does_not_replace_current_results() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.query = "current".to_string();
        app.results = vec![search_result_with_times("current-result", 100, None)];
        app.active_search_id = 2;

        app.apply_search_response(
            &store,
            SearchResponse {
                id: 1,
                query: "old".to_string(),
                phase: SearchPhase::Text,
                result: Ok(vec![search_result_with_times("old-result", 200, None)]),
            },
        );

        assert_eq!(app.results[0].session.source_id, "current-result");
    }

    #[test]
    fn text_search_response_keeps_semantic_refinement_pending() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.query = "parser".to_string();
        app.active_search_id = 1;
        app.semantic_progress.done_sessions = 1;

        app.apply_search_response(
            &store,
            SearchResponse {
                id: 1,
                query: "parser".to_string(),
                phase: SearchPhase::Text,
                result: Ok(vec![search_result_with_times("fts-result", 100, None)]),
            },
        );

        assert_eq!(app.results[0].session.source_id, "fts-result");
        assert!(app.search_in_flight);
        assert_eq!(app.search_feedback.as_deref(), Some("Refining semantic results..."));
    }

    #[test]
    fn filter_time_range_left_right_defers_search_until_esc() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.mode = AppMode::Filters;
        app.query = "parser".to_string();
        app.filter_focus = FilterFocus::Time;

        app.handle_filters_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &store);

        assert_eq!(app.time_filter, TimeRange::All);
        assert!(app.filters_dirty);
        assert!(!app.search_pending);
        assert!(matches!(app.mode, AppMode::Filters));

        app.handle_filters_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &store);

        assert_eq!(app.time_filter, TimeRange::Today);
        assert!(matches!(app.mode, AppMode::Search));
        assert!(app.search_pending);
        assert_eq!(app.search_feedback.as_deref(), Some("Filters queued..."));
    }

    #[test]
    fn filter_sort_left_right_defers_search_until_esc() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.mode = AppMode::Filters;
        app.query = "parser".to_string();
        app.filter_focus = FilterFocus::Sort;

        app.handle_filters_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &store);

        assert_eq!(app.sort_order, SortOrder::Newest);
        assert!(app.filters_dirty);
        assert!(!app.search_pending);

        app.handle_filters_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &store);

        assert_eq!(app.sort_order, SortOrder::Relevance);
        assert!(matches!(app.mode, AppMode::Search));
        assert!(app.search_pending);
        assert_eq!(app.search_feedback.as_deref(), Some("Filters queued..."));
    }

    #[test]
    fn filter_esc_closes_without_syncing_results_or_stats() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.mode = AppMode::Filters;
        app.filter_focus = FilterFocus::Time;
        app.results = vec![search_result_with_times("existing", 100, None)];
        app.total_sessions = 42;
        app.total_messages = 99;

        app.handle_filters_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &store);
        app.handle_filters_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &store);

        assert!(matches!(app.mode, AppMode::Search));
        assert_eq!(app.results[0].session.source_id, "existing");
        assert_eq!(app.total_sessions, 42);
        assert_eq!(app.total_messages, 99);
        assert!(app.search_pending);
        assert_eq!(app.search_feedback.as_deref(), Some("Filters queued..."));
    }

    #[test]
    fn filter_commit_invalidates_in_flight_search_response() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app = app_with_sources();
        app.query = "parser".to_string();
        app.mode = AppMode::Filters;
        app.filter_focus = FilterFocus::Time;
        app.results = vec![search_result_with_times("current-result", 100, None)];
        app.search_request_id = 1;
        app.active_search_id = 1;
        app.search_in_flight = true;

        app.handle_filters_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &store);
        app.handle_filters_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &store);
        app.apply_search_response(
            &store,
            SearchResponse {
                id: 1,
                query: "parser".to_string(),
                phase: SearchPhase::Text,
                result: Ok(vec![search_result_with_times("old-filter-result", 200, None)]),
            },
        );

        assert_eq!(app.results[0].session.source_id, "current-result");
        assert!(app.search_pending);
    }

    #[test]
    fn applying_project_picker_returns_to_filter_overview_without_searching() {
        let mut app = app_with_sources();
        app.mode = AppMode::Filters;
        app.filters_editing_project = true;
        app.project_picker_selection = Some("/Users/x/git/samzong/Recall".to_string());
        app.project_picker_dirty = true;

        app.apply_project_picker();

        assert!(matches!(app.mode, AppMode::Filters));
        assert!(!app.filters_editing_project);
        assert_eq!(app.project_filter, None);
        assert_eq!(app.draft_project_filter, Some("/Users/x/git/samzong/Recall".to_string()));
        assert!(app.filters_dirty);
        assert!(!app.search_pending);
    }

    #[test]
    fn applying_source_picker_returns_to_filter_overview_without_searching() {
        let mut app = app_with_sources();
        app.mode = AppMode::Filters;
        app.filters_editing_source = true;
        app.source_picker_selection = vec!["codex".to_string()];
        app.source_picker_dirty = true;

        app.apply_source_picker();

        assert!(matches!(app.mode, AppMode::Filters));
        assert!(!app.filters_editing_source);
        assert!(app.source_filter_selection.is_empty());
        assert_eq!(app.draft_source_filter_selection, vec!["codex".to_string()]);
        assert!(app.filters_dirty);
        assert!(!app.search_pending);
    }
}
