use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum SyncCommands {
    #[command(about = "Manage scheduled global sync (macOS only)")]
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommands,
    },
}

#[derive(Subcommand)]
pub(crate) enum ScheduleCommands {
    #[command(about = "Install or reload a user LaunchAgent for this sync command")]
    Install {
        #[arg(long, default_value = "30m", value_parser = parse_interval, help = "Sync interval: integer seconds (s), minutes (m), or hours (h); minimum 60s")]
        interval: u32,
    },
    #[command(about = "Unload and remove the scheduled sync LaunchAgent; retain logs")]
    Uninstall,
    #[command(about = "Show installation, load state, binary path, and recent log activity")]
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    LocalSync,
    RemoteSync,
}

fn parse_interval(value: &str) -> std::result::Result<u32, String> {
    let invalid = || {
        "interval must be an integer followed by s, m, or h (for example 30m, 1h, or 900s), from 60s to 2147483647s".to_string()
    };
    let (number, multiplier) = match value.as_bytes().last() {
        Some(b's') => (&value[..value.len() - 1], 1),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 3600),
        _ => return Err(invalid()),
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    number
        .parse::<u32>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|seconds| (60..=i32::MAX as u32).contains(seconds))
        .ok_or_else(invalid)
}

pub(crate) fn run(command: ScheduleCommands, job: Job) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::run(command, job)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (command, job);
        unsupported(std::env::consts::OS)
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported(os: &str) -> Result<()> {
    anyhow::bail!("not supported yet on {os}")
}

#[cfg(target_os = "macos")]
mod macos;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_have_explicit_units_and_bounded_seconds() {
        for (value, expected) in [
            ("30m", 1800),
            ("1h", 3600),
            ("900s", 900),
            ("1m", 60),
            ("2147483647s", i32::MAX as u32),
        ] {
            assert_eq!(parse_interval(value).unwrap(), expected);
        }
        for value in [
            "",
            "30",
            "s",
            "0m",
            "59s",
            "1.5h",
            "1h30m",
            "-1m",
            "+1m",
            " 1m",
            "2147483648s",
            "4294967295h",
            "999999999999s",
            "时间",
        ] {
            assert!(parse_interval(value).is_err(), "accepted {value}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn all_schedule_actions_reject_the_current_platform() {
        for job in [Job::LocalSync, Job::RemoteSync] {
            for command in [
                ScheduleCommands::Install { interval: 1800 },
                ScheduleCommands::Uninstall,
                ScheduleCommands::Status,
            ] {
                assert!(run(command, job).is_err());
            }
        }
    }
}
