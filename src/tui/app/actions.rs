use super::*;

impl App {
    pub(super) fn start_resume_confirmation(&mut self, origin: ResumeOrigin, store: &Store) {
        self.start_source_command_confirmation(
            origin,
            session_action::SessionAction::Resume,
            PendingCommandAction::Resume,
            store,
        );
    }

    pub(super) fn start_app_open_confirmation(&mut self, origin: ResumeOrigin, store: &Store) {
        self.start_source_command_confirmation(
            origin,
            session_action::SessionAction::OpenApp,
            PendingCommandAction::OpenApp,
            store,
        );
    }

    fn start_source_command_confirmation(
        &mut self,
        origin: ResumeOrigin,
        source_action: session_action::SessionAction,
        action: PendingCommandAction,
        store: &Store,
    ) {
        let session = match origin {
            ResumeOrigin::Viewing => self.viewing.session.clone(),
            ResumeOrigin::Search => {
                self.results.get(self.selected_index).map(|r| r.session.clone())
            }
        };
        let Some(session) = session else {
            return;
        };
        if session.is_import || !store.has_native_binding(&session.id).unwrap_or(false) {
            self.status_message =
                Some("Imported session: not resumable on this machine".to_string());
            return;
        }
        let Some(command) =
            session_action::command_for(source_action, &session.source, &session.source_id)
        else {
            self.status_message =
                Some(format!("{} not supported for {}", source_action.title(), session.source));
            return;
        };
        self.pending_resume = Some(PendingResume {
            native_session_id: Some(session.id.clone()),
            command,
            action,
            source_label: self.source_label_for(&session.source).to_string(),
            session_title: session.title.clone(),
            cwd: session.directory.clone(),
            origin,
        });
        self.mode = AppMode::ConfirmResume;
    }

    pub(super) fn open_handoff_target_picker(&mut self) {
        self.present_handoff_targets(handoff::available_targets());
    }

    pub(super) fn present_handoff_targets(&mut self, targets: Vec<handoff::HandoffTarget>) {
        if targets.is_empty() {
            self.status_message = Some("No handoff targets found on PATH".to_string());
            return;
        }
        self.handoff_targets = targets;
        self.handoff_target_selected = 0;
        self.mode = AppMode::HandoffTarget;
    }

    pub(super) fn handle_handoff_target_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Viewing;
            }
            KeyCode::Up | KeyCode::Char('k') if self.handoff_target_selected > 0 => {
                self.handoff_target_selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j')
                if self.handoff_target_selected + 1 < self.handoff_targets.len() =>
            {
                self.handoff_target_selected += 1;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(target) = self.handoff_targets.get(self.handoff_target_selected).cloned()
                else {
                    return;
                };
                self.start_handoff_confirmation(&target);
            }
            _ => {}
        }
    }

    fn start_handoff_confirmation(&mut self, target: &handoff::HandoffTarget) {
        let Some(session) = self.viewing.session.as_ref() else {
            return;
        };
        let prompt = handoff::build_prompt(session, &self.viewing.messages);
        let command = match handoff::command_for_target(target, prompt) {
            Ok(command) => command,
            Err(error) => {
                self.status_message = Some(error.to_string());
                self.mode = AppMode::Viewing;
                return;
            }
        };
        self.pending_resume = Some(PendingResume {
            native_session_id: None,
            command,
            action: PendingCommandAction::Handoff,
            source_label: target.label.clone(),
            session_title: session.title.clone(),
            cwd: session.directory.clone(),
            origin: ResumeOrigin::Viewing,
        });
        self.mode = AppMode::ConfirmResume;
    }

    pub(super) fn handle_confirm_resume_key(&mut self, key: KeyEvent, store: &Store) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if let Some(pending) = self.pending_resume.take() {
                    if pending
                        .native_session_id
                        .as_deref()
                        .is_some_and(|id| !store.has_native_binding(id).unwrap_or(false))
                    {
                        self.status_message =
                            Some("Session is no longer native to this machine".into());
                        self.mode = AppMode::Search;
                        return;
                    }
                    self.exec_on_exit = Some((pending.command, pending.cwd));
                    self.should_quit = true;
                } else {
                    self.mode = AppMode::Search;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                let origin =
                    self.pending_resume.take().map(|p| p.origin).unwrap_or(ResumeOrigin::Search);
                self.mode = match origin {
                    ResumeOrigin::Search => AppMode::Search,
                    ResumeOrigin::Viewing => AppMode::Viewing,
                };
            }
            _ => {}
        }
    }
}
