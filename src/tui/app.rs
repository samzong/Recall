use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::AppConfig;
use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;
use crate::types::{
    BackgroundJobStatus, MatchSource, Message, Role, SearchResult, SemanticProgress,
};

pub enum AppMode {
    Search,
    Viewing,
    ExportInput,
    Settings,
}

#[derive(PartialEq)]
pub enum PanelFocus {
    SessionList,
    Preview,
}

pub enum FilterFocus {
    Source,
    Time,
    Sort,
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
    pub source_filter_index: usize,
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
            source_filter_index: 0,
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
        };
        app.reset_search_defaults();
        app.update_scope_metrics(store);
        app.load_recent(store);
        app
    }

    pub fn source_filter_ids(&self) -> Option<Vec<String>> {
        let enabled = self.enabled_sources();
        if enabled.is_empty() {
            return None;
        }
        if self.source_filter_index == 0 {
            if enabled.len() == self.all_sources.len() {
                None
            } else {
                Some(enabled.into_iter().map(|(id, _)| id.clone()).collect())
            }
        } else {
            enabled.get(self.source_filter_index - 1).map(|(id, _)| vec![id.clone()])
        }
    }

    pub fn source_filter_label(&self) -> &str {
        if self.source_filter_index == 0 {
            if self.enabled_sources().len() == self.all_sources.len() { "ALL" } else { "DEFAULT" }
        } else {
            self.enabled_sources()
                .get(self.source_filter_index - 1)
                .map(|(_, label)| label.as_str())
                .unwrap_or("ALL")
        }
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
        let recent = store.list_recent_sessions(200).unwrap_or_default();
        self.results = recent
            .into_iter()
            .filter(|session| self.session_matches_filters(session, source_ids.as_deref()))
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
            AppMode::Viewing => self.handle_viewing_key(key),
            AppMode::ExportInput => self.handle_export_key(key),
            AppMode::Settings => self.handle_settings_key(key, store, engine, provider),
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
            AppMode::Viewing => {
                if self.viewing_selected_msg > 0 {
                    self.viewing_selected_msg -= 1;
                }
            }
            AppMode::Settings => {
                if self.settings_selected > 0 {
                    self.settings_selected -= 1;
                }
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
            AppMode::Viewing => {
                if self.viewing_selected_msg + 1 < self.viewing_messages.len() {
                    self.viewing_selected_msg += 1;
                }
            }
            AppMode::Settings => {
                if self.settings_selected + 1 < self.settings_row_count() {
                    self.settings_selected += 1;
                }
            }
            _ => {}
        }
    }

    fn handle_search_key(
        &mut self,
        key: KeyEvent,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.mode = AppMode::Settings;
            self.settings_selected = 0;
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
            KeyCode::Backspace if self.panel_focus == PanelFocus::SessionList => {
                if self.cursor_pos > 0 {
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
            KeyCode::Right => {
                if self.panel_focus == PanelFocus::SessionList {
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
            }
            KeyCode::Up => {
                self.handle_scroll_up(store);
            }
            KeyCode::Down => {
                self.handle_scroll_down(store);
            }
            KeyCode::Enter => {
                if !self.results.is_empty() {
                    self.enter_viewing(store);
                }
            }
            KeyCode::Tab => {
                self.cycle_filter(store, engine, provider);
            }
            _ => {}
        }
    }

    fn handle_viewing_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Search;
                self.viewing_messages.clear();
                self.viewing_selected_msg = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.viewing_selected_msg > 0 {
                    self.viewing_selected_msg -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.viewing_selected_msg + 1 < self.viewing_messages.len() {
                    self.viewing_selected_msg += 1;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.viewing_selected_msg = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.viewing_messages.is_empty() {
                    self.viewing_selected_msg = self.viewing_messages.len() - 1;
                }
            }
            KeyCode::Char('c') => {
                self.copy_current_message();
            }
            KeyCode::Char('e') => {
                self.start_export();
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
            directory: None,
        };

        match engine.hybrid_search(query, embedding, &filters, 200) {
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
        if let Ok((sessions, messages)) =
            store.stats_for_scope(self.source_filter_ids().as_deref(), self.time_filter)
        {
            self.total_sessions = sessions;
            self.total_messages = messages;
        }
        if let Ok(progress) =
            store.semantic_progress_for_scope(self.source_filter_ids().as_deref(), self.time_filter)
        {
            self.semantic_progress = progress;
        }
        if let Ok(status) = store.background_job_status("pipeline") {
            self.background_status = status;
        }
    }

    fn enabled_sources(&self) -> Vec<&(String, String)> {
        self.all_sources.iter().filter(|(id, _)| self.config.is_source_enabled(id)).collect()
    }

    fn reset_search_defaults(&mut self) {
        self.source_filter_index = 0;
        self.time_filter = self.config.sync_window.to_time_range();
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
                if self.config.enabled_sources.len() == 1 {
                    self.status_message = Some("At least one source must stay enabled".to_string());
                    return;
                }
                self.config.enabled_sources.retain(|id| id != source_id);
            } else {
                self.config.enabled_sources.push(source_id.clone());
                self.config.enabled_sources.sort();
                self.config.enabled_sources.dedup();
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

    fn session_matches_filters(
        &self,
        session: &crate::types::Session,
        sources: Option<&[String]>,
    ) -> bool {
        if let Some(sources) = sources
            && !sources.iter().any(|source| source == &session.source)
        {
            return false;
        }

        match self.time_filter.millis_ago() {
            Some(min_ts) => session.started_at >= min_ts,
            None => true,
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
            self.viewing_messages = msgs;
            self.viewing_selected_msg = 0;
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

        let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
        self.export_path = format!("{home}/recall-{source}-{safe_title}.txt");
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
            KeyCode::Backspace => {
                if self.export_cursor > 0 {
                    let prev = self.export_path[..self.export_cursor]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.export_path.replace_range(prev..self.export_cursor, "");
                    self.export_cursor = prev;
                }
            }
            KeyCode::Left => {
                if self.export_cursor > 0 {
                    self.export_cursor = self.export_path[..self.export_cursor]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            KeyCode::Right => {
                if self.export_cursor < self.export_path.len() {
                    self.export_cursor = self.export_path[self.export_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.export_cursor + i)
                        .unwrap_or(self.export_path.len());
                }
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
            results.sort_by(|a, b| b.session.started_at.cmp(&a.session.started_at));
        }
    }

    fn cycle_filter(
        &mut self,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
        match self.filter_focus {
            FilterFocus::Source => {
                self.source_filter_index =
                    (self.source_filter_index + 1) % (self.enabled_sources().len() + 1);
                self.filter_focus = FilterFocus::Time;
            }
            FilterFocus::Time => {
                self.time_filter = match self.time_filter {
                    TimeRange::All => TimeRange::Today,
                    TimeRange::Today => TimeRange::Week,
                    TimeRange::Week => TimeRange::Month,
                    TimeRange::Month => TimeRange::All,
                };
                self.filter_focus = FilterFocus::Sort;
            }
            FilterFocus::Sort => {
                self.sort_order = match self.sort_order {
                    SortOrder::Relevance => SortOrder::Newest,
                    SortOrder::Newest => SortOrder::Relevance,
                };
                self.filter_focus = FilterFocus::Source;
            }
        }
        if !self.query.is_empty() {
            self.update_scope_metrics(store);
            self.do_search(store, engine, provider);
        } else {
            self.update_scope_metrics(store);
            self.load_recent(store);
        }
    }
}
