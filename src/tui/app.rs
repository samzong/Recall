use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::adapters::{ResumeCommand, app_command_for, resume_command_for};
use crate::config::AppConfig;
use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::{ProjectDirectory, Store};
use crate::embedding::EmbeddingProvider;
use crate::skill_audit::{self, SkillAuditFilters, SkillAuditReport};
use crate::types::{
    BackgroundJobStatus, MatchSource, Message, Role, SearchResult, SemanticProgress,
};
use crate::usage::{self, UsageFilters, UsageReport};

const USAGE_LOADING_MIN_MS: u128 = 75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageTab {
    Tokens,
    Skills,
}

pub enum AppMode {
    Search,
    Usage,
    Viewing,
    ExportInput,
    Settings,
    Filters,
    ConfirmResume,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, label: &str) -> (String, String) {
        (id.to_string(), label.to_string())
    }

    fn app_with_sources() -> App {
        App {
            mode: AppMode::Search,
            panel_focus: PanelFocus::SessionList,
            query: String::new(),
            cursor_pos: 0,
            results: Vec::new(),
            selected_index: 0,
            preview_messages: Vec::new(),
            preview_selected_msg: 0,
            viewing_messages: Vec::new(),
            viewing_selected_msg: 0,
            all_sources: vec![
                source("claude", "Claude"),
                source("cursor", "Cursor"),
                source("codex", "Codex"),
            ],
            config: AppConfig::default(),
            source_filter_selection: Vec::new(),
            project_directories: Vec::new(),
            project_filter: None,
            time_filter: TimeRange::All,
            filter_focus: FilterFocus::Source,
            should_quit: false,
            last_keystroke: Instant::now(),
            search_pending: false,
            embedding_init_pending: false,
            embedding_unavailable: false,
            status_message: None,
            sort_order: SortOrder::Relevance,
            export_path: String::new(),
            export_cursor: 0,
            total_sessions: 0,
            total_messages: 0,
            semantic_progress: SemanticProgress::default(),
            background_status: BackgroundJobStatus::default(),
            semantic_last_refresh: Instant::now(),
            settings_selected: 0,
            pending_resume: None,
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
                started_at: 0,
                updated_at: None,
                message_count: 1,
                entrypoint: None,
            },
            match_source: MatchSource::Fts,
            snippet: None,
        }
    }

    #[test]
    fn ctrl_o_from_search_confirms_codex_app_open() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let engine = SearchEngine::new(&store.conn);
        let mut provider = None;
        let mut app = app_with_sources();
        app.results = vec![codex_search_result()];

        app.handle_search_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &store,
            &engine,
            &mut provider,
        );

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

        assert_eq!(app.source_filter_selection, vec!["codex".to_string()]);
    }

    #[test]
    fn applying_project_picker_closes_filters_window() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let engine = SearchEngine::new(&store.conn);
        let mut provider = None;
        let mut app = app_with_sources();
        app.mode = AppMode::Filters;
        app.filters_editing_project = true;
        app.project_picker_selection = Some("/Users/x/git/samzong/Recall".to_string());
        app.project_picker_dirty = true;

        app.apply_project_picker(&store, &engine, &mut provider);

        assert!(matches!(app.mode, AppMode::Search));
        assert!(!app.filters_editing_project);
        assert_eq!(app.project_filter, Some("/Users/x/git/samzong/Recall".to_string()));
    }

    #[test]
    fn applying_source_picker_closes_filters_window() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let engine = SearchEngine::new(&store.conn);
        let mut provider = None;
        let mut app = app_with_sources();
        app.mode = AppMode::Filters;
        app.filters_editing_source = true;
        app.source_picker_selection = vec!["codex".to_string()];
        app.source_picker_dirty = true;

        app.apply_source_picker(&store, &engine, &mut provider);

        assert!(matches!(app.mode, AppMode::Search));
        assert!(!app.filters_editing_source);
        assert_eq!(app.source_filter_selection, vec!["codex".to_string()]);
    }
}

#[derive(Clone, Copy)]
pub enum ResumeOrigin {
    Search,
    Viewing,
}

#[derive(Clone, Copy)]
pub enum PendingCommandAction {
    Resume,
    OpenApp,
}

pub struct PendingResume {
    pub command: ResumeCommand,
    pub action: PendingCommandAction,
    pub source_label: String,
    pub session_title: String,
    pub cwd: Option<String>,
    pub origin: ResumeOrigin,
}

pub struct SanitizedLine {
    pub text: String,
    pub lower: String,
}

fn build_viewing_caches(msgs: &[Message]) -> Vec<Vec<SanitizedLine>> {
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

#[derive(PartialEq)]
pub enum PanelFocus {
    SessionList,
    Preview,
}

#[derive(Clone, Copy, PartialEq)]
pub enum FilterFocus {
    Source,
    Project,
    Time,
    Sort,
}

impl FilterFocus {
    fn next(self) -> Self {
        match self {
            Self::Source => Self::Project,
            Self::Project => Self::Time,
            Self::Time => Self::Sort,
            Self::Sort => Self::Source,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Source => Self::Sort,
            Self::Project => Self::Source,
            Self::Time => Self::Project,
            Self::Sort => Self::Time,
        }
    }
}

#[derive(Clone, Copy)]
pub enum SourcePickerRow {
    All,
    Source(usize),
}

#[derive(Clone, Copy)]
pub enum ProjectPickerRow {
    All,
    Project(usize),
}

#[derive(Clone, Copy, PartialEq)]
pub enum SortOrder {
    Relevance,
    Newest,
}

pub struct App {
    pub mode: AppMode,
    pub panel_focus: PanelFocus,
    pub query: String,
    pub cursor_pos: usize,
    pub results: Vec<SearchResult>,
    pub selected_index: usize,
    pub preview_messages: Vec<Message>,
    pub preview_selected_msg: usize,
    pub viewing_messages: Vec<Message>,
    pub viewing_selected_msg: usize,
    pub all_sources: Vec<(String, String)>,
    pub config: AppConfig,
    pub source_filter_selection: Vec<String>,
    pub time_filter: TimeRange,
    pub filter_focus: FilterFocus,
    pub should_quit: bool,
    pub last_keystroke: Instant,
    pub search_pending: bool,
    pub embedding_init_pending: bool,
    pub embedding_unavailable: bool,
    pub status_message: Option<String>,
    pub sort_order: SortOrder,
    pub export_path: String,
    pub export_cursor: usize,
    pub total_sessions: u64,
    pub total_messages: u64,
    pub semantic_progress: SemanticProgress,
    pub background_status: BackgroundJobStatus,
    pub semantic_last_refresh: Instant,
    pub settings_selected: usize,
    pub pending_resume: Option<PendingResume>,
    pub exec_on_exit: Option<(ResumeCommand, Option<String>)>,
    pub viewing_search_query: String,
    pub viewing_search_input: Option<String>,
    pub viewing_search_input_cursor: usize,
    pub viewing_search_status: Option<String>,
    pub viewing_sanitized_lines: Vec<Vec<SanitizedLine>>,
    pub viewing_match_cache: Vec<usize>,
    pub source_picker_query: String,
    pub source_picker_cursor: usize,
    pub source_picker_selected: usize,
    pub source_picker_selection: Vec<String>,
    pub source_picker_dirty: bool,
    pub source_picker_typing: bool,
    pub filters_editing_source: bool,
    pub project_directories: Vec<ProjectDirectory>,
    pub project_filter: Option<String>,
    pub project_picker_query: String,
    pub project_picker_cursor: usize,
    pub project_picker_selected: usize,
    pub project_picker_selection: Option<String>,
    pub project_picker_dirty: bool,
    pub project_picker_typing: bool,
    pub filters_editing_project: bool,
    pub usage_report: Option<UsageReport>,
    pub usage_year_report: Option<UsageReport>,
    pub usage_error: Option<String>,
    pub usage_time_filter: TimeRange,
    pub usage_refresh_requested_at: Option<Instant>,
    pub usage_breakdown_scroll: u16,
    pub usage_tab: UsageTab,
    pub skill_audit_report: Option<SkillAuditReport>,
    pub skill_audit_error: Option<String>,
    pub skill_audit_selected: usize,
}

impl App {
    pub fn new(store: &Store, all_sources: Vec<(String, String)>, mut config: AppConfig) -> Self {
        config.normalize_sources(&all_sources);

        let (total_sessions, total_messages) = store.stats().unwrap_or((0, 0));
        let semantic_progress = store.semantic_progress().unwrap_or_default();
        let background_status = store.background_job_status("pipeline").unwrap_or_default();

        let mut app = Self {
            mode: AppMode::Search,
            panel_focus: PanelFocus::SessionList,
            query: String::new(),
            cursor_pos: 0,
            results: Vec::new(),
            selected_index: 0,
            preview_messages: Vec::new(),
            preview_selected_msg: 0,
            viewing_messages: Vec::new(),
            viewing_selected_msg: 0,
            all_sources,
            config,
            source_filter_selection: Vec::new(),
            time_filter: TimeRange::All,
            filter_focus: FilterFocus::Source,
            should_quit: false,
            last_keystroke: Instant::now(),
            search_pending: false,
            embedding_init_pending: false,
            embedding_unavailable: false,
            status_message: None,
            sort_order: SortOrder::Relevance,
            export_path: String::new(),
            export_cursor: 0,
            total_sessions,
            total_messages,
            semantic_progress,
            background_status,
            semantic_last_refresh: Instant::now(),
            settings_selected: 0,
            pending_resume: None,
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

    pub fn source_filter_ids(&self) -> Option<Vec<String>> {
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

    pub fn source_filter_label(&self) -> String {
        let explicit = self.normalized_source_selection(&self.source_filter_selection);
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

    pub fn time_filter_label(&self) -> &'static str {
        match self.time_filter {
            TimeRange::Today => "Today",
            TimeRange::Week => "7d",
            TimeRange::Month => "30d",
            TimeRange::All => "All",
        }
    }

    pub fn usage_time_label(&self) -> &'static str {
        match self.usage_time_filter {
            TimeRange::Today => "Today",
            TimeRange::Week => "7d",
            TimeRange::Month => "30d",
            TimeRange::All => "All day",
        }
    }

    pub fn sort_label(&self) -> &'static str {
        match self.sort_order {
            SortOrder::Relevance => "Relevance",
            SortOrder::Newest => "Newest",
        }
    }

    pub fn project_filter_label(&self) -> String {
        self.project_filter
            .as_deref()
            .map(short_project_label)
            .unwrap_or_else(|| "All projects".to_string())
    }

    pub fn source_label_for<'a>(&'a self, source_id: &'a str) -> &'a str {
        self.all_sources
            .iter()
            .find(|(id, _)| id == source_id)
            .map(|(_, label)| label.as_str())
            .unwrap_or(source_id)
    }

    pub fn load_recent(&mut self, store: &Store) {
        let source_ids = self.source_filter_ids();
        let recent = store
            .list_recent_sessions_for_search_scope(
                source_ids.as_deref(),
                self.time_filter,
                self.project_filter.as_deref(),
                200,
            )
            .unwrap_or_default();
        self.results = recent
            .into_iter()
            .map(|session| SearchResult { session, match_source: MatchSource::Fts, snippet: None })
            .collect();
        self.selected_index = 0;
        self.panel_focus = PanelFocus::SessionList;
        self.load_preview(store);
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        self.status_message = None;

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match self.mode {
            AppMode::Search => self.handle_search_key(key, store, engine, provider),
            AppMode::Usage => self.handle_usage_key(key, store),
            AppMode::Viewing => self.handle_viewing_key(key),
            AppMode::ExportInput => self.handle_export_key(key),
            AppMode::Settings => self.handle_settings_key(key, store, engine, provider),
            AppMode::Filters => self.handle_filters_key(key, store, engine, provider),
            AppMode::ConfirmResume => self.handle_confirm_resume_key(key),
        }
    }

    pub fn handle_scroll_up(&mut self, store: &Store) {
        match self.mode {
            AppMode::Search => match self.panel_focus {
                PanelFocus::SessionList => {
                    if !self.results.is_empty() && self.selected_index > 0 {
                        self.selected_index -= 1;
                        self.load_preview(store);
                    }
                }
                PanelFocus::Preview => {
                    if self.preview_selected_msg > 0 {
                        self.preview_selected_msg -= 1;
                    }
                }
            },
            AppMode::Viewing if self.viewing_selected_msg > 0 => {
                self.viewing_selected_msg -= 1;
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

    pub fn handle_scroll_down(&mut self, store: &Store) {
        match self.mode {
            AppMode::Search => match self.panel_focus {
                PanelFocus::SessionList => {
                    if self.selected_index + 1 < self.results.len() {
                        self.selected_index += 1;
                        self.load_preview(store);
                    }
                }
                PanelFocus::Preview => {
                    if self.preview_selected_msg + 1 < self.preview_messages.len() {
                        self.preview_selected_msg += 1;
                    }
                }
            },
            AppMode::Viewing if self.viewing_selected_msg + 1 < self.viewing_messages.len() => {
                self.viewing_selected_msg += 1;
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

    fn handle_search_key(
        &mut self,
        key: KeyEvent,
        store: &Store,
        _engine: &SearchEngine,
        _provider: &mut Option<EmbeddingProvider>,
    ) {
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
                    self.load_recent(store);
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char(c) if self.panel_focus == PanelFocus::SessionList => {
                self.query.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.last_keystroke = Instant::now();
                self.search_pending = true;
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
                self.last_keystroke = Instant::now();
                self.search_pending = true;
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
                self.viewing_search_query.clear();
                self.viewing_search_status = None;
                self.viewing_sanitized_lines.clear();
                self.viewing_match_cache.clear();
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
            KeyCode::Char('c') => {
                self.copy_current_message();
            }
            KeyCode::Char('e') => {
                self.start_export();
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

    fn recompute_viewing_matches(&mut self) {
        self.viewing_match_cache.clear();
        if self.viewing_search_query.is_empty() {
            return;
        }
        let needle = self.viewing_search_query.to_lowercase();
        for (i, msg_lines) in self.viewing_sanitized_lines.iter().enumerate() {
            if msg_lines.iter().any(|l| l.lower.contains(&needle)) {
                self.viewing_match_cache.push(i);
            }
        }
    }

    pub fn viewing_match_indices(&self) -> &[usize] {
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
        self.start_command_confirmation(origin, PendingCommandAction::Resume);
    }

    fn start_app_open_confirmation(&mut self, origin: ResumeOrigin) {
        self.start_command_confirmation(origin, PendingCommandAction::OpenApp);
    }

    fn start_command_confirmation(&mut self, origin: ResumeOrigin, action: PendingCommandAction) {
        let Some(result) = self.results.get(self.selected_index) else {
            return;
        };
        let session = &result.session;
        let command = match action {
            PendingCommandAction::Resume => resume_command_for(&session.source, &session.source_id),
            PendingCommandAction::OpenApp => app_command_for(&session.source, &session.source_id),
        };
        let Some(command) = command else {
            let action_label = match action {
                PendingCommandAction::Resume => "Resume",
                PendingCommandAction::OpenApp => "Open in app",
            };
            self.status_message =
                Some(format!("{action_label} not supported for {}", session.source));
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

    fn handle_settings_key(
        &mut self,
        key: KeyEvent,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Search;
            }
            KeyCode::Up => self.handle_scroll_up(store),
            KeyCode::Down => self.handle_scroll_down(store),
            KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => {
                self.update_setting(store, engine, provider);
            }
            _ => {}
        }
    }

    fn handle_filters_key(
        &mut self,
        key: KeyEvent,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        if self.filters_editing_source {
            self.handle_source_picker_key(key, store, engine, provider);
            return;
        }
        if self.filters_editing_project {
            self.handle_project_picker_key(key, store, engine, provider);
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Search;
            }
            KeyCode::Up => self.handle_scroll_up(store),
            KeyCode::Down => self.handle_scroll_down(store),
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.activate_filter_row(store, engine, provider);
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.clear_filters();
                self.refresh_after_filter_change(store, engine, provider);
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.time_filter = TimeRange::Today;
                self.refresh_after_filter_change(store, engine, provider);
            }
            KeyCode::Char('w') | KeyCode::Char('W') => {
                self.time_filter = TimeRange::Week;
                self.refresh_after_filter_change(store, engine, provider);
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                self.time_filter = TimeRange::Month;
                self.refresh_after_filter_change(store, engine, provider);
            }
            KeyCode::Char('l') | KeyCode::Char('L') => {
                self.time_filter = TimeRange::All;
                self.refresh_after_filter_change(store, engine, provider);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.sort_order = SortOrder::Relevance;
                self.refresh_after_filter_change(store, engine, provider);
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.sort_order = SortOrder::Newest;
                self.refresh_after_filter_change(store, engine, provider);
            }
            _ => {}
        }
    }

    fn activate_filter_row(
        &mut self,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        match self.filter_focus {
            FilterFocus::Source => {
                self.open_source_picker();
            }
            FilterFocus::Project => {
                self.open_project_picker(store);
            }
            FilterFocus::Time => {
                self.time_filter = match self.time_filter {
                    TimeRange::All => TimeRange::Today,
                    TimeRange::Today => TimeRange::Week,
                    TimeRange::Week => TimeRange::Month,
                    TimeRange::Month => TimeRange::All,
                };
                self.refresh_after_filter_change(store, engine, provider);
            }
            FilterFocus::Sort => {
                self.sort_order = match self.sort_order {
                    SortOrder::Relevance => SortOrder::Newest,
                    SortOrder::Newest => SortOrder::Relevance,
                };
                self.refresh_after_filter_change(store, engine, provider);
            }
        }
    }

    fn handle_source_picker_key(
        &mut self,
        key: KeyEvent,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
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
                self.apply_source_picker(store, engine, provider);
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
            self.normalized_source_selection(&self.source_filter_selection);
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

    fn handle_project_picker_key(
        &mut self,
        key: KeyEvent,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
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
                self.apply_project_picker(store, engine, provider);
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
        self.project_picker_selection = self.project_filter.clone();
        self.project_picker_dirty = false;
        self.project_picker_typing = false;
        if let Some(selected_project) = self.project_filter.as_ref() {
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

    fn apply_project_picker(
        &mut self,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        self.commit_project_picker_filter();
        self.close_project_picker();
        self.mode = AppMode::Search;
        self.refresh_after_filter_change(store, engine, provider);
    }

    fn commit_project_picker_filter(&mut self) {
        if self.project_picker_dirty {
            self.project_filter = self.project_picker_selection.clone();
        } else if let Some(row) = self.project_picker_rows().get(self.project_picker_selected) {
            match *row {
                ProjectPickerRow::All => {
                    self.project_filter = None;
                }
                ProjectPickerRow::Project(index) => {
                    if let Some(project) = self.project_directories.get(index) {
                        self.project_filter = Some(project.directory.clone());
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

    fn apply_source_picker(
        &mut self,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        self.commit_source_picker_filter();

        self.source_picker_query.clear();
        self.source_picker_cursor = 0;
        self.source_picker_selected = 0;
        self.source_picker_typing = false;
        self.source_picker_selection.clear();
        self.source_picker_dirty = false;
        self.filters_editing_source = false;
        self.mode = AppMode::Search;
        self.refresh_after_filter_change(store, engine, provider);
    }

    fn commit_source_picker_filter(&mut self) {
        let confirming_existing_multi_selection = !self.source_picker_dirty
            && self.source_picker_query.trim().is_empty()
            && self.source_picker_selection.len() > 1;

        if self.source_picker_dirty || confirming_existing_multi_selection {
            self.source_filter_selection =
                self.normalized_source_selection(&self.source_picker_selection);
        } else if let Some(row) = self.source_picker_rows().get(self.source_picker_selected) {
            match *row {
                SourcePickerRow::All => {
                    self.source_filter_selection.clear();
                }
                SourcePickerRow::Source(index) => {
                    if let Some((source_id, _)) = self.all_sources.get(index) {
                        self.source_filter_selection = vec![source_id.clone()];
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
        self.source_filter_selection.clear();
        self.project_filter = None;
        self.time_filter = TimeRange::All;
        self.sort_order = SortOrder::Relevance;
        self.filter_focus = FilterFocus::Source;
    }

    fn refresh_after_filter_change(
        &mut self,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        self.update_scope_metrics(store);
        self.refresh_usage(store);
        if self.query.is_empty() {
            self.load_recent(store);
        } else {
            self.do_search(store, engine, provider);
        }
    }

    pub fn refresh_usage(&mut self, store: &Store) {
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

    pub fn request_usage_refresh(&mut self) {
        self.usage_error = None;
        self.usage_refresh_requested_at = Some(Instant::now());
    }

    pub fn usage_is_loading(&self) -> bool {
        self.usage_refresh_requested_at.is_some()
    }

    pub fn usage_refresh_is_due(&self) -> bool {
        self.usage_refresh_requested_at
            .map(|requested_at| requested_at.elapsed().as_millis() >= USAGE_LOADING_MIN_MS)
            .unwrap_or(false)
    }

    pub fn fail_usage_refresh(&mut self, error: impl std::fmt::Display) {
        self.usage_refresh_requested_at = None;
        self.usage_report = None;
        self.usage_year_report = None;
        self.skill_audit_report = None;
        self.usage_error = Some(format!("Usage unavailable: {error}"));
        self.skill_audit_error = Some(format!("Skill audit unavailable: {error}"));
    }

    pub fn try_search(
        &mut self,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        self.refresh_semantic_progress(store);
        if self.embedding_init_pending {
            self.do_search(store, engine, provider);
            return;
        }
        if !self.search_pending {
            return;
        }
        if self.last_keystroke.elapsed().as_millis() < 150 {
            return;
        }
        self.search_pending = false;
        self.do_search(store, engine, provider);
    }

    fn do_search(
        &mut self,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        let query = self.query.trim();
        if query.is_empty() {
            self.load_recent(store);
            return;
        }

        if !self.semantic_ready() {
            self.run_search(store, engine, None);
            return;
        }

        if provider.is_none() && !self.embedding_init_pending && !self.embedding_unavailable {
            self.status_message = Some("Loading embedding model...".to_string());
            self.embedding_init_pending = true;
            return;
        }
        if self.embedding_init_pending {
            self.embedding_init_pending = false;
            match EmbeddingProvider::new(false) {
                Ok(p) => {
                    *provider = Some(p);
                    self.embedding_unavailable = false;
                    self.status_message = None;
                }
                Err(_) => {
                    self.embedding_unavailable = true;
                    self.status_message =
                        Some("Semantic unavailable — using text search only".to_string());
                }
            }
        }
        let embedding = provider
            .as_ref()
            .and_then(|p| p.embed_query(&[query]).ok())
            .and_then(|mut e| if e.is_empty() { None } else { Some(e.swap_remove(0)) });

        self.run_search(store, engine, embedding.as_deref());
    }

    fn run_search(&mut self, store: &Store, engine: &SearchEngine, embedding: Option<&[f32]>) {
        let query = self.query.trim();

        let filters = SearchFilters {
            sources: self.source_filter_ids(),
            time_range: self.time_filter,
            directory: self.project_filter.clone(),
        };

        match engine.hybrid_search(query, embedding, &filters, 200, 3) {
            Ok(mut results) => {
                self.apply_sort(&mut results);
                self.results = results;
                self.selected_index = 0;
                self.status_message = None;
            }
            Err(e) => {
                self.status_message = Some(format!("Search error: {e}"));
                self.results.clear();
            }
        }

        self.panel_focus = PanelFocus::SessionList;
        self.load_preview(store);
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
        ) {
            self.total_sessions = sessions;
            self.total_messages = messages;
        }
        if let Ok(progress) = store.semantic_progress_for_search_scope(
            self.source_filter_ids().as_deref(),
            self.time_filter,
            self.project_filter.as_deref(),
        ) {
            self.semantic_progress = progress;
        }
        if let Ok(status) = store.background_job_status("pipeline") {
            self.background_status = status;
        }
    }

    pub fn enabled_sources(&self) -> Vec<&(String, String)> {
        self.all_sources.iter().filter(|(id, _)| self.config.is_source_enabled(id)).collect()
    }

    pub fn source_is_selected(&self, source_id: &str) -> bool {
        self.normalized_source_selection(&self.source_filter_selection)
            .iter()
            .any(|id| id == source_id)
    }

    pub fn source_is_selected_in_picker(&self, source_id: &str) -> bool {
        self.normalized_source_selection(&self.source_picker_selection)
            .iter()
            .any(|id| id == source_id)
    }

    pub fn source_picker_rows(&self) -> Vec<SourcePickerRow> {
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

    pub fn project_picker_rows(&self) -> Vec<ProjectPickerRow> {
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
        self.time_filter = self.config.sync_window.to_time_range();
    }

    fn reset_usage_dashboard(&mut self) {
        self.source_filter_selection.clear();
        self.usage_time_filter = TimeRange::All;
        self.usage_breakdown_scroll = 0;
        self.skill_audit_selected = 0;
    }

    pub fn usage_tab_label(&self) -> &'static str {
        match self.usage_tab {
            UsageTab::Tokens => "tokens",
            UsageTab::Skills => "skills",
        }
    }

    pub fn skill_audit_entry_count(&self) -> usize {
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
        1 + self.all_sources.len()
    }

    fn update_setting(
        &mut self,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        if self.settings_selected == 0 {
            self.config.sync_window = self.config.sync_window.next();
        } else if let Some((source_id, _)) = self.all_sources.get(self.settings_selected - 1) {
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
            self.do_search(store, engine, provider);
        }
    }

    fn load_preview(&mut self, store: &Store) {
        self.preview_selected_msg = 0;
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
            self.viewing_sanitized_lines = build_viewing_caches(&msgs);
            self.viewing_messages = msgs;
            self.viewing_selected_msg = 0;
            self.viewing_search_query.clear();
            self.viewing_search_input = None;
            self.viewing_search_input_cursor = 0;
            self.viewing_search_status = None;
            self.viewing_match_cache.clear();
            self.mode = AppMode::Viewing;
        }
    }

    fn copy_current_message(&mut self) {
        let text = self.viewing_messages.get(self.viewing_selected_msg).map(|m| m.content.clone());
        if let Some(text) = text {
            self.copy_to_clipboard(&text);
        }
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

        let mut content = String::new();
        content.push_str(&format!("Session: {}\n", session.title));
        content.push_str(&format!("Source: {}\n", session.source));
        if let Some(ref dir) = session.directory {
            content.push_str(&format!("Directory: {dir}\n"));
        }
        content.push_str(&format!(
            "Date: {}\n",
            chrono::DateTime::from_timestamp_millis(session.started_at)
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default()
        ));
        content.push_str(&format!("Messages: {}\n", self.viewing_messages.len()));
        content.push_str("\n---\n\n");

        for msg in &self.viewing_messages {
            let role = match msg.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            content.push_str(&format!("## {role}\n\n{}\n\n", msg.content));
        }

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
            results.sort_by_key(|b| std::cmp::Reverse(b.session.started_at));
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
