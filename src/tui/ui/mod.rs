mod pickers;
mod popups;
mod search;
mod usage;
mod usage_chart;
mod usage_projects;
mod usage_skills;
mod viewing;

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::scrollbar;
use ratatui::text::{Line, Span};
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
        AppMode::SharePicker => {
            viewing::render_viewing(f, app);
            pickers::render_share_picker(f, app);
        }
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
            pickers::render_handoff_target_picker(f, app);
        }
        AppMode::Subagents => {
            viewing::render_viewing(f, app);
            pickers::render_subagents_picker(f, app);
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
    use crate::types::{MatchSource, Message, Role, SearchResult, Session};

    fn test_app() -> App {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        App::new(&store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default())
    }

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
                locations: Vec::new(),
                alternative_versions: 0,
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
    fn render_viewing_shows_subagent_hint_and_picker() {
        let mut app = test_app();
        app.viewing.session = Some(numbered_session_result(1).session);
        app.viewing.children =
            vec![numbered_session_result(2).session, numbered_session_result(3).session];
        app.viewing.messages = vec![Message {
            session_id: "session1".to_string(),
            role: Role::User,
            content: "hello".to_string(),
            timestamp: None,
            seq: 0,
        }];

        app.mode = AppMode::Viewing;
        let viewing = render_to_text(&app, 80, 12);
        assert!(viewing.contains("▾ 2 subs"), "header hints at subagents:\n{viewing}");

        app.mode = AppMode::Subagents;
        let picker = render_to_text(&app, 80, 12);
        assert!(picker.contains("Subagents (2)"), "picker title:\n{picker}");
        assert!(picker.contains("Session 2"));
        assert!(picker.contains("Session 3"));
    }

    #[test]
    fn render_subagents_picker_scrolls_selection_into_view() {
        let mut app = test_app();
        app.viewing.session = Some(numbered_session_result(1).session);
        app.viewing.children = (1..=20).map(|n| numbered_session_result(n).session).collect();
        app.viewing.child_selected = 19;
        app.mode = AppMode::Subagents;

        let picker = render_to_text(&app, 80, 12);

        assert!(picker.contains("Session 20"), "selected last row is visible:\n{picker}");
        assert!(picker.contains("Session 16"), "window keeps rows near the selection:\n{picker}");
        assert!(!picker.contains("Session 15"), "rows above the window are hidden:\n{picker}");
    }

    #[test]
    fn render_share_result_popup_shows_share_url() {
        let mut app = test_app();
        app.mode = AppMode::ShareResult;
        app.results = vec![SearchResult {
            session: Session {
                title: "Test session".into(),
                directory: None,
                updated_at: None,
                duration_minutes: None,
                is_import: false,
                ..numbered_session_result(1).session
            },
            match_source: MatchSource::Fts,
            snippet: None,
        }];
        app.viewing.messages = vec![Message {
            session_id: "session1".to_string(),
            role: Role::User,
            content: "hello".to_string(),
            timestamp: None,
            seq: 0,
        }];
        app.viewing.lines =
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
    }

    #[test]
    fn render_handoff_target_picker_fits_short_terminal() {
        let mut app = test_app();
        app.mode = AppMode::HandoffTarget;
        app.handoff_targets = (0..17)
            .map(|index| crate::handoff::HandoffTarget {
                id: format!("t{index}"),
                label: format!("Target{index}"),
            })
            .collect();
        app.handoff_target_selected = 16;

        let backend = TestBackend::new(90, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let rendered = (0..18)
            .map(|y| buffer_row(terminal.backend().buffer(), y, 90))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Target16 (t16)"));
        assert!(!rendered.contains("Target0 (t0)"));
    }

    #[test]
    fn usage_charts_render_today_daily_weekly_and_metric_toggles() {
        use std::collections::BTreeMap;

        use chrono::{Datelike, Duration, Local};

        use crate::db::search::TimeRange;
        use crate::tui::usage_state::UsageTab;
        use crate::usage::{PeriodUsage, TokenTotals, UsageReport, UsageSummary};

        let now = Local::now();
        let today = now.date_naive();
        let two_days_ago = today - Duration::days(2);
        let iso_now = today.iso_week();
        let iso_prev = (today - Duration::weeks(2)).iso_week();
        let mut hourly_today_tokens = [0i64; 24];
        let mut hourly_today_sessions = [0usize; 24];
        hourly_today_tokens[9] = 1_500;
        hourly_today_tokens[15] = 3_000;
        hourly_today_sessions[9] = 2;
        hourly_today_sessions[15] = 5;

        let mut daily_all_sessions = BTreeMap::new();
        daily_all_sessions.insert(two_days_ago.format("%Y-%m-%d").to_string(), 3);
        daily_all_sessions.insert(today.format("%Y-%m-%d").to_string(), 7);

        let week_prev = format!("{}-W{:02}", iso_prev.year(), iso_prev.week());
        let week_now = format!("{}-W{:02}", iso_now.year(), iso_now.week());
        let mut weekly_all_sessions = BTreeMap::new();
        weekly_all_sessions.insert(week_prev.clone(), 4);
        weekly_all_sessions.insert(week_now.clone(), 9);

        let report = UsageReport {
            scope: None,
            summary: UsageSummary {
                events: 10,
                sessions: 10,
                sources: 1,
                models: 1,
                token_source_events: BTreeMap::new(),
                tokens: TokenTotals { total_tokens: 4_500, ..TokenTotals::default() },
            },
            by_source: Vec::new(),
            by_model: Vec::new(),
            daily: vec![
                PeriodUsage {
                    period: two_days_ago.format("%Y-%m-%d").to_string(),
                    sessions: 3,
                    events: 3,
                    tokens: TokenTotals { total_tokens: 1_500, ..TokenTotals::default() },
                },
                PeriodUsage {
                    period: today.format("%Y-%m-%d").to_string(),
                    sessions: 7,
                    events: 7,
                    tokens: TokenTotals { total_tokens: 3_000, ..TokenTotals::default() },
                },
            ],
            weekly: vec![
                PeriodUsage {
                    period: week_prev,
                    sessions: 4,
                    events: 4,
                    tokens: TokenTotals { total_tokens: 1_500, ..TokenTotals::default() },
                },
                PeriodUsage {
                    period: week_now,
                    sessions: 9,
                    events: 9,
                    tokens: TokenTotals { total_tokens: 3_000, ..TokenTotals::default() },
                },
            ],
            monthly: Vec::new(),
            hourly_today_tokens,
            hourly_today_sessions,
            daily_all_sessions,
            weekly_all_sessions,
        };

        let mut app = test_app();
        app.mode = AppMode::Usage;
        app.usage.tab = UsageTab::Tokens;
        app.usage.year_report = Some(report.clone());
        app.usage.report = Some(report);

        app.usage.time_filter = TimeRange::Today;
        let today_tokens = render_to_text(&app, 100, 26);
        assert!(today_tokens.contains("Token Activity Map"));
        assert!(today_tokens.contains("Tokens Over Time (today)"));
        assert!(!today_tokens.contains("(24h)"));
        assert!(today_tokens.chars().any(|c| "▁▂▃▄▅▆▇█".contains(c)));

        app.usage.toggle_chart_metric();
        let today_sessions = render_to_text(&app, 100, 26);
        assert!(today_sessions.contains("Session Activity Map"));
        assert!(today_sessions.contains("Sessions Over Time (today)"));

        app.usage.time_filter = TimeRange::Week;
        let week_daily = render_to_text(&app, 100, 26);
        assert!(week_daily.contains("Sessions Over Time (daily)"));

        app.usage.toggle_granularity();
        let week_weekly = render_to_text(&app, 100, 26);
        assert!(week_weekly.contains("Sessions Over Time (weekly)"));

        let narrow = render_to_text(&app, 55, 22);
        assert!(narrow.contains("Recall Usage"));
    }

    #[test]
    fn share_picker_defaults_toggles_and_renders_counts() {
        use crate::db::store::SessionTopologyWrite;
        use crate::tui::share_state::SharePublishOutcome;
        use crate::types::{ParentLink, ParentRelation, ThreadRole};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut root = numbered_session_result(1).session;
        root.id = "root-1".into();
        root.source_id = "root-src".into();
        root.title = "Primary Thread".into();
        root.message_count = 1;

        let mut child_non_empty = numbered_session_result(2).session;
        child_non_empty.id = "child-1".into();
        child_non_empty.source_id = "child-src-1".into();
        child_non_empty.title = "NonEmpty Subagent".into();
        child_non_empty.message_count = 1;

        let mut child_empty = numbered_session_result(3).session;
        child_empty.id = "child-2".into();
        child_empty.source_id = "child-src-2".into();
        child_empty.title = "Empty Subagent".into();
        child_empty.message_count = 0;

        store
            .persist_session_with_usage_and_events_with_topology(
                &root,
                &[Message {
                    session_id: "root-1".into(),
                    role: Role::User,
                    content: "hi".into(),
                    timestamp: None,
                    seq: 0,
                }],
                &[],
                None,
                &[],
                None,
                &SessionTopologyWrite {
                    thread_role: Some(ThreadRole::Primary),
                    parents: &[],
                    parser_version: Some(1),
                },
            )
            .unwrap();
        store
            .persist_session_with_usage_and_events_with_topology(
                &child_non_empty,
                &[Message {
                    session_id: "child-1".into(),
                    role: Role::User,
                    content: "sub".into(),
                    timestamp: None,
                    seq: 0,
                }],
                &[],
                None,
                &[],
                None,
                &SessionTopologyWrite {
                    thread_role: Some(ThreadRole::Subagent),
                    parents: &[ParentLink {
                        relation: ParentRelation::Spawn,
                        source: "codex".into(),
                        source_id: "root-src".into(),
                    }],
                    parser_version: Some(1),
                },
            )
            .unwrap();
        store
            .persist_session_with_usage_and_events_with_topology(
                &child_empty,
                &[],
                &[],
                None,
                &[],
                None,
                &SessionTopologyWrite {
                    thread_role: Some(ThreadRole::Subagent),
                    parents: &[ParentLink {
                        relation: ParentRelation::Spawn,
                        source: "codex".into(),
                        source_id: "root-src".into(),
                    }],
                    parser_version: Some(1),
                },
            )
            .unwrap();

        let mut app =
            App::new(&store, vec![("codex".to_string(), "CDX".to_string())], AppConfig::default());
        app.viewing.load(child_non_empty.clone(), &store);
        app.mode = AppMode::Viewing;

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &store);
        assert!(matches!(app.mode, AppMode::SharePicker));
        let picker = app.share_picker.as_ref().unwrap();
        assert_eq!(picker.items.len(), 3);
        assert!(picker.items[0].is_root && picker.items[0].checked);
        assert!(picker.items[1].checked);
        assert!(!picker.items[2].checked);

        let rendered = render_to_text(&app, 100, 14);
        assert!(rendered.contains("Share sessions (2/3)"));
        assert!(rendered.contains("[x] Primary Thread"));
        assert!(rendered.contains("0 msgs"));

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &store);
        assert!(app.share_picker.as_ref().unwrap().items[0].checked);

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &store);
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &store);
        assert!(!app.share_picker.as_ref().unwrap().items[1].checked);

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &store);
        assert!(app.share_picker.as_ref().unwrap().items[1].checked);
        assert!(!app.share_picker.as_ref().unwrap().items[2].checked);

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &store);
        assert!(app.share_picker.as_ref().unwrap().items[0].checked);
        assert!(!app.share_picker.as_ref().unwrap().items[1].checked);
        assert!(!app.share_picker.as_ref().unwrap().items[2].checked);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &store);
        assert!(matches!(app.mode, AppMode::Viewing));
        assert!(app.share_picker.is_none());

        let (tx, rx) = std::sync::mpsc::channel();
        app.share_request_id = 5;
        app.share_publish_rx = Some(rx);
        tx.send(crate::tui::share_state::SharePublishResponse {
            request_id: 4,
            result: Ok(SharePublishOutcome {
                url: "https://stale.example".into(),
                session_count: 1,
                bundle_diff: None,
            }),
        })
        .unwrap();
        app.poll_share_publish();
        assert!(app.share_popup.is_none());
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        let mut row = String::new();
        for x in 0..width {
            row.push_str(buffer[(x, y)].symbol());
        }
        row
    }
}
