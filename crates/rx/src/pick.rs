use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::disable_raw_mode;
use ratatui::{
    DefaultTerminal, Frame, TerminalOptions, Viewport,
    backend::{Backend, ClearType},
    layout::Position,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::EnvLookup;
use crate::args::Harness;

const VIEWPORT_HEIGHT: u16 = 9;

struct Palette {
    accent: Color,
    primary: Color,
    muted: Color,
}

impl Palette {
    fn current(env: &EnvLookup) -> Self {
        if env.get("NO_COLOR").is_some() {
            return Self { accent: Color::Reset, primary: Color::Reset, muted: Color::Reset };
        }
        Self {
            accent: Color::Rgb(116, 199, 236),
            primary: Color::Rgb(220, 223, 228),
            muted: Color::Rgb(111, 115, 122),
        }
    }
}

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

struct App {
    selected: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum PickerAction {
    Continue,
    Cancel,
    Launch(Harness),
}

impl App {
    fn handle_key(&mut self, key: KeyEvent) -> PickerAction {
        match key.code {
            KeyCode::Esc => PickerAction::Cancel,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                PickerAction::Cancel
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
}

pub(crate) fn harness(env: &EnvLookup) -> Result<Option<Harness>> {
    pick(io::stdin().is_terminal() && io::stdout().is_terminal(), env)
}

fn pick(interactive: bool, env: &EnvLookup) -> Result<Option<Harness>> {
    if !interactive {
        bail!("missing harness name\n\n{}", crate::help_text().trim_end());
    }
    let mut terminal = match ratatui::try_init_with_options(TerminalOptions {
        viewport: Viewport::Inline(VIEWPORT_HEIGHT),
    }) {
        Ok(terminal) => terminal,
        Err(error) => {
            ratatui::restore();
            return Err(error).context("failed to initialize harness picker");
        }
    };
    let result = run(&mut terminal, env);
    let cleanup = collapse_inline(&mut terminal);
    match result {
        Ok(harness) => {
            cleanup.context("failed to clear harness picker")?;
            Ok(harness)
        }
        Err(error) => Err(error),
    }
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

fn run(terminal: &mut DefaultTerminal, env: &EnvLookup) -> Result<Option<Harness>> {
    let palette = Palette::current(env);
    let mut app = App { selected: 0 };
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
            PickerAction::Launch(harness) => return Ok(Some(harness)),
        }
    }
}

fn render(frame: &mut Frame<'_>, app: &App, palette: &Palette) {
    let mut lines =
        vec![Line::from(Span::styled("pick a harness", Style::default().fg(palette.accent)))];
    for (index, choice) in CHOICES.iter().enumerate() {
        let selected = index == app.selected;
        let marker = if selected { "→ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(palette.accent)),
            Span::styled("[", Style::default().fg(palette.muted)),
            Span::styled(
                choice.shortcut.to_string(),
                Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled("] ", Style::default().fg(palette.muted)),
            Span::styled(
                format!("{:<10}", choice.harness.as_str()),
                Style::default().fg(if selected { palette.accent } else { palette.primary }),
            ),
            Span::styled(format!("rx{}", choice.shortcut), Style::default().fg(palette.muted)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "↑↓ move  enter launch  esc cancel",
        Style::default().fg(palette.muted),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), frame.area());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EnvLookup;
    use std::collections::HashMap;

    #[test]
    fn harness_picker_requires_tty() {
        let error = pick(false, &EnvLookup::isolated(HashMap::new())).unwrap_err();
        assert!(error.to_string().contains("missing harness name"));
    }

    #[test]
    fn shortcuts_launch_matching_harnesses() {
        let mut app = App { selected: 0 };
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
        let mut app = App { selected: 0 };
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            PickerAction::Continue
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::Launch(Harness::Codex)
        );
    }

    #[test]
    fn control_c_still_cancels() {
        let mut app = App { selected: 0 };
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            PickerAction::Cancel
        );
    }
}
