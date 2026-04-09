use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;
use crate::types::{MatchSource, Message, Role, SearchResult};

pub enum AppMode {
    Search,
    Viewing,
    ExportInput,
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
    pub available_sources: Vec<(String, String)>,
    pub source_filter_index: usize,
    pub time_filter: TimeRange,
    pub filter_focus: FilterFocus,
    pub should_quit: bool,
    pub last_keystroke: Instant,
    pub search_pending: bool,
    pub embedding_init_pending: bool,
    pub status_message: Option<String>,
    pub sort_order: SortOrder,
    pub export_path: String,
    pub export_cursor: usize,
    pub total_sessions: u64,
    pub total_messages: u64,
}

impl App {
    pub fn new(store: &Store, available_sources: Vec<(String, String)>) -> Self {
        let recent = store.list_recent_sessions(200).unwrap_or_default();
        let results: Vec<SearchResult> = recent
            .into_iter()
            .map(|session| SearchResult { session, match_source: MatchSource::Fts, snippet: None })
            .collect();

        let (total_sessions, total_messages) = store.stats().unwrap_or((0, 0));

        let mut app = Self {
            mode: AppMode::Search,
            panel_focus: PanelFocus::SessionList,
            query: String::new(),
            cursor_pos: 0,
            results,
            selected_index: 0,
            preview_messages: Vec::new(),
            preview_selected_msg: 0,
            viewing_messages: Vec::new(),
            viewing_selected_msg: 0,
            available_sources,
            source_filter_index: 0,
            time_filter: TimeRange::All,
            filter_focus: FilterFocus::Source,
            should_quit: false,
            last_keystroke: Instant::now(),
            search_pending: false,
            embedding_init_pending: false,
            status_message: None,
            sort_order: SortOrder::Relevance,
            export_path: String::new(),
            export_cursor: 0,
            total_sessions,
            total_messages,
        };
        app.load_preview(store);
        app
    }

    pub fn source_filter(&self) -> Option<&str> {
        if self.source_filter_index == 0 {
            None
        } else {
            Some(&self.available_sources[self.source_filter_index - 1].0)
        }
    }

    pub fn source_filter_label(&self) -> &str {
        if self.source_filter_index == 0 {
            "ALL"
        } else {
            &self.available_sources[self.source_filter_index - 1].1
        }
    }

    pub fn source_label_for<'a>(&'a self, source_id: &'a str) -> &'a str {
        self.available_sources
            .iter()
            .find(|(id, _)| id == source_id)
            .map(|(_, label)| label.as_str())
            .unwrap_or(source_id)
    }

    pub fn load_recent(&mut self, store: &Store) {
        let recent = store.list_recent_sessions(200).unwrap_or_default();
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
            AppMode::Viewing => self.handle_viewing_key(key),
            AppMode::ExportInput => self.handle_export_key(key),
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

    pub fn try_search(
        &mut self,
        store: &Store,
        engine: &SearchEngine,
        provider: &mut Option<EmbeddingProvider>,
    ) {
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

        if provider.is_none() && !self.embedding_init_pending {
            self.status_message = Some("Loading embedding model...".to_string());
            self.embedding_init_pending = true;
            return;
        }
        if self.embedding_init_pending {
            self.embedding_init_pending = false;
            match EmbeddingProvider::new(false) {
                Ok(p) => {
                    *provider = Some(p);
                    self.status_message = None;
                }
                Err(_) => {
                    self.status_message =
                        Some("Embedding model failed — using text search only".to_string());
                }
            }
        }
        let embedding = provider
            .as_ref()
            .and_then(|p| p.embed_query(&[query]).ok())
            .and_then(|mut e| if e.is_empty() { None } else { Some(e.swap_remove(0)) });

        let filters = SearchFilters {
            source: self.source_filter().map(|s| s.to_string()),
            time_range: self.time_filter,
            directory: None,
        };

        match engine.hybrid_search(query, embedding.as_deref(), &filters, 200) {
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
                    (self.source_filter_index + 1) % (self.available_sources.len() + 1);
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
            self.do_search(store, engine, provider);
        }
    }
}
