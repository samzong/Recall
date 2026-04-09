use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEvent, MouseEvent, MouseEventKind};

pub enum AppEvent {
    Key(KeyEvent),
    ScrollUp,
    ScrollDown,
    Tick,
}

pub fn poll_event(tick_rate: Duration) -> Result<AppEvent> {
    if event::poll(tick_rate)? {
        match event::read()? {
            Event::Key(key) => return Ok(AppEvent::Key(key)),
            Event::Mouse(MouseEvent { kind, .. }) => match kind {
                MouseEventKind::ScrollUp => return Ok(AppEvent::ScrollUp),
                MouseEventKind::ScrollDown => return Ok(AppEvent::ScrollDown),
                _ => {}
            },
            _ => {}
        }
    }
    Ok(AppEvent::Tick)
}
