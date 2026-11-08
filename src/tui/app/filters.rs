use super::*;

impl App {
    pub(crate) fn source_filter_ids(&self) -> Option<Vec<String>> {
        let explicit = self.normalized_source_selection(&self.filters.active.sources);
        if !explicit.is_empty() {
            return Some(explicit);
        }

        let enabled = self.enabled_sources();
        if enabled.is_empty() {
            return None;
        }

        if enabled.len() == self.all_sources.len() {
            None
        } else {
            Some(enabled.into_iter().map(|(id, _)| id.clone()).collect())
        }
    }

    pub(crate) fn source_filter_label(&self) -> String {
        self.source_filter_label_for_selection(&self.filters.active.sources)
    }

    pub(crate) fn draft_source_filter_label(&self) -> String {
        self.source_filter_label_for_selection(&self.filters.draft.sources)
    }

    fn source_filter_label_for_selection(&self, selection: &[String]) -> String {
        let explicit = self.normalized_source_selection(selection);
        if explicit.is_empty() {
            return if self.enabled_sources().len() == self.all_sources.len() {
                "ALL".to_string()
            } else {
                "DEFAULT".to_string()
            };
        }

        if explicit.len() == 1 {
            return self.source_label_for(&explicit[0]).to_string();
        }

        let labels: Vec<&str> =
            explicit.iter().map(|id| self.source_label_for(id)).take(2).collect();
        if explicit.len() == 2 {
            labels.join(", ")
        } else {
            format!("{}, +{}", labels.join(", "), explicit.len() - labels.len())
        }
    }

    pub(crate) fn time_filter_label(&self) -> &'static str {
        Self::time_range_label(self.filters.active.time)
    }

    pub(crate) fn draft_time_filter_label(&self) -> &'static str {
        Self::time_range_label(self.filters.draft.time)
    }

    fn time_range_label(time_range: TimeRange) -> &'static str {
        match time_range {
            TimeRange::Today => "Today",
            TimeRange::Week => "7d",
            TimeRange::Month => "30d",
            TimeRange::All => "All",
        }
    }

    pub(crate) fn sort_label(&self) -> &'static str {
        Self::sort_order_label(self.effective_sort_order())
    }

    pub(super) fn effective_sort_order(&self) -> SortOrder {
        if self.query.trim().is_empty() { SortOrder::Newest } else { self.filters.active.sort }
    }

    pub(crate) fn draft_sort_label(&self) -> &'static str {
        Self::sort_order_label(self.filters.draft.sort)
    }

    fn sort_order_label(sort_order: SortOrder) -> &'static str {
        match sort_order {
            SortOrder::Relevance => "Relevance",
            SortOrder::Newest => "Newest",
        }
    }

    pub(crate) fn project_filter_label(&self) -> String {
        scope_label(&self.filters.active.scope)
    }

    pub(crate) fn draft_project_filter_label(&self) -> String {
        scope_label(&self.filters.draft.scope)
    }

    pub(crate) fn source_label_for<'a>(&'a self, source_id: &'a str) -> &'a str {
        self.all_sources
            .iter()
            .find(|(id, _)| id == source_id)
            .map(|(_, label)| label.as_str())
            .unwrap_or(source_id)
    }

    pub(crate) fn enabled_sources(&self) -> Vec<&(String, String)> {
        self.all_sources.iter().filter(|(id, _)| self.config.is_source_enabled(id)).collect()
    }

    pub(crate) fn source_is_selected_in_picker(&self, source_id: &str) -> bool {
        self.normalized_source_selection(&self.filters.sources).iter().any(|id| id == source_id)
    }

    pub(crate) fn source_picker_rows(&self) -> Vec<SourcePickerRow> {
        let query = self.filters.source_picker.query.trim().to_lowercase();
        let mut rows = Vec::new();
        if query.is_empty() {
            rows.push(SourcePickerRow::All);
        }

        for (index, (source_id, label)) in self.all_sources.iter().enumerate() {
            if !self.config.is_source_enabled(source_id) {
                continue;
            }

            if query.is_empty()
                || source_id.to_lowercase().contains(&query)
                || label.to_lowercase().contains(&query)
            {
                rows.push(SourcePickerRow::Source(index));
            }
        }

        rows
    }

    pub(crate) fn project_picker_rows(&self) -> Vec<ProjectPickerRow> {
        let query = self.filters.project_picker.query.trim().to_lowercase();
        let mut rows = Vec::new();
        if query.is_empty() {
            rows.push(ProjectPickerRow::All);
        }

        for (index, project) in self.project_directories.iter().enumerate() {
            if query.is_empty() || project_matches_query(&project.directory, &query) {
                rows.push(ProjectPickerRow::Project(index));
            }
        }

        rows
    }

    pub(super) fn normalized_source_selection(&self, selection: &[String]) -> Vec<String> {
        self.all_sources
            .iter()
            .filter(|(source_id, _)| {
                self.config.is_source_enabled(source_id)
                    && selection.iter().any(|selected| selected == source_id)
            })
            .map(|(source_id, _)| source_id.clone())
            .collect()
    }

    pub(super) fn reset_search_defaults(&mut self) {
        self.filters.active = FilterValues {
            time: self.config.sync_window.to_time_range(),
            ..FilterValues::new(crate::project_scope::auto_scope())
        };
        self.filters.reset_draft();
    }
}
