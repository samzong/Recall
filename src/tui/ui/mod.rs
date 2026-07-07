mod popups;
mod search;
mod usage;
mod viewing;

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::scrollbar;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::tui::app::App;
use crate::tui::share_state::{AppMode, ResumeOrigin};

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
        Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD);
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

pub(super) fn line_with_background(mut line: Line<'static>, bg: Color) -> Line<'static> {
    if bg == Color::Reset {
        return line;
    }

    for span in &mut line.spans {
        if span.style.bg.is_none() {
            span.style.bg = Some(bg);
        }
    }
    line
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

    let mut state =
        ScrollbarState::new(content_len).viewport_content_length(viewport_len).position(position);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .symbols(scrollbar::VERTICAL)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_symbol("▌")
            .track_symbol(Some("▌"))
            .thumb_style(Style::default().fg(Color::Cyan))
            .track_style(Style::default().fg(Color::DarkGray)),
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
    fn highlight_spans_marks_each_query_term() {
        let spans = highlight_spans(
            "Alpha beta Gamma",
            "alpha beta gamma",
            &["alpha".to_string(), "gamma".to_string()],
            Style::default(),
        );

        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["Alpha", " beta ", "Gamma"]);
        assert_eq!(spans[0].style.bg, Some(Color::Yellow));
        assert_eq!(spans[1].style.bg, None);
        assert_eq!(spans[2].style.bg, Some(Color::Yellow));
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
        assert_eq!(spans[0].style.bg, Some(Color::Yellow));
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
        });

        let backend = TestBackend::new(100, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let summary = buffer_row(terminal.backend().buffer(), 1, 100);

        assert!(summary.contains(
            "tokens 31 input 10 output 9 cache r/w 6/4 reasoning 2 | time 2m | user msgs 2/3"
        ));
        assert_eq!(terminal.backend().buffer()[(2, 1)].fg, Color::Green);
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
}
