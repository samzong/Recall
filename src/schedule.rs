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
    #[command(about = "Install or reload a user LaunchAgent for recall sync --project all")]
    Install {
        #[arg(long, default_value = "30m", value_parser = parse_interval, help = "Sync interval: integer seconds (s), minutes (m), or hours (h); minimum 60s")]
        interval: u32,
    },
    #[command(about = "Unload and remove the scheduled sync LaunchAgent; retain logs")]
    Uninstall,
    #[command(about = "Show installation, load state, binary path, and recent log activity")]
    Status,
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

pub(crate) fn run(command: ScheduleCommands) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::run(command)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = command;
        unsupported(std::env::consts::OS)
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported(os: &str) -> Result<()> {
    anyhow::bail!("not supported yet on {os}")
}

#[cfg(target_os = "macos")]
mod macos {
    use super::ScheduleCommands;
    use anyhow::{Context, Result};
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    const LABEL: &str = "io.github.samzong.recall.sync";

    struct Agent {
        plist: PathBuf,
        stdout: PathBuf,
        stderr: PathBuf,
        domain: String,
    }

    impl Agent {
        fn new() -> Result<Self> {
            let home = dirs::home_dir().context("cannot determine home directory")?;
            let uid = checked(Command::new("/usr/bin/id").arg("-u"))?;
            let uid = std::str::from_utf8(&uid.stdout)?.trim().parse::<u32>()?;
            let logs = home.join("Library/Logs/recall");
            Ok(Self {
                plist: home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")),
                stdout: logs.join("sync.stdout.log"),
                stderr: logs.join("sync.stderr.log"),
                domain: format!("gui/{uid}"),
            })
        }

        fn target(&self) -> String {
            format!("{}/{LABEL}", self.domain)
        }

        fn loaded_job(&self) -> Result<Option<String>> {
            let output = Command::new("/bin/launchctl").args(["print", &self.target()]).output()?;
            if output.status.success() {
                Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
            } else if output.status.code() == Some(113) {
                Ok(None)
            } else {
                command_error("launchctl print", &output)
            }
        }

        fn bootout(&self) -> Result<()> {
            if self.loaded_job()?.is_some() {
                checked(Command::new("/bin/launchctl").args(["bootout", &self.target()]))?;
            }
            Ok(())
        }

        fn install(&self, interval: u32) -> Result<()> {
            let binary = std::env::current_exe().context("cannot resolve Recall executable")?;
            let plist = render_plist(&binary, interval, &self.stdout, &self.stderr)?;
            let directory = self.plist.parent().context("LaunchAgent path has no parent")?;
            fs::create_dir_all(directory)?;
            fs::create_dir_all(self.stdout.parent().context("log path has no parent")?)?;
            let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
            temporary.write_all(plist.as_bytes())?;
            checked(Command::new("/usr/bin/plutil").arg("-lint").arg(temporary.path()))?;
            self.bootout()?;
            temporary.persist(&self.plist).context("cannot write LaunchAgent plist")?;
            checked(Command::new("/bin/launchctl").args(["bootstrap", &self.domain]).arg(&self.plist))
                .with_context(|| format!("could not load {}; inspect `recall sync schedule status` or remove it with `recall sync schedule uninstall`", self.plist.display()))?;
            anyhow::ensure!(
                self.loaded_job()?.is_some(),
                "LaunchAgent was not loaded after bootstrap"
            );
            checked(Command::new("/bin/launchctl").args(["kickstart", &self.target()]))
                .context("could not start the first sync; inspect `recall sync schedule status` or remove it with `recall sync schedule uninstall`")?;
            println!(
                "Installed: {LABEL}\nInterval: {interval}s\nBinary: {}\nPlist: {}\nStdout: {}\nStderr: {}\nFirst sync: started now; macOS may ask for access to multiple folders in sequence (Documents, Downloads, Desktop, or other volumes). Click Allow within {} seconds for each prompt; if the first sync fails, rerun `recall sync schedule install`.",
                binary.display(),
                self.plist.display(),
                self.stdout.display(),
                self.stderr.display(),
                crate::repo_identity::GIT_METADATA_PROBE_TIMEOUT.as_secs()
            );
            Ok(())
        }

        fn uninstall(&self) -> Result<()> {
            self.bootout()?;
            match fs::remove_file(&self.plist) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            println!(
                "Not installed: {LABEL}\nLogs retained: {}",
                self.stdout.parent().unwrap().display()
            );
            Ok(())
        }

        fn status(&self) -> Result<()> {
            let job = self.loaded_job()?;
            let installed = self.plist.try_exists()?;
            println!(
                "{}: {LABEL}\nLoaded: {}\nPlist: {}",
                if installed { "Installed" } else { "Not installed" },
                if job.is_some() { "yes" } else { "no" },
                self.plist.display()
            );
            if let Some(job) = job {
                println!("Last exit code: {}", last_exit_code(&job).unwrap_or("unavailable"));
            }
            if installed {
                let output = checked(
                    Command::new("/usr/bin/plutil")
                        .args(["-convert", "json", "-o", "-"])
                        .arg(&self.plist),
                )?;
                let definition: Definition = serde_json::from_slice(&output.stdout)
                    .context("cannot read scheduled sync configuration")?;
                let binary = definition
                    .program_arguments
                    .first()
                    .context("LaunchAgent has no binary path")?;
                println!(
                    "Interval: {}s\nBinary: {binary}\nBinary exists: {}",
                    definition.start_interval,
                    if Path::new(binary).try_exists()? { "yes" } else { "no" }
                );
                log_status("Stdout", &definition.standard_out_path)?;
                log_status("Stderr", &definition.standard_error_path)?;
            } else {
                log_status("Stdout", &self.stdout)?;
                log_status("Stderr", &self.stderr)?;
            }
            Ok(())
        }
    }

    fn last_exit_code(job: &str) -> Option<&str> {
        job.lines().find_map(|line| line.trim().strip_prefix("last exit code = "))
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Definition {
        program_arguments: Vec<String>,
        start_interval: u32,
        standard_out_path: PathBuf,
        standard_error_path: PathBuf,
    }

    fn log_status(name: &str, path: &Path) -> Result<()> {
        let modified = match fs::metadata(path) {
            Ok(metadata) => {
                chrono::DateTime::<chrono::Utc>::from(metadata.modified()?).to_rfc3339()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "none".to_string(),
            Err(error) => return Err(error.into()),
        };
        println!("{name}: {}\n{name} last modified: {modified}", path.display());
        Ok(())
    }

    fn checked(command: &mut Command) -> Result<Output> {
        let output = command
            .output()
            .with_context(|| format!("cannot run {}", command.get_program().to_string_lossy()))?;
        if !output.status.success() {
            return command_error(&command.get_program().to_string_lossy(), &output);
        }
        Ok(output)
    }

    fn command_error<T>(name: &str, output: &Output) -> Result<T> {
        anyhow::bail!(
            "{name} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }

    fn xml_path(path: &Path) -> Result<String> {
        let text = path.to_str().context("LaunchAgent paths must be valid UTF-8")?;
        anyhow::ensure!(
            text.chars().all(|c| matches!(c, '\t' | '\n' | '\r')
                || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}')),
            "LaunchAgent path contains a character unsupported by XML"
        );
        Ok(text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\r', "&#13;"))
    }

    fn render_plist(binary: &Path, interval: u32, stdout: &Path, stderr: &Path) -> Result<String> {
        anyhow::ensure!(binary.is_absolute(), "Recall executable path must be absolute");
        let binary = xml_path(binary)?;
        let stdout = xml_path(stdout)?;
        let stderr = xml_path(stderr)?;
        Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>{binary}</string><string>sync</string><string>--project</string><string>all</string></array>
    <key>StartInterval</key><integer>{interval}</integer>
    <key>KeepAlive</key><false/>
    <key>AbandonProcessGroup</key><true/>
    <key>StandardOutPath</key><string>{stdout}</string>
    <key>StandardErrorPath</key><string>{stderr}</string>
    <key>Umask</key><integer>63</integer>
</dict>
</plist>
"#
        ))
    }

    pub(super) fn run(command: ScheduleCommands) -> Result<()> {
        let agent = Agent::new()?;
        match command {
            ScheduleCommands::Install { interval } => agent.install(interval),
            ScheduleCommands::Uninstall => agent.uninstall(),
            ScheduleCommands::Status => agent.status(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn launchctl_exit_status_preserves_codes_and_pending_state() {
            for value in ["0", "7", "(never exited)"] {
                let job = format!(
                    "gui/501/recall = {{\n\tstate = not running\n\tlast exit code = {value}\n}}"
                );
                assert_eq!(last_exit_code(&job), Some(value));
            }
            assert_eq!(last_exit_code("state = running\nlast terminating signal = 9"), None);
        }

        #[test]
        fn plist_roundtrips_paths_and_global_sync_arguments() {
            let binary = Path::new("/Applications/Recall & <Tools>/recall");
            let stdout = Path::new("/Users/example/Library/Logs/recall/out\r.log");
            let stderr = Path::new("/Users/example/Library/Logs/recall/err.log");
            let plist = render_plist(binary, 1800, stdout, stderr).unwrap();
            let file = tempfile::NamedTempFile::new().unwrap();
            fs::write(file.path(), plist).unwrap();
            let output = checked(
                Command::new("/usr/bin/plutil")
                    .args(["-convert", "json", "-o", "-"])
                    .arg(file.path()),
            )
            .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["Label"], LABEL);
            assert_eq!(
                value["ProgramArguments"],
                serde_json::json!([binary.to_str().unwrap(), "sync", "--project", "all"])
            );
            assert_eq!(value["StartInterval"], 1800);
            assert_eq!(value["KeepAlive"], false);
            assert_eq!(value["AbandonProcessGroup"], true);
            assert_eq!(value["Umask"], 0o77);
            let definition: Definition = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(definition.standard_out_path, stdout);
            assert_eq!(definition.standard_error_path, stderr);
            assert!(render_plist(Path::new("recall"), 1800, stdout, stderr).is_err());
            assert!(render_plist(Path::new("/bad\u{1}/recall"), 1800, stdout, stderr).is_err());
        }
    }
}

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
        for command in [
            ScheduleCommands::Install { interval: 1800 },
            ScheduleCommands::Uninstall,
            ScheduleCommands::Status,
        ] {
            assert!(run(command).is_err());
        }
    }
}
