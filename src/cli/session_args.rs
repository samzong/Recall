use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

use crate::db::search::ThreadRoleFilter;

#[derive(Args)]
pub(crate) struct SessionSelector {
    #[arg(long, help = "Recall session id")]
    pub(crate) id: Option<String>,
    #[arg(long, help = "Source id or label")]
    pub(crate) source: Option<String>,
    #[arg(long, help = "Source-native session id")]
    pub(crate) source_id: Option<String>,
}

#[derive(Args)]
pub(crate) struct SessionListArgs {
    #[arg(long, help = "Search query text")]
    pub(crate) query: Option<String>,
    #[arg(long, help = "Filter by source id or label")]
    pub(crate) source: Option<String>,
    #[arg(long, value_parser = crate::query::parse_time_range_arg, help = "Filter by time range")]
    pub(crate) time: Option<String>,
    #[arg(long, help = "Filter by project directory, including child paths")]
    pub(crate) project: Option<String>,
    #[arg(long, help = "Filter by repository identity")]
    pub(crate) repo: Option<String>,
    #[arg(long, value_enum, help = "Filter by topology thread role")]
    pub(crate) thread_role: Option<ThreadRoleFilter>,
    #[arg(long, default_value_t = 50, help = "Maximum sessions to return")]
    pub(crate) limit: usize,
    #[arg(long, default_value_t = 0, help = "Skip sessions for pagination")]
    pub(crate) offset: usize,
    #[arg(long, help = "Return all matching sessions")]
    pub(crate) all: bool,
    #[arg(long, help = "Run incremental sync before listing")]
    pub(crate) sync: bool,
    #[arg(long, value_enum, help = "Sort order")]
    pub(crate) sort: Option<SessionSort>,
    #[arg(long, value_enum, default_value_t = SessionListFormat::Table)]
    pub(crate) format: SessionListFormat,
}

#[derive(Subcommand)]
pub(crate) enum SessionCommands {
    #[command(about = "List indexed sessions")]
    List(SessionListArgs),
    #[command(about = "Show one indexed session")]
    Show {
        #[command(flatten)]
        selector: SessionSelector,
        #[arg(long, help = "Include messages in structured output")]
        messages: bool,
        #[arg(long, help = "Comma-separated: metadata,messages,usage,events")]
        include: Option<String>,
        #[arg(long, help = "First message sequence to include")]
        from_seq: Option<u32>,
        #[arg(long, help = "Last message sequence to include")]
        to_seq: Option<u32>,
        #[arg(long, conflicts_with_all = ["from_seq", "to_seq", "cursor"], help = "Show a message and its neighbors (default 3 before and 3 after)")]
        around_seq: Option<u32>,
        #[arg(long, requires = "around_seq", help = "Number of messages before the anchor")]
        before: Option<u32>,
        #[arg(long, requires = "around_seq", help = "Number of messages after the anchor")]
        after: Option<u32>,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=32000), help = "Page message content within this Unicode character budget")]
        max_chars: Option<u32>,
        #[arg(long, conflicts_with_all = ["from_seq", "to_seq", "around_seq", "before", "after", "role"], help = "Continue a previous message page")]
        cursor: Option<String>,
        #[arg(long, value_enum, default_value_t = SessionRoleFilter::All)]
        role: SessionRoleFilter,
        #[arg(long, value_enum, default_value_t = SessionDetailFormat::Text)]
        format: SessionDetailFormat,
    },
    #[command(about = "Export selected sessions")]
    Export {
        #[arg(long = "id", help = "Recall session id; may be repeated")]
        ids: Vec<String>,
        #[arg(long, help = "Source id or label")]
        source: Option<String>,
        #[arg(long, help = "Source-native session id")]
        source_id: Option<String>,
        #[arg(long, help = "File containing newline-delimited session ids")]
        ids_file: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = SessionExportFormat::Jsonl)]
        format: SessionExportFormat,
        #[arg(
            long,
            help = "Comma-separated JSONL fields; messages is required: metadata,messages,usage,events"
        )]
        include: Option<String>,
        #[arg(long, help = "Output path; stdout when omitted")]
        output: Option<PathBuf>,
    },
    #[command(about = "Share one selected session")]
    Share {
        #[command(flatten)]
        selector: SessionSelector,
        #[arg(long, help = "Validate and render metadata without deploying")]
        dry_run: bool,
        #[arg(long, help = "Open the resulting URL")]
        open: bool,
        #[arg(long, help = "Copy the resulting URL to clipboard")]
        copy_url: bool,
        #[arg(long, help = "Markdown file to render as the share page TL;DR")]
        tldr_file: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = SessionActionFormat::Text)]
        format: SessionActionFormat,
    },
    #[command(about = "Resume one selected session in its source CLI")]
    Resume {
        #[command(flatten)]
        selector: SessionSelector,
        #[arg(long, help = "Print the command instead of executing it")]
        print_command: bool,
        #[arg(long, value_enum, default_value_t = SessionActionFormat::Text)]
        format: SessionActionFormat,
    },
    #[command(about = "Open one selected session in its source app")]
    Open {
        #[command(flatten)]
        selector: SessionSelector,
        #[arg(long, help = "Print the command instead of executing it")]
        print_command: bool,
        #[arg(long, value_enum, default_value_t = SessionActionFormat::Text)]
        format: SessionActionFormat,
    },
    #[command(about = "Handoff one selected session to a new target agent session")]
    Handoff {
        #[command(flatten)]
        selector: SessionSelector,
        #[arg(long, help = "Target agent id")]
        to: String,
        #[arg(long, help = "Print the handoff prompt instead of executing the target")]
        print_prompt: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionListFormat {
    Table,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionDetailFormat {
    Text,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionExportFormat {
    Jsonl,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionActionFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionSort {
    Newest,
    Oldest,
    Updated,
    Relevance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionRoleFilter {
    All,
    User,
    Assistant,
}
