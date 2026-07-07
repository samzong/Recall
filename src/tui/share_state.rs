use crate::adapters::ResumeCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingCommandAction {
    Resume,
    OpenApp,
    Handoff,
}

pub(crate) enum AppMode {
    Search,
    Usage,
    Viewing,
    ShareResult,
    ExportInput,
    Settings,
    Filters,
    HandoffTarget,
    ConfirmResume,
}

#[derive(Clone, Copy)]
pub(crate) enum ResumeOrigin {
    Search,
    Viewing,
}

pub(crate) struct PendingResume {
    pub(crate) command: ResumeCommand,
    pub(crate) action: PendingCommandAction,
    pub(crate) source_label: String,
    pub(crate) session_title: String,
    pub(crate) cwd: Option<String>,
    pub(crate) origin: ResumeOrigin,
}

pub(crate) struct SharePopup {
    pub(crate) url: Option<String>,
    pub(crate) message: String,
    pub(crate) is_error: bool,
}
