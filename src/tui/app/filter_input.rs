use super::*;

impl App {
    pub(super) fn handle_filters_key(&mut self, key: KeyEvent, store: &Store) {
        if self.filters.editing.is_some() {
            self.handle_picker_key(key, store);
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.close_filters();
            }
            KeyCode::Up => self.handle_scroll_up(store),
            KeyCode::Down => self.handle_scroll_down(store),
            KeyCode::Left => {
                self.adjust_filter_value(false);
            }
            KeyCode::Right => {
                self.adjust_filter_value(true);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.activate_filter_row(store);
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.filters.clear_filters();
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.filters.set_time_filter(TimeRange::Today);
            }
            KeyCode::Char('w') | KeyCode::Char('W') => {
                self.filters.set_time_filter(TimeRange::Week);
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                self.filters.set_time_filter(TimeRange::Month);
            }
            KeyCode::Char('l') | KeyCode::Char('L') => {
                self.filters.set_time_filter(TimeRange::All);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.filters.set_sort_order(SortOrder::Relevance);
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.filters.set_sort_order(SortOrder::Newest);
            }
            _ => {}
        }
    }

    fn activate_filter_row(&mut self, store: &Store) {
        match self.filters.focus {
            FilterFocus::Source => {
                self.open_source_picker();
            }
            FilterFocus::Project => {
                self.open_project_picker(store);
            }
            FilterFocus::Time | FilterFocus::Sort => {}
        }
    }

    fn close_filters(&mut self) {
        self.mode = AppMode::Search;
        if self.filters.commit() {
            self.invalidate_active_search();
            self.queue_search_with_feedback("Filters queued...");
        }
    }

    fn adjust_filter_value(&mut self, forward: bool) {
        match self.filters.focus {
            FilterFocus::Time => self.filters.cycle_time_filter(forward),
            FilterFocus::Sort => self.filters.cycle_sort_order(),
            FilterFocus::Source | FilterFocus::Project => {}
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent, store: &Store) {
        let project = self.filters.editing == Some(FilterFocus::Project);
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('u') => {
                    self.filters.picker_mut().clear_query();
                    return;
                }
                KeyCode::Char('a') => {
                    if project {
                        self.filters.project = None;
                    } else {
                        self.filters.sources.clear();
                    }
                    self.filters.picker_mut().dirty = true;
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc if self.filters.picker().typing => {
                self.filters.picker_mut().typing = false
            }
            KeyCode::Esc => {
                if project {
                    self.close_project_picker();
                } else {
                    self.close_source_picker();
                }
            }
            KeyCode::Enter => {
                if project {
                    self.apply_project_picker();
                } else {
                    self.apply_source_picker();
                }
            }
            KeyCode::Char(' ') => {
                if project {
                    self.toggle_project_picker_row();
                } else {
                    self.toggle_source_picker_row();
                }
            }
            KeyCode::Char('/') if !self.filters.picker().typing => {
                self.filters.picker_mut().typing = true
            }
            KeyCode::Up => self.handle_scroll_up(store),
            KeyCode::Down => self.handle_scroll_down(store),
            _ => self.filters.picker_mut().handle_text_key(key),
        }
    }

    pub(super) fn open_filters(&mut self) {
        self.mode = AppMode::Filters;
        self.filters.open();
    }

    fn open_source_picker(&mut self) {
        self.mode = AppMode::Filters;
        self.filters.editing = Some(FilterFocus::Source);
        self.filters.source_picker.reset();
        self.filters.sources = self.normalized_source_selection(&self.filters.draft.sources);
        if let Some(selected_source) = self.filters.sources.first() {
            self.filters.source_picker.selected = self
                .source_picker_rows()
                .iter()
                .position(|row| match row {
                    SourcePickerRow::All => false,
                    SourcePickerRow::Source(index) => self
                        .all_sources
                        .get(*index)
                        .map(|(source_id, _)| source_id == selected_source)
                        .unwrap_or(false),
                })
                .unwrap_or(0);
        }
    }

    fn close_source_picker(&mut self) {
        self.filters.editing = None;
        self.filters.source_picker.reset();
        self.filters.sources.clear();
    }

    fn open_project_picker(&mut self, store: &Store) {
        self.project_directories = store.list_project_directories().unwrap_or_default();
        self.mode = AppMode::Filters;
        self.filters.editing = Some(FilterFocus::Project);
        self.filters.project_picker.reset();
        self.filters.project = match &self.filters.draft.scope {
            ProjectScope::Directory(directory) => Some(directory.clone()),
            ProjectScope::Global | ProjectScope::Repository { .. } => None,
        };
        if let Some(selected_project) = self.filters.project.clone().as_ref() {
            self.filters.project_picker.selected = self
                .project_picker_rows()
                .iter()
                .position(|row| match row {
                    ProjectPickerRow::All => false,
                    ProjectPickerRow::Project(index) => self
                        .project_directories
                        .get(*index)
                        .map(|project| &project.directory == selected_project)
                        .unwrap_or(false),
                })
                .unwrap_or(0);
        }
    }

    fn close_project_picker(&mut self) {
        self.filters.editing = None;
        self.filters.project_picker.reset();
        self.filters.project = None;
    }

    fn apply_project_picker(&mut self) {
        let previous_scope = self.filters.draft.scope.clone();
        self.commit_project_picker_filter();
        self.close_project_picker();
        self.mode = AppMode::Filters;
        if self.filters.draft.scope != previous_scope {
            self.filters.dirty = true;
        }
    }

    fn commit_project_picker_filter(&mut self) {
        if self.filters.project_picker.dirty {
            self.filters.draft.scope = match self.filters.project.clone() {
                Some(directory) => ProjectScope::Directory(directory),
                None => ProjectScope::Global,
            };
        } else if let Some(row) =
            self.project_picker_rows().get(self.filters.project_picker.selected)
        {
            match *row {
                ProjectPickerRow::All => self.filters.draft.scope = ProjectScope::Global,
                ProjectPickerRow::Project(index) => {
                    if let Some(project) = self.project_directories.get(index) {
                        self.filters.draft.scope =
                            ProjectScope::Directory(project.directory.clone());
                    }
                }
            }
        }
    }

    pub(super) fn toggle_project_picker_row(&mut self) {
        let Some(row) =
            self.project_picker_rows().get(self.filters.project_picker.selected).copied()
        else {
            return;
        };

        match row {
            ProjectPickerRow::All => {
                self.filters.project = None;
            }
            ProjectPickerRow::Project(index) => {
                let Some(project) = self.project_directories.get(index) else {
                    return;
                };
                if self.filters.project.as_deref() == Some(project.directory.as_str()) {
                    self.filters.project = None;
                } else {
                    self.filters.project = Some(project.directory.clone());
                }
            }
        }
        self.filters.project_picker.dirty = true;
    }

    fn apply_source_picker(&mut self) {
        let previous = self.filters.draft.sources.clone();
        self.commit_source_picker_filter();

        self.filters.source_picker.reset();
        self.filters.sources.clear();
        self.filters.editing = None;
        self.mode = AppMode::Filters;
        if self.filters.draft.sources != previous {
            self.filters.dirty = true;
        }
    }

    pub(super) fn commit_source_picker_filter(&mut self) {
        let confirming_existing_multi_selection = !self.filters.source_picker.dirty
            && self.filters.source_picker.query.trim().is_empty()
            && self.filters.sources.len() > 1;

        if self.filters.source_picker.dirty || confirming_existing_multi_selection {
            self.filters.draft.sources = self.normalized_source_selection(&self.filters.sources);
        } else if let Some(row) = self.source_picker_rows().get(self.filters.source_picker.selected)
        {
            match *row {
                SourcePickerRow::All => {
                    self.filters.draft.sources.clear();
                }
                SourcePickerRow::Source(index) => {
                    if let Some((source_id, _)) = self.all_sources.get(index) {
                        self.filters.draft.sources = vec![source_id.clone()];
                    }
                }
            }
        }
    }

    pub(super) fn toggle_source_picker_row(&mut self) {
        let Some(row) = self.source_picker_rows().get(self.filters.source_picker.selected).copied()
        else {
            return;
        };

        match row {
            SourcePickerRow::All => {
                self.filters.sources.clear();
            }
            SourcePickerRow::Source(index) => {
                let Some((source_id, _)) = self.all_sources.get(index) else {
                    return;
                };
                if let Some(pos) = self.filters.sources.iter().position(|id| id == source_id) {
                    self.filters.sources.remove(pos);
                } else {
                    self.filters.sources.push(source_id.clone());
                }
                self.filters.sources = self.normalized_source_selection(&self.filters.sources);
            }
        }
        self.filters.source_picker.dirty = true;
    }
}
