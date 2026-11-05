use super::{Cli, Commands};
use clap::Parser;

#[test]
fn time_filter_validates_values_for_every_command() {
    let commands: &[&[&str]] = &[
        &["recall", "search", "query", "--time", "definitely-invalid"],
        &["recall", "session", "list", "--time", "definitely-invalid"],
        &["recall", "usage", "--time", "definitely-invalid"],
        &["recall", "export", "--time", "definitely-invalid"],
    ];

    for args in commands {
        let Err(error) = Cli::try_parse_from(*args) else {
            panic!("accepted invalid --time for {}", args[1]);
        };
        assert_eq!(error.exit_code(), 2);
    }

    for value in ["today", "7d", "week", "30d", "month", "all", "WEEK"] {
        assert!(Cli::try_parse_from(["recall", "usage", "--time", value]).is_ok());
    }
}

#[test]
fn unknown_subcommand_parses_as_external_extension() {
    let cli = Cli::try_parse_from(["recall", "reflect", "--limit", "3"]).unwrap();
    match cli.command {
        Some(Commands::External(args)) => {
            assert_eq!(args, ["reflect", "--limit", "3"]);
        }
        _ => panic!("expected external command"),
    }
}
