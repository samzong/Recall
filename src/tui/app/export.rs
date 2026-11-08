use super::*;

impl App {
    pub(super) fn start_export(&mut self) {
        let session = match self.viewing.session.as_ref() {
            Some(session) => session,
            None => return,
        };

        let safe_title: String = session
            .title
            .chars()
            .take(40)
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let source = self.source_label_for(&session.source);

        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
            .or_else(|| dirs::home_dir().map(|h| h.display().to_string()))
            .unwrap_or_default();
        self.export_path = format!("{cwd}/recall-{source}-{safe_title}.txt");
        self.export_cursor = self.export_path.len();
        self.mode = AppMode::ExportInput;
    }

    pub(super) fn handle_export_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Viewing;
                self.export_path.clear();
            }
            KeyCode::Enter => {
                let path = self.export_path.clone();
                self.mode = AppMode::Viewing;
                self.do_export(&path);
                self.export_path.clear();
            }
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Left | KeyCode::Right => {
                edit_text(&mut self.export_path, &mut self.export_cursor, key.code);
            }
            _ => {}
        }
    }

    fn do_export(&mut self, path: &str) {
        let session = match self.viewing.session.as_ref() {
            Some(session) => session,
            None => return,
        };

        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            let _ = std::fs::create_dir_all(parent);
        }

        let content = transcript::render_plain(session, &self.viewing.messages);

        match std::fs::write(path, &content) {
            Ok(_) => {
                self.status_message = Some(format!("Exported to {path}"));
            }
            Err(e) => {
                self.status_message = Some(format!("Export failed: {e}"));
            }
        }
    }
}
