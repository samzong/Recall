use crate::db::search::TimeRange;
use crate::project_scope::ProjectScope;

#[derive(PartialEq)]
pub(crate) enum PanelFocus {
    SessionList,
    Preview,
}

pub(crate) enum SearchMouseTarget {
    SessionList(Option<usize>),
    Preview,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FilterFocus {
    Source,
    Project,
    Time,
    Sort,
}

impl FilterFocus {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Source => Self::Project,
            Self::Project => Self::Time,
            Self::Time => Self::Sort,
            Self::Sort => Self::Source,
        }
    }

    pub(crate) fn previous(self) -> Self {
        match self {
            Self::Source => Self::Sort,
            Self::Project => Self::Source,
            Self::Time => Self::Project,
            Self::Sort => Self::Time,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SourcePickerRow {
    All,
    Source(usize),
}

#[derive(Clone, Copy)]
pub(crate) enum ProjectPickerRow {
    All,
    Project(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SortOrder {
    Relevance,
    Newest,
}

#[derive(Clone, PartialEq)]
pub(crate) struct FilterValues {
    pub(crate) sources: Vec<String>,
    pub(crate) scope: ProjectScope,
    pub(crate) time: TimeRange,
    pub(crate) sort: SortOrder,
}

impl FilterValues {
    pub(crate) fn new(scope: ProjectScope) -> Self {
        Self { sources: Vec::new(), scope, time: TimeRange::All, sort: SortOrder::Relevance }
    }
}

pub(crate) struct FilterState {
    pub(crate) active: FilterValues,
    pub(crate) draft: FilterValues,
    pub(crate) focus: FilterFocus,
    pub(crate) dirty: bool,
    pub(crate) editing: Option<FilterFocus>,
    pub(crate) source_picker: PickerState,
    pub(crate) sources: Vec<String>,
    pub(crate) project_picker: PickerState,
    pub(crate) project: Option<String>,
}

impl FilterState {
    pub(crate) fn picker(&self) -> &PickerState {
        if self.editing == Some(FilterFocus::Project) {
            &self.project_picker
        } else {
            &self.source_picker
        }
    }

    pub(crate) fn picker_mut(&mut self) -> &mut PickerState {
        if self.editing == Some(FilterFocus::Project) {
            &mut self.project_picker
        } else {
            &mut self.source_picker
        }
    }

    pub(crate) fn new(scope: ProjectScope) -> Self {
        let active = FilterValues::new(scope);
        Self {
            draft: active.clone(),
            active,
            focus: FilterFocus::Source,
            dirty: false,
            editing: None,
            source_picker: PickerState::default(),
            sources: Vec::new(),
            project_picker: PickerState::default(),
            project: None,
        }
    }

    pub(crate) fn reset_draft(&mut self) {
        self.draft = self.active.clone();
        self.dirty = false;
    }

    pub(crate) fn open(&mut self) {
        self.editing = None;
        self.reset_draft();
    }

    pub(crate) fn commit(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        self.active = self.draft.clone();
        self.dirty = false;
        true
    }
    pub(crate) fn set_time_filter(&mut self, time_filter: TimeRange) {
        if self.draft.time != time_filter {
            self.draft.time = time_filter;
            self.dirty = true;
        }
    }
    pub(crate) fn cycle_time_filter(&mut self, forward: bool) {
        let next = match (self.draft.time, forward) {
            (TimeRange::All, true) => TimeRange::Today,
            (TimeRange::Today, true) => TimeRange::Week,
            (TimeRange::Week, true) => TimeRange::Month,
            (TimeRange::Month, true) => TimeRange::All,
            (TimeRange::All, false) => TimeRange::Month,
            (TimeRange::Month, false) => TimeRange::Week,
            (TimeRange::Week, false) => TimeRange::Today,
            (TimeRange::Today, false) => TimeRange::All,
        };
        self.set_time_filter(next);
    }
    pub(crate) fn set_sort_order(&mut self, sort_order: SortOrder) {
        if self.draft.sort != sort_order {
            self.draft.sort = sort_order;
            self.dirty = true;
        }
    }
    pub(crate) fn cycle_sort_order(&mut self) {
        let next = match self.draft.sort {
            SortOrder::Relevance => SortOrder::Newest,
            SortOrder::Newest => SortOrder::Relevance,
        };
        self.set_sort_order(next);
    }
    pub(crate) fn clear_filters(&mut self) {
        let was_filtered = !self.draft.sources.is_empty()
            || self.draft.scope != ProjectScope::Global
            || self.draft.time != TimeRange::All
            || self.draft.sort != SortOrder::Relevance;
        self.draft.sources.clear();
        self.draft.scope = ProjectScope::Global;
        self.draft.time = TimeRange::All;
        self.draft.sort = SortOrder::Relevance;
        self.focus = FilterFocus::Source;
        if was_filtered {
            self.dirty = true;
        }
    }
}

#[derive(Default)]
pub(crate) struct PickerState {
    pub(crate) query: String,
    pub(crate) cursor: usize,
    pub(crate) selected: usize,
    pub(crate) dirty: bool,
    pub(crate) typing: bool,
}

impl PickerState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
    pub(crate) fn clear_query(&mut self) {
        self.query.clear();
        self.cursor = 0;
        self.selected = 0;
        self.typing = true;
    }
}
