use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::disable_raw_mode;
use ratatui::{
    DefaultTerminal, Frame, TerminalOptions, Viewport,
    backend::{Backend, ClearType},
    layout::Position,
    style::{Color, Style},
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
    alias: &'static str,
}

const CHOICES: [Choice; 6] = [
    Choice { harness: Harness::Claude, alias: "rxc" },
    Choice { harness: Harness::Codex, alias: "rxx" },
    Choice { harness: Harness::OpenCode, alias: "rxo" },
    Choice { harness: Harness::Pi, alias: "rxp" },
    Choice { harness: Harness::Dsh, alias: "rxd" },
    Choice { harness: Harness::Kimi, alias: "rxk" },
];

struct App {
    selected: usize,
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
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(None);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if app.selected > 0 {
                    app.selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.selected + 1 < CHOICES.len() {
                    app.selected += 1;
                }
            }
            KeyCode::Enter => return Ok(Some(CHOICES[app.selected].harness)),
            _ => {}
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
            Span::styled(
                format!("{marker}{:<10}", choice.harness.as_str()),
                Style::default().fg(if selected { palette.accent } else { palette.primary }),
            ),
            Span::styled(choice.alias, Style::default().fg(palette.muted)),
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
}
