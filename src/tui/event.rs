use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};

pub(crate) enum AppEvent {
    Key(KeyEvent),
    MouseDown { column: u16, row: u16 },
    MouseDrag { column: u16, row: u16 },
    MouseUp,
    ScrollUp { column: u16, row: u16 },
    ScrollDown { column: u16, row: u16 },
    Tick,
}

pub(crate) fn poll_event(tick_rate: Duration) -> Result<AppEvent> {
    if event::poll(tick_rate)? {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => return Ok(AppEvent::Key(key)),
            Event::Mouse(MouseEvent { kind, column, row, .. }) => match kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    return Ok(AppEvent::MouseDown { column, row });
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    return Ok(AppEvent::MouseDrag { column, row });
                }
                MouseEventKind::Up(MouseButton::Left) => return Ok(AppEvent::MouseUp),
                MouseEventKind::ScrollUp => return Ok(AppEvent::ScrollUp { column, row }),
                MouseEventKind::ScrollDown => return Ok(AppEvent::ScrollDown { column, row }),
                _ => {}
            },
            _ => {}
        }
    }
    Ok(AppEvent::Tick)
}

pub(crate) fn cursor_prev(text: &str, cursor: usize) -> usize {
    text[..cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0)
}

pub(crate) fn cursor_next(text: &str, cursor: usize) -> usize {
    text[cursor..].char_indices().nth(1).map(|(i, _)| cursor + i).unwrap_or(text.len())
}

pub(crate) fn edit_text(text: &mut String, cursor: &mut usize, key: KeyCode) -> bool {
    match key {
        KeyCode::Char(c) => {
            text.insert(*cursor, c);
            *cursor += c.len_utf8();
        }
        KeyCode::Backspace if *cursor > 0 => {
            let previous = cursor_prev(text, *cursor);
            text.replace_range(previous..*cursor, "");
            *cursor = previous;
        }
        other => {
            *cursor = match other {
                KeyCode::Left => cursor_prev(text, *cursor),
                KeyCode::Right => cursor_next(text, *cursor),
                KeyCode::Home => 0,
                KeyCode::End => text.len(),
                _ => *cursor,
            };
            return false;
        }
    }
    true
}

impl crate::tui::search_state::PickerState {
    pub(crate) fn handle_text_key(&mut self, key: KeyEvent) {
        if (self.typing || matches!(key.code, KeyCode::Char(_)))
            && edit_text(&mut self.query, &mut self.cursor, key.code)
        {
            self.typing = true;
            self.selected = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_editing_preserves_utf8_cursor_boundaries() {
        let mut text = "a界🙂".to_string();
        let mut cursor = text.len();
        for (key, expected, position, changed) in [
            (KeyCode::Left, "a界🙂", 4, false),
            (KeyCode::Backspace, "a🙂", 1, true),
            (KeyCode::Char('é'), "aé🙂", 3, true),
            (KeyCode::Right, "aé🙂", 7, false),
            (KeyCode::Home, "aé🙂", 0, false),
            (KeyCode::Backspace, "aé🙂", 0, false),
            (KeyCode::End, "aé🙂", 7, false),
        ] {
            assert_eq!(edit_text(&mut text, &mut cursor, key), changed);
            assert_eq!((&*text, cursor), (expected, position));
        }
    }
}
