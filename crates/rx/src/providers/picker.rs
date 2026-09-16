use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use ratatui::{
    DefaultTerminal, Frame, TerminalOptions, Viewport,
    layout::{Position, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::*;
use crate::ui::Palette;

const PAGE_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Provider,
    ApiKey,
    ConfirmLogout,
}

pub(super) enum Outcome {
    Login { id: String, key: String },
    Logout { index: usize },
    Use { index: usize },
}

struct App<'a> {
    action: Action,
    step: Step,
    providers: &'a [ProviderState],
    query: String,
    cursor: usize,
    selected: Option<usize>,
    api_key: String,
    validation_error: bool,
    direct: bool,
    exit: bool,
    outcome: Option<Outcome>,
}

impl<'a> App<'a> {
    fn new(action: Action, providers: &'a [ProviderState]) -> Self {
        let mut app = Self {
            action,
            step: Step::Provider,
            providers,
            query: String::new(),
            cursor: 0,
            selected: None,
            api_key: String::new(),
            validation_error: false,
            direct: false,
            exit: false,
            outcome: None,
        };
        if action != Action::Login
            && let Some(position) =
                app.filtered_providers().iter().position(|index| app.providers[*index].default)
        {
            app.cursor = position;
        }
        app
    }

    fn login_for_provider(providers: &'a [ProviderState], selected: usize) -> Self {
        let mut app = Self::new(Action::Login, providers);
        app.selected = Some(selected);
        app.step = Step::ApiKey;
        app.direct = true;
        app
    }

    fn filtered_providers(&self) -> Vec<usize> {
        let candidates = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, provider)| provider.selectable(self.action));
        if self.query.is_empty() {
            return candidates.map(|(index, _)| index).collect();
        }
        let matcher = SkimMatcherV2::default().ignore_case();
        let mut matches = candidates
            .filter_map(|(index, state)| {
                let provider = &state.provider;
                let candidate = format!("{} {} {}", provider.name, provider.id, provider.endpoint);
                matcher.fuzzy_match(&candidate, &self.query).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        matches.into_iter().map(|(index, _)| index).collect()
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(value) => self.handle_paste(&value),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.exit = true;
            return;
        }
        match self.step {
            Step::Provider => self.handle_provider_key(key),
            Step::ApiKey => self.handle_api_key(key),
            Step::ConfirmLogout => self.handle_logout_confirmation(key),
        }
    }

    fn handle_provider_key(&mut self, key: KeyEvent) {
        let filtered = self.filtered_providers();
        let count = filtered.len();
        match key.code {
            KeyCode::Up if count > 0 => {
                self.cursor = self.cursor.checked_sub(1).unwrap_or(count - 1);
            }
            KeyCode::Down if count > 0 => self.cursor = (self.cursor + 1) % count,
            KeyCode::Enter if count > 0 => {
                self.selected = filtered.get(self.cursor).copied();
                self.api_key.clear();
                self.validation_error = false;
                match self.action {
                    Action::Login => self.step = Step::ApiKey,
                    Action::Logout => self.step = Step::ConfirmLogout,
                    Action::Use => {
                        self.outcome =
                            Some(Outcome::Use { index: self.selected.expect("selected provider") });
                        self.exit = true;
                    }
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.cursor = 0;
            }
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.cursor = 0;
            }
            KeyCode::Esc => self.exit = true,
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.query.push(character);
                self.cursor = 0;
            }
            _ => {}
        }
    }

    fn handle_api_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter if self.api_key.trim().is_empty() => self.validation_error = true,
            KeyCode::Enter => {
                let index = self.selected.expect("selected provider");
                self.outcome = Some(Outcome::Login {
                    id: self.providers[index].provider.id.clone(),
                    key: self.api_key.trim().to_string(),
                });
                self.exit = true;
            }
            KeyCode::Backspace => {
                self.api_key.pop();
                self.validation_error = false;
            }
            KeyCode::Esc => {
                self.api_key.clear();
                self.validation_error = false;
                if self.direct {
                    self.exit = true;
                } else {
                    self.step = Step::Provider;
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.api_key.push(character);
                self.validation_error = false;
            }
            _ => {}
        }
    }

    fn handle_logout_confirmation(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                self.outcome =
                    Some(Outcome::Logout { index: self.selected.expect("selected provider") });
                self.exit = true;
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => self.step = Step::Provider,
            _ => {}
        }
    }

    fn handle_paste(&mut self, value: &str) {
        let value = value.chars().filter(|character| !character.is_control()).collect::<String>();
        match self.step {
            Step::Provider => {
                self.query.push_str(&value);
                self.cursor = 0;
            }
            Step::ApiKey => {
                self.api_key.push_str(&value);
                self.validation_error = false;
            }
            Step::ConfirmLogout => {}
        }
    }
}

pub(super) fn run_ui(
    action: Action,
    providers: &[ProviderState],
    env: &EnvLookup,
    selected: Option<usize>,
) -> Result<Option<Outcome>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        let command = match action {
            Action::Login => "login",
            Action::Logout => "logout",
            Action::Use => "use",
        };
        bail!("rx providers {command} requires an interactive terminal");
    }
    let palette = Palette::current(env);
    let mut terminal =
        match ratatui::try_init_with_options(TerminalOptions { viewport: Viewport::Inline(13) }) {
            Ok(terminal) => terminal,
            Err(error) => {
                ratatui::restore();
                return Err(error).context("failed to initialize provider picker");
            }
        };
    let result = run_provider_ui(&mut terminal, action, providers, selected, palette);
    let cleanup = terminal.clear();
    ratatui::restore();
    match result {
        Ok(outcome) => {
            cleanup.context("failed to clear provider picker")?;
            Ok(outcome)
        }
        Err(error) => Err(error),
    }
}

fn run_provider_ui(
    terminal: &mut DefaultTerminal,
    action: Action,
    providers: &[ProviderState],
    selected: Option<usize>,
    palette: Palette,
) -> Result<Option<Outcome>> {
    let mut app = match selected {
        Some(index) => App::login_for_provider(providers, index),
        None => App::new(action, providers),
    };
    while !app.exit {
        terminal.draw(|frame| render(frame, &app, palette))?;
        if event::poll(Duration::from_millis(100))? {
            app.handle_event(event::read()?);
        }
    }
    Ok(app.outcome)
}

fn render(frame: &mut Frame, app: &App, palette: Palette) {
    let area = frame.area();
    match app.step {
        Step::Provider => render_provider_picker(frame, area, app, palette),
        Step::ApiKey => render_api_key(frame, area, app, palette),
        Step::ConfirmLogout => render_logout_confirmation(frame, area, app, palette),
    }
}

fn render_provider_picker(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let filtered = app.filtered_providers();
    let mut lines = Vec::with_capacity(13);
    let query = if app.query.is_empty() {
        Span::styled(
            if app.action == Action::Login {
                "Search providers"
            } else {
                "Search configured providers"
            },
            palette.muted,
        )
    } else {
        Span::styled(app.query.as_str(), palette.primary)
    };
    lines.push(Line::from(vec![Span::styled("> ", palette.accent), query]));
    lines.push(Line::default());
    if filtered.is_empty() {
        lines.push(Line::from(Span::styled("  No matching providers", palette.muted)));
    } else {
        let page_start = (app.cursor / PAGE_SIZE) * PAGE_SIZE;
        for (visible_index, provider_index) in
            filtered.iter().skip(page_start).take(PAGE_SIZE).enumerate()
        {
            let selected = page_start + visible_index == app.cursor;
            lines.push(provider_line(
                &app.providers[*provider_index],
                selected,
                area.width,
                palette,
            ));
        }
    }
    lines.resize(2 + PAGE_SIZE, Line::default());
    let position = if filtered.is_empty() { 0 } else { app.cursor + 1 };
    lines.push(Line::from(Span::styled(format!("({position}/{})", filtered.len()), palette.muted)));
    lines.push(Line::from(vec![
        Span::styled("*", palette.default),
        Span::styled(" default  ", palette.muted),
        Span::styled("•", palette.configured),
        Span::styled(" configured", palette.muted),
    ]));
    lines.push(Line::from(Span::styled(
        "type to search  ↑↓ move  enter select  esc clear/cancel",
        palette.muted,
    )));
    frame.render_widget(Paragraph::new(lines), area);
    let cursor_x = area.x + 2 + app.query.chars().count() as u16;
    frame.set_cursor_position(Position::new(cursor_x.min(area.right().saturating_sub(1)), area.y));
}

fn provider_line(
    state: &ProviderState,
    selected: bool,
    width: u16,
    palette: Palette,
) -> Line<'static> {
    const NAME_WIDTH: usize = 20;
    const MARKER_WIDTH: u16 = 2;
    let name_style = if selected {
        (palette.accent, Modifier::BOLD)
    } else {
        (palette.primary, Modifier::empty())
    };
    let (marker, marker_style) = if state.default {
        ("* ", palette.default)
    } else if state.configured {
        ("• ", palette.configured)
    } else {
        ("  ", palette.muted)
    };
    let endpoint_width = usize::from(width.saturating_sub(2 + MARKER_WIDTH + NAME_WIDTH as u16));
    let endpoint = truncate(&state.provider.endpoint, endpoint_width);
    Line::from(vec![
        Span::styled(if selected { "→ " } else { "  " }, palette.accent),
        Span::styled(marker, marker_style),
        Span::styled(state.provider.name.clone(), name_style),
        Span::raw(" ".repeat(NAME_WIDTH.saturating_sub(state.provider.name.chars().count()))),
        Span::styled(endpoint, palette.muted),
    ])
}

fn render_api_key(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let provider = &app.providers[app.selected.expect("selected provider")].provider;
    let bullets = "•".repeat(app.api_key.chars().count());
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Provider  ", palette.muted),
            Span::styled(provider.name.clone(), (palette.accent, Modifier::BOLD)),
        ]),
        Line::from(Span::styled(format!("  {}", provider.endpoint), palette.muted)),
        Line::default(),
        Line::from(vec![
            Span::styled("> API key  ", palette.accent),
            Span::styled(bullets, palette.primary),
        ]),
        Line::from(Span::styled(format!("  Credential: {}", provider.env), palette.muted)),
    ];
    if app.validation_error {
        lines.push(Line::from(Span::styled("  API key is required", palette.error)));
    } else {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        if app.direct {
            "enter save  esc cancel  ctrl+c cancel"
        } else {
            "enter save  esc back  ctrl+c cancel"
        },
        palette.muted,
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    let cursor_x = area.x + 11 + app.api_key.chars().count() as u16;
    frame.set_cursor_position(Position::new(
        cursor_x.min(area.right().saturating_sub(1)),
        area.y + 3,
    ));
}

fn render_logout_confirmation(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let state = &app.providers[app.selected.expect("selected provider")];
    let detail = if state.stored_key {
        "Removes the locally stored API key."
    } else if state.environment_active {
        "The key comes from your environment; rx will show the unset command."
    } else {
        "No local credential is configured."
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("Log out of ", palette.error),
            Span::styled(state.provider.name.clone(), (palette.primary, Modifier::BOLD)),
            Span::styled("?", palette.error),
        ]),
        Line::from(Span::styled(format!("  {}", state.provider.endpoint), palette.muted)),
        Line::default(),
        Line::from(Span::styled(format!("  {detail}"), palette.primary)),
        Line::default(),
        Line::from(vec![
            Span::styled("y", (palette.error, Modifier::BOLD)),
            Span::styled(" confirm  ", palette.muted),
            Span::styled("n/esc", palette.accent),
            Span::styled(" back", palette.muted),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn truncate(value: &str, width: usize) -> String {
    match width {
        0 => String::new(),
        1 => "…".to_string(),
        _ if value.chars().count() > width => {
            let mut truncated = value.chars().take(width - 1).collect::<String>();
            truncated.push('…');
            truncated
        }
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests;
