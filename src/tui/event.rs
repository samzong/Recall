use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};

pub(crate) enum AppEvent {
    Key(KeyEvent),
    MouseDown { column: u16, row: u16 },
    ScrollUp { column: u16, row: u16 },
    ScrollDown { column: u16, row: u16 },
    Tick,
}

pub(crate) fn poll_event(tick_rate: Duration) -> Result<AppEvent> {
    if event::poll(tick_rate)? {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => return Ok(AppEvent::Key(key)),
            Event::Mouse(MouseEvent { kind, column, row, .. }) => match kind {
                MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left) => {
                    return Ok(AppEvent::MouseDown { column, row });
                }
                MouseEventKind::ScrollUp => return Ok(AppEvent::ScrollUp { column, row }),
                MouseEventKind::ScrollDown => return Ok(AppEvent::ScrollDown { column, row }),
                _ => {}
            },
            _ => {}
        }
    }
    Ok(AppEvent::Tick)
}
