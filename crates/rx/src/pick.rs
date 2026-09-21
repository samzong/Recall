use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::disable_raw_mode;
use ratatui::{
    DefaultTerminal, Frame, TerminalOptions, Viewport,
    backend::{Backend, ClearType},
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::EnvLookup;
use crate::args::Harness;
use crate::config::Paths;
use crate::providers::LaunchChoice;

const HARNESS_HEIGHT: u16 = 9;
const PROVIDER_CHROME: u16 = 2;
const NAME_WIDTH: usize = 14;

use crate::ui::{Palette, truncate};

#[derive(Clone, Copy)]
struct Choice {
    harness: Harness,
    shortcut: char,
}

const CHOICES: [Choice; 6] = [
    Choice { harness: Harness::Claude, shortcut: 'c' },
    Choice { harness: Harness::Codex, shortcut: 'x' },
    Choice { harness: Harness::OpenCode, shortcut: 'o' },
    Choice { harness: Harness::Pi, shortcut: 'p' },
    Choice { harness: Harness::Dsh, shortcut: 'd' },
    Choice { harness: Harness::Kimi, shortcut: 'k' },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Harness,
    Provider,
}

struct App {
    selected: usize,
    step: Step,
    providers: Vec<LaunchChoice>,
    provider_cursor: usize,
    provider: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum PickerAction {
    Continue,
    Cancel,
    Launch(Harness),
}

impl App {
    fn new(providers: Vec<LaunchChoice>, provider: Option<String>) -> Self {
        Self { selected: 0, step: Step::Harness, providers, provider_cursor: 0, provider }
    }

    fn provider_label(&self) -> Option<String> {
        match &self.provider {
            Some(id) => Some(
                self.providers
                    .iter()
                    .find(|choice| &choice.id == id)
                    .map_or_else(|| id.clone(), |choice| choice.name.clone()),
            ),
            None => self
                .providers
                .iter()
                .find(|choice| choice.default)
                .map(|choice| format!("{} (default)", choice.name)),
        }
    }

    fn open_providers(&mut self) {
        if self.providers.is_empty() {
            return;
        }
        self.provider_cursor = self
            .provider
            .as_deref()
            .and_then(|id| self.providers.iter().position(|choice| choice.id == id))
            .or_else(|| self.providers.iter().position(|choice| choice.default))
            .unwrap_or(0);
        self.step = Step::Provider;
    }

    fn handle_key(&mut self, key: KeyEvent) -> PickerAction {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return PickerAction::Cancel;
        }
        match self.step {
            Step::Harness => self.handle_harness_key(key),
            Step::Provider => self.handle_provider_key(key),
        }
    }

    fn handle_harness_key(&mut self, key: KeyEvent) -> PickerAction {
        match key.code {
            KeyCode::Esc => PickerAction::Cancel,
            KeyCode::Tab => {
                self.open_providers();
                PickerAction::Continue
            }
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                PickerAction::Continue
            }
            KeyCode::Down => {
                if self.selected + 1 < CHOICES.len() {
                    self.selected += 1;
                }
                PickerAction::Continue
            }
            KeyCode::Enter => PickerAction::Launch(CHOICES[self.selected].harness),
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                CHOICES
                    .iter()
                    .find(|choice| choice.shortcut.eq_ignore_ascii_case(&character))
                    .map_or(PickerAction::Continue, |choice| PickerAction::Launch(choice.harness))
            }
            _ => PickerAction::Continue,
        }
    }

    fn handle_provider_key(&mut self, key: KeyEvent) -> PickerAction {
        match key.code {
            KeyCode::Up => {
                if self.provider_cursor > 0 {
                    self.provider_cursor -= 1;
                }
            }
            KeyCode::Down => {
                if self.provider_cursor + 1 < self.providers.len() {
                    self.provider_cursor += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(choice) = self.providers.get(self.provider_cursor) {
                    self.provider = Some(choice.id.clone());
                }
                self.step = Step::Harness;
            }
            KeyCode::Esc | KeyCode::Tab => self.step = Step::Harness,
            _ => {}
        }
        PickerAction::Continue
    }
}

pub(crate) fn harness(env: &EnvLookup) -> Result<Option<Harness>> {
    Ok(pick(interactive(), env, Vec::new(), None)?.map(|(harness, _)| harness))
}

pub(crate) fn harness_with_provider(
    paths: &Paths,
    env: &EnvLookup,
    provider: Option<String>,
) -> Result<Option<(Harness, Option<String>)>> {
    let interactive = interactive();
    let providers =
        if interactive { crate::providers::launch_choices(paths, env)? } else { Vec::new() };
    pick(interactive, env, providers, provider)
}

fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn pick(
    interactive: bool,
    env: &EnvLookup,
    providers: Vec<LaunchChoice>,
    provider: Option<String>,
) -> Result<Option<(Harness, Option<String>)>> {
    if !interactive {
        bail!("missing harness name\n\n{}", crate::help_text().trim_end());
    }
    let mut terminal = match ratatui::try_init_with_options(TerminalOptions {
        viewport: Viewport::Inline(viewport_height(providers.len())),
    }) {
        Ok(terminal) => terminal,
        Err(error) => {
            ratatui::restore();
            return Err(error).context("failed to initialize harness picker");
        }
    };
    let result = run(&mut terminal, env, providers, provider);
    let cleanup = collapse_inline(&mut terminal);
    match result {
        Ok(picked) => {
            cleanup.context("failed to clear harness picker")?;
            Ok(picked)
        }
        Err(error) => Err(error),
    }
}

fn viewport_height(providers: usize) -> u16 {
    let wanted = PROVIDER_CHROME.saturating_add(u16::try_from(providers).unwrap_or(u16::MAX));
    let available =
        crossterm::terminal::size().map_or(u16::MAX, |(_, rows)| rows.saturating_sub(1));
    wanted.clamp(HARNESS_HEIGHT, available.max(HARNESS_HEIGHT))
}

fn collapse_inline(terminal: &mut DefaultTerminal) -> io::Result<()> {
    let area = terminal.get_frame().area();
    let backend = terminal.backend_mut();
    let erased = (|| {
        for y in area.top()..area.bottom() {
            backend.set_cursor_position(Position { x: 0, y })?;
            backend.clear_region(ClearType::CurrentLine)?;
        }
        backend.set_cursor_position(area.as_position())?;
        backend.show_cursor()?;
        backend.flush()
    })();
    disable_raw_mode()?;
    erased
}

fn run(
    terminal: &mut DefaultTerminal,
    env: &EnvLookup,
    providers: Vec<LaunchChoice>,
    provider: Option<String>,
) -> Result<Option<(Harness, Option<String>)>> {
    let palette = Palette::current(env);
    let mut app = App::new(providers, provider);
    loop {
        terminal.draw(|frame| render(frame, &app, &palette))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match app.handle_key(key) {
            PickerAction::Continue => {}
            PickerAction::Cancel => return Ok(None),
            PickerAction::Launch(harness) => return Ok(Some((harness, app.provider))),
        }
    }
}

fn render(frame: &mut Frame<'_>, app: &App, palette: &Palette) {
    let area = frame.area();
    let lines = match app.step {
        Step::Harness => harness_lines(app, palette),
        Step::Provider => provider_lines(app, palette, area),
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn harness_lines(app: &App, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("pick a harness", palette.accent))];
    for (index, choice) in CHOICES.iter().enumerate() {
        let selected = index == app.selected;
        let marker = if selected { "→ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(marker, palette.accent),
            Span::styled("[", palette.muted),
            Span::styled(choice.shortcut.to_string(), (palette.accent, Modifier::BOLD)),
            Span::styled("] ", palette.muted),
            Span::styled(
                format!("{:<10}", choice.harness.as_str()),
                Style::default().fg(if selected { palette.accent } else { palette.primary }),
            ),
            Span::styled(format!("rx{}", choice.shortcut), palette.muted),
        ]));
    }
    if let Some(label) = app.provider_label() {
        lines.push(Line::from(vec![
            Span::styled("provider: ", palette.muted),
            Span::styled(label, palette.configured),
        ]));
    }
    lines.push(Line::from(Span::styled(
        if app.providers.is_empty() {
            "↑↓ move  enter launch  esc cancel"
        } else {
            "↑↓ move  enter launch  tab provider  esc cancel"
        },
        palette.muted,
    )));
    lines
}

fn provider_lines(app: &App, palette: &Palette, area: Rect) -> Vec<Line<'static>> {
    let mut lines =
        vec![Line::from(Span::styled("pick a provider for this launch", palette.accent))];
    let visible = usize::from(area.height.saturating_sub(PROVIDER_CHROME)).max(1);
    let paged = app.providers.len() > visible;
    let page = (visible - usize::from(paged)).max(1);
    let page_start = (app.provider_cursor / page) * page;
    let endpoint_width = usize::from(area.width.saturating_sub(4 + NAME_WIDTH as u16));
    for (offset, choice) in app.providers.iter().skip(page_start).take(page).enumerate() {
        let selected = page_start + offset == app.provider_cursor;
        lines.push(Line::from(vec![
            Span::styled(if selected { "→ " } else { "  " }, palette.accent),
            Span::styled(if choice.default { "* " } else { "  " }, palette.default),
            Span::styled(
                format!("{:<NAME_WIDTH$}", truncate(&choice.name, NAME_WIDTH)),
                Style::default().fg(if selected { palette.accent } else { palette.primary }),
            ),
            Span::styled(truncate(&choice.endpoint, endpoint_width), palette.muted),
        ]));
    }
    if paged {
        lines.push(Line::from(Span::styled(
            format!("({}/{})", app.provider_cursor + 1, app.providers.len()),
            palette.muted,
        )));
    }
    lines.push(Line::from(Span::styled("↑↓ move  enter select  esc back", palette.muted)));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EnvLookup;
    use std::collections::HashMap;

    fn choice(id: &str, default: bool) -> LaunchChoice {
        LaunchChoice {
            id: id.to_string(),
            name: id.to_string(),
            endpoint: format!("https://{id}.example/v1"),
            default,
        }
    }

    fn app() -> App {
        App::new(Vec::new(), None)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn harness_picker_requires_tty() {
        let error =
            pick(false, &EnvLookup::isolated(HashMap::new()), Vec::new(), None).unwrap_err();
        assert!(error.to_string().contains("missing harness name"));
    }

    #[test]
    fn shortcuts_launch_matching_harnesses() {
        let mut app = app();
        for choice in CHOICES {
            let key = KeyEvent::new(KeyCode::Char(choice.shortcut), KeyModifiers::NONE);
            assert_eq!(app.handle_key(key), PickerAction::Launch(choice.harness));

            let key = KeyEvent::new(
                KeyCode::Char(choice.shortcut.to_ascii_uppercase()),
                KeyModifiers::SHIFT,
            );
            assert_eq!(app.handle_key(key), PickerAction::Launch(choice.harness));
        }
    }

    #[test]
    fn arrows_and_enter_still_select_harnesses() {
        let mut app = app();
        assert_eq!(app.handle_key(press(KeyCode::Down)), PickerAction::Continue);
        assert_eq!(app.handle_key(press(KeyCode::Enter)), PickerAction::Launch(Harness::Codex));
    }

    #[test]
    fn control_c_still_cancels() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            PickerAction::Cancel
        );
    }

    #[test]
    fn provider_step_overrides_the_default_for_one_launch() {
        let mut app = App::new(vec![choice("openrouter", true), choice("deepseek", false)], None);
        assert_eq!(app.provider_label().as_deref(), Some("openrouter (default)"));
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.provider_cursor, 0);
        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.provider.as_deref(), Some("deepseek"));
        assert_eq!(app.provider_label().as_deref(), Some("deepseek"));
        assert_eq!(app.handle_key(press(KeyCode::Enter)), PickerAction::Launch(Harness::Claude));
    }

    #[test]
    fn provider_step_escape_keeps_the_previous_selection() {
        let mut app = App::new(vec![choice("openrouter", true)], None);
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.handle_key(press(KeyCode::Esc)), PickerAction::Continue);
        assert_eq!(app.provider, None);
        assert_eq!(app.handle_key(press(KeyCode::Esc)), PickerAction::Cancel);
    }

    #[test]
    fn tab_without_configured_providers_stays_on_the_harness_step() {
        let mut app = app();
        assert_eq!(app.handle_key(press(KeyCode::Tab)), PickerAction::Continue);
        assert_eq!(app.handle_key(press(KeyCode::Enter)), PickerAction::Launch(Harness::Claude));
    }

    fn rendered(app: &App, height: u16) -> Vec<String> {
        provider_lines(
            app,
            &Palette::current(&EnvLookup::isolated(HashMap::new())),
            Rect::new(0, 0, 80, height),
        )
        .iter()
        .map(|line| line.to_string())
        .collect()
    }

    #[test]
    fn the_provider_list_pages_only_when_it_cannot_fit() {
        let providers = (0..6).map(|index| choice(&format!("p{index}"), index == 0)).collect();
        let app = App::new(providers, None);

        let full = rendered(&app, 9);
        assert_eq!(full.len(), 8);
        assert!(full.iter().all(|line| !line.starts_with('(')), "{full:?}");
        assert!(full.iter().any(|line| line.contains("p5")), "{full:?}");

        let short = rendered(&app, 6);
        assert_eq!(short.len(), 6);
        assert!(short.iter().any(|line| line.contains("(1/6)")), "{short:?}");
    }

    #[test]
    fn a_requested_provider_seeds_the_picker() {
        let app = App::new(vec![choice("deepseek", false)], Some("none".to_string()));
        assert_eq!(app.provider_label().as_deref(), Some("none"));
    }
}
