mod popups;
mod search;
mod usage;
mod viewing;

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::scrollbar;
use ratatui::text::Span;
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::tui::app::App;
use crate::tui::share_state::{AppMode, ResumeOrigin};
use crate::tui::theme::THEME;

pub(super) fn highlight_spans(
    text: &str,
    hay: &str,
    needles: &[String],
    base: Style,
) -> Vec<Span<'static>> {
    if needles.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    if hay.len() != text.len() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    let match_style =
        Style::default().fg(THEME.match_fg).bg(THEME.match_bg).add_modifier(Modifier::BOLD);
    while cursor < text.len() {
        let hit = needles
            .iter()
            .filter(|n| !n.is_empty())
            .filter_map(|n| hay[cursor..].find(n.as_str()).map(|rel| (cursor + rel, n.len())))
            .min_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        match hit {
            Some((start, len)) => {
                let end = start + len;
                if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                    spans.push(Span::styled(text[cursor..].to_string(), base));
                    break;
                }
                if start > cursor {
                    spans.push(Span::styled(text[cursor..start].to_string(), base));
                }
                spans.push(Span::styled(text[start..end].to_string(), match_style));
                cursor = end;
            }
            None => {
                spans.push(Span::styled(text[cursor..].to_string(), base));
                break;
            }
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base));
    }
    spans
}

pub(super) fn row_visible(row: usize, viewport_start: usize, viewport_end: usize) -> bool {
    row >= viewport_start && row < viewport_end
}

pub(super) fn render_vertical_scrollbar(
    f: &mut Frame,
    area: Rect,
    content_len: usize,
    viewport_len: usize,
    position: usize,
) {
    if viewport_len == 0 || content_len <= viewport_len {
        return;
    }

    let max_position = content_len - viewport_len;
    let mut state = ScrollbarState::new(max_position + 1)
        .viewport_content_length(viewport_len)
        .position(position.min(max_position));
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .symbols(scrollbar::VERTICAL)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_symbol("▌")
            .track_symbol(Some("▌"))
            .thumb_style(Style::default().fg(THEME.scrollbar_thumb))
            .track_style(Style::default().fg(THEME.scrollbar_track)),
        area.inner(Margin { vertical: 1, horizontal: 0 }),
        &mut state,
    );
}
pub(super) fn truncate_label(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        return label.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    format!("{}…", label.chars().take(max_chars - 1).collect::<String>())
}

pub(super) fn truncate_start(label: &str, max_chars: usize) -> String {
    let char_count = label.chars().count();
    if char_count <= max_chars {
        return label.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let tail: String = label.chars().skip(char_count - max_chars + 1).collect();
    format!("…{tail}")
}

pub(super) fn format_count(value: usize) -> String {
    format_compact(value as i64)
}

pub(super) fn format_compact(value: i64) -> String {
    let abs = value.abs() as f64;
    if abs >= 1_000_000_000.0 {
        format!("{:.2}B", value as f64 / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}
pub(crate) fn render(f: &mut Frame, app: &App) {
    match app.mode {
        AppMode::Search => search::render_search(f, app),
        AppMode::Usage => usage::render_usage_dashboard(f, app),
        AppMode::Viewing => viewing::render_viewing(f, app),
        AppMode::ShareResult => {
            viewing::render_viewing(f, app);
            popups::render_share_result(f, app);
        }
        AppMode::Filters => {
            search::render_search(f, app);
            search::render_filter_picker(f, app);
        }
        AppMode::HandoffTarget => {
            viewing::render_viewing(f, app);
            popups::render_handoff_target_picker(f, app);
        }
        AppMode::ExportInput => {
            viewing::render_viewing(f, app);
            popups::render_export_input(f, app);
        }
        AppMode::Settings => {
            search::render_search(f, app);
            popups::render_settings(f, app);
        }
        AppMode::ConfirmResume => {
            match app.pending_resume.as_ref().map(|p| p.origin) {
                Some(ResumeOrigin::Viewing) => viewing::render_viewing(f, app),
                _ => search::render_search(f, app),
            }
            popups::render_confirm_resume(f, app);
        }
        AppMode::ConfirmDelete => {
            match app.pending_delete.as_ref().map(|p| p.origin) {
                Some(ResumeOrigin::Viewing) => viewing::render_viewing(f, app),
                _ => search::render_search(f, app),
            }
            popups::render_confirm_delete(f, app);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::config::AppConfig;
    use crate::db::store::Store;
    use crate::tui::share_state::AppMode;
    use crate::tui::share_state::SharePopup;
    use crate::tui::share_state::{PendingDelete, ResumeOrigin};
    use crate::tui::viewing_state::SanitizedLine;
    use crate::tui::viewing_state::ViewingSessionSummary;
    use crate::types::{MatchSource, Message, Role, SearchResult, Session};
    use crate::usage::TokenTotals;

    fn numbered_session_result(n: usize) -> SearchResult {
        SearchResult {
            session: Session {
                id: format!("session{n}"),
                source: "codex".to_string(),
                source_id: format!("source{n}"),
                title: format!("Session {n}"),
                directory: None,
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

    fn render_to_text(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        (0..height)
            .map(|y| buffer_row(terminal.backend().buffer(), y, width))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_scrollbar_thumb_touches_bottom_at_max_position() {
        let backend = TestBackend::new(2, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let frame = terminal
            .draw(|f| {
                render_vertical_scrollbar(f, ratatui::layout::Rect::new(0, 0, 2, 10), 20, 5, 15)
            })
            .unwrap();

        assert_eq!(frame.buffer[(1, 8)].style().fg, Some(THEME.scrollbar_thumb));
    }

    #[test]
    fn highlight_spans_marks_each_query_term() {
        let spans = highlight_spans(
            "Alpha beta Gamma",
            "alpha beta gamma",
            &["alpha".to_string(), "gamma".to_string()],
            Style::default(),
        );

        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["Alpha", " beta ", "Gamma"]);
        assert_eq!(spans[0].style.bg, Some(THEME.match_bg));
        assert_eq!(spans[1].style.bg, None);
        assert_eq!(spans[2].style.bg, Some(THEME.match_bg));
    }

    #[test]
    fn highlight_spans_prefers_longest_term_at_same_position() {
        let spans = highlight_spans(
            "foobar baz",
            "foobar baz",
            &["foo".to_string(), "foobar".to_string()],
            Style::default(),
        );

        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["foobar", " baz"]);
        assert_eq!(spans[0].style.bg, Some(THEME.match_bg));
    }

    #[test]
    fn render_result_list_scrolls_selected_row_into_view() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app =
            App::new(&store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default());
        app.results = (1..=6).map(numbered_session_result).collect();
        app.selected_index = 3;

        let rendered = render_to_text(&app, 80, 10);

        assert!(rendered.contains("Sessions [4/6]"));
        assert!(!rendered.contains("Session 1"));
        assert!(rendered.contains("Session 2"));
        assert!(rendered.contains("Session 4"));
    }

    #[test]
    fn render_result_list_keeps_viewport_when_selection_is_visible() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app =
            App::new(&store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default());
        app.results = (1..=6).map(numbered_session_result).collect();
        app.selected_index = 1;
        app.result_scroll_offset = 1;

        let rendered = render_to_text(&app, 80, 10);

        assert!(!rendered.contains("Session 1"));
        assert!(rendered.contains("Session 2"));
        assert!(rendered.contains("Session 4"));
    }

    #[test]
    fn render_result_list_selected_row_background_fills_interior() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app =
            App::new(&store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default());
        let mut selected = numbered_session_result(1);
        selected.session.title = "--- /tmp/pyrefly_base.txt漢".to_string();
        app.results = vec![selected, numbered_session_result(2)];
        app.selected_index = 0;

        let width = 80;
        let height = 10;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let frame = terminal.draw(|f| render(f, &app)).unwrap();

        let layout =
            crate::tui::layout::search_layout(ratatui::layout::Rect::new(0, 0, width, height));
        let inner = layout.list_inner();
        let buffer = frame.buffer;
        let selected_y = inner.y;
        let unselected_y = inner.y + 1;

        for x in inner.x..inner.x + inner.width {
            assert_eq!(
                buffer[(x, selected_y)].style().bg,
                Some(THEME.selected_bg),
                "selected row x={x} should have the selected background"
            );
        }
        assert!(
            (inner.x..inner.x + inner.width)
                .all(|x| buffer[(x, unselected_y)].style().bg != Some(THEME.selected_bg))
        );
    }

    #[test]
    fn render_viewing_shows_one_line_session_summary_below_title() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app =
            App::new(&store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default());
        app.mode = AppMode::Viewing;
        app.results = vec![SearchResult {
            session: Session {
                id: "session1".to_string(),
                source: "codex".to_string(),
                source_id: "source1".to_string(),
                title: "Test session".to_string(),
                directory: Some("/tmp/repo".to_string()),
                repo_remote: None,
                repo_slug: None,
                repo_name: None,
                started_at: 0,
                updated_at: Some(120_000),
                message_count: 1,
                entrypoint: None,
                custom_title: None,
                summary: None,
                duration_minutes: Some(2),
                source_file_path: None,
                is_import: false,
            },
            match_source: MatchSource::Fts,
            snippet: None,
        }];
        app.viewing_messages = vec![Message {
            session_id: "session1".to_string(),
            role: Role::User,
            content: "hello".to_string(),
            timestamp: Some(0),
            seq: 0,
        }];
        app.viewing_sanitized_lines =
            vec![vec![SanitizedLine { text: "hello".to_string(), lower: "hello".to_string() }]];
        app.viewing_session_summary = Some(ViewingSessionSummary {
            user_messages: 2,
            total_messages: 3,
            duration_minutes: Some(2),
            usage_events: 2,
            tokens: TokenTotals {
                input_tokens: 10,
                output_tokens: 9,
                cache_read_tokens: 6,
                cache_write_tokens: 4,
                reasoning_tokens: 2,
                total_tokens: 31,
            },
            started_calendar: "2024-01-01".to_string(),
        });

        let backend = TestBackend::new(100, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let summary = buffer_row(terminal.backend().buffer(), 1, 100);

        assert!(summary.contains(
            "tokens 31 input 10 output 9 cache r/w 6/4 reasoning 2 | time 2m | user msgs 2/3"
        ));
        assert_eq!(terminal.backend().buffer()[(2, 1)].fg, THEME.summary);
    }

    #[test]
    fn render_confirm_delete_popup_shows_index_only_warning() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app =
            App::new(&store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default());
        app.mode = AppMode::ConfirmDelete;
        app.results = vec![numbered_session_result(1)];
        app.pending_delete = Some(PendingDelete {
            source: "codex".to_string(),
            source_id: "source1".to_string(),
            session_title: "Test session".to_string(),
            source_label: "CDX".to_string(),
            origin: ResumeOrigin::Search,
        });

        let rendered = render_to_text(&app, 100, 18);

        assert!(rendered.contains("Delete session"));
        assert!(rendered.contains("search index only"));
        assert!(rendered.contains("kept"));
        assert!(rendered.contains("[Y]"));
        assert!(rendered.contains("delete from index"));
        assert!(rendered.contains("[N]"));
    }

    #[test]
    fn render_share_result_popup_shows_share_url() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app =
            App::new(&store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default());
        app.mode = AppMode::ShareResult;
        app.results = vec![SearchResult {
            session: Session {
                id: "session1".to_string(),
                source: "codex".to_string(),
                source_id: "source1".to_string(),
                title: "Test session".to_string(),
                directory: None,
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
        }];
        app.viewing_messages = vec![Message {
            session_id: "session1".to_string(),
            role: Role::User,
            content: "hello".to_string(),
            timestamp: None,
            seq: 0,
        }];
        app.viewing_sanitized_lines =
            vec![vec![SanitizedLine { text: "hello".to_string(), lower: "hello".to_string() }]];
        app.share_popup = Some(SharePopup {
            url: Some("https://recall-share.pages.dev/source1".to_string()),
            message: "Session shared".to_string(),
            is_error: false,
        });

        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let rendered = (0..18)
            .map(|y| buffer_row(terminal.backend().buffer(), y, 100))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("https://recall-share.pages.dev/source1"));
        assert!(rendered.contains("[O]"));
        assert!(rendered.contains("open"));
        assert!(rendered.contains("[C]"));
        assert!(rendered.contains("copy URL"));
        assert!(rendered.contains("[Enter/Esc]"));
        assert!(rendered.contains("close"));
    }

    #[test]
    fn render_handoff_target_picker_shows_targets() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut app =
            App::new(&store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default());
        app.mode = AppMode::HandoffTarget;
        app.handoff_target_selected = 3;
        app.results = vec![SearchResult {
            session: Session {
                id: "session1".to_string(),
                source: "codex".to_string(),
                source_id: "source1".to_string(),
                title: "Test session".to_string(),
                directory: None,
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
                is_import: true,
            },
            match_source: MatchSource::Fts,
            snippet: None,
        }];
        app.viewing_messages = vec![Message {
            session_id: "session1".to_string(),
            role: Role::User,
            content: "hello".to_string(),
            timestamp: None,
            seq: 0,
        }];
        app.viewing_sanitized_lines =
            vec![vec![SanitizedLine { text: "hello".to_string(), lower: "hello".to_string() }]];

        let backend = TestBackend::new(90, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let rendered = (0..18)
            .map(|y| buffer_row(terminal.backend().buffer(), y, 90))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Handoff target"));
        assert!(rendered.contains("Codex (codex)"));
        assert!(rendered.contains("OpenCode (opencode)"));
        assert!(rendered.contains("[Enter]"));
        assert!(rendered.contains("select"));
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        let mut row = String::new();
        for x in 0..width {
            row.push_str(buffer[(x, y)].symbol());
        }
        row
    }

    fn viewing_app_two_messages(store: &Store, selected: usize) -> App {
        let mut app =
            App::new(store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default());
        app.mode = AppMode::Viewing;
        app.results = vec![SearchResult {
            session: Session {
                id: "session1".to_string(),
                source: "codex".to_string(),
                source_id: "source1".to_string(),
                title: "Test session".to_string(),
                directory: Some("/tmp/repo".to_string()),
                repo_remote: None,
                repo_slug: None,
                repo_name: None,
                started_at: 0,
                updated_at: None,
                message_count: 2,
                entrypoint: None,
                custom_title: None,
                summary: None,
                duration_minutes: None,
                source_file_path: None,
                is_import: false,
            },
            match_source: MatchSource::Fts,
            snippet: None,
        }];
        app.viewing_messages = vec![
            Message {
                session_id: "session1".to_string(),
                role: Role::User,
                content: "hello".to_string(),
                timestamp: None,
                seq: 0,
            },
            Message {
                session_id: "session1".to_string(),
                role: Role::Assistant,
                content: "world".to_string(),
                timestamp: None,
                seq: 1,
            },
        ];
        app.viewing_sanitized_lines = vec![
            vec![SanitizedLine { text: "hello".to_string(), lower: "hello".to_string() }],
            vec![SanitizedLine { text: "world".to_string(), lower: "world".to_string() }],
        ];
        app.viewing_selected_msg = selected;
        app
    }

    #[test]
    fn render_viewing_selected_message_shows_role_colored_gutter() {
        use ratatui::layout::Rect;
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let app = viewing_app_two_messages(&store, 0);

        let width = 40u16;
        let height = 12u16;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let frame = terminal.draw(|f| render(f, &app)).unwrap();
        let buffer = frame.buffer;

        let layout = crate::tui::layout::viewing_layout(Rect::new(0, 0, width, height));
        let msgs = layout.messages;

        // Selected (msg 0) header row: gutter cell is the role-colored half block.
        assert_eq!(buffer[(msgs.x, msgs.y)].symbol(), "▌");
        assert_eq!(buffer[(msgs.x, msgs.y)].style().fg, Some(THEME.user));
        // Header text is no longer bg-filled (ratatui Cell::style() always reports Some(_);
        // an untouched/no-explicit-bg cell reports Some(Color::Reset), never DarkGray).
        assert_eq!(buffer[(msgs.x + 2, msgs.y)].style().bg, Some(THEME.background));

        // Selected body line (msg 0, one wrapped line) also carries the gutter,
        // and the body text keeps its own White fg with no DarkGray fill.
        assert_eq!(buffer[(msgs.x, msgs.y + 1)].symbol(), "▌");
        assert_eq!(buffer[(msgs.x, msgs.y + 1)].style().fg, Some(THEME.user));
        assert_eq!(buffer[(msgs.x + 2, msgs.y + 1)].symbol(), "h");
        assert_eq!(buffer[(msgs.x + 2, msgs.y + 1)].style().fg, Some(THEME.text));
        assert_ne!(buffer[(msgs.x + 2, msgs.y + 1)].style().bg, Some(THEME.message_highlight));
    }

    #[test]
    fn render_viewing_unselected_message_shows_blank_gutter() {
        use ratatui::layout::Rect;
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let app = viewing_app_two_messages(&store, 0);

        let width = 40u16;
        let height = 12u16;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let frame = terminal.draw(|f| render(f, &app)).unwrap();
        let buffer = frame.buffer;

        let layout = crate::tui::layout::viewing_layout(Rect::new(0, 0, width, height));
        let msgs = layout.messages;

        // msg 0 occupies rows 0 (header), 1 (body), 2 (blank) -> msg 1 header at row 3.
        let hdr1_y = msgs.y + 3;
        assert_eq!(buffer[(msgs.x, hdr1_y)].symbol(), " ");
        assert_ne!(buffer[(msgs.x, hdr1_y)].symbol(), "▌");
        // Unselected header text keeps role color, no bg fill.
        assert_eq!(buffer[(msgs.x + 2, hdr1_y)].style().fg, Some(THEME.assistant));
        assert_eq!(buffer[(msgs.x + 2, hdr1_y)].style().bg, Some(THEME.background));
    }
}
