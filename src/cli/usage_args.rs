use clap::Args;

use crate::wrapped::WrappedPeriod;

#[derive(Args)]
pub(crate) struct UsageArgs {
    #[arg(long, help = "Output usage report as JSON", conflicts_with = "card")]
    pub(crate) json: bool,
    #[arg(long, help = "Filter by comma-separated source ids or labels", conflicts_with = "card")]
    pub(crate) source: Option<String>,
    #[arg(
        long,
        value_parser = crate::query::parse_time_range_arg,
        help = "Filter by time range",
        conflicts_with = "card"
    )]
    pub(crate) time: Option<String>,
    #[arg(
        long,
        help = "Filter by a path, owner/repo, a remote URL, or all",
        conflicts_with = "projects"
    )]
    pub(crate) project: Option<String>,
    #[arg(
        long,
        help = "Group usage by project",
        conflicts_with_all = ["card", "project"]
    )]
    pub(crate) projects: bool,
    #[arg(long, help = "Print a shareable usage stats card")]
    pub(crate) card: bool,
    #[arg(
        long,
        value_enum,
        default_value_t = WrappedPeriod::Week,
        requires = "card",
        help = "Card time window"
    )]
    pub(crate) period: WrappedPeriod,
}
