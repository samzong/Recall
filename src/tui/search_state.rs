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
