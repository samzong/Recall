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
mod macos {
    use super::{Job, ScheduleCommands};
    use anyhow::{Context, Result};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    impl Job {
        fn label(self) -> &'static str {
            match self {
                Self::LocalSync => "io.github.samzong.recall.sync",
                Self::RemoteSync => "io.github.samzong.recall.remote-sync",
            }
        }

        fn log_name(self) -> &'static str {
            match self {
                Self::LocalSync => "sync",
                Self::RemoteSync => "remote-sync",
            }
        }

        fn arguments(self) -> &'static [&'static str] {
            match self {
                Self::LocalSync => &["sync", "--project", "all"],
                Self::RemoteSync => &["remote", "sync"],
            }
        }

        fn command(self) -> &'static str {
            match self {
                Self::LocalSync => "recall sync schedule",
                Self::RemoteSync => "recall remote sync schedule",
            }
        }
    }

    struct Agent {
        job: Job,
        plist: PathBuf,
        stdout: PathBuf,
        stderr: PathBuf,
        domain: String,
    }

    impl Agent {
        fn new(job: Job) -> Result<Self> {
            let home = dirs::home_dir().context("cannot determine home directory")?;
            let uid = checked(Command::new("/usr/bin/id").arg("-u"))?;
            let uid = std::str::from_utf8(&uid.stdout)?.trim().parse::<u32>()?;
            Ok(Self::for_home(job, &home, format!("gui/{uid}")))
        }

        fn for_home(job: Job, home: &Path, domain: String) -> Self {
            let logs = home.join("Library/Logs/recall");
            Self {
                job,
                plist: home.join("Library/LaunchAgents").join(format!("{}.plist", job.label())),
                stdout: logs.join(format!("{}.stdout.log", job.log_name())),
                stderr: logs.join(format!("{}.stderr.log", job.log_name())),
                domain,
            }
        }

        fn target(&self) -> String {
            format!("{}/{}", self.domain, self.job.label())
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
            let environment = job_environment(self.job)?;
            let plist = render_plist(
                self.job,
                &binary,
                interval,
                &self.stdout,
                &self.stderr,
                &environment,
            )?;
            let command = self.job.command();
            let label = self.job.label();
            let directory = self.plist.parent().context("LaunchAgent path has no parent")?;
            fs::create_dir_all(directory)?;
            fs::create_dir_all(self.stdout.parent().context("log path has no parent")?)?;
            let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
            temporary.write_all(plist.as_bytes())?;
            checked(Command::new("/usr/bin/plutil").arg("-lint").arg(temporary.path()))?;
            self.bootout()?;
            temporary.persist(&self.plist).context("cannot write LaunchAgent plist")?;
            checked(Command::new("/bin/launchctl").args(["bootstrap", &self.domain]).arg(&self.plist))
                .with_context(|| format!("could not load {}; inspect `{command} status` or remove it with `{command} uninstall`", self.plist.display()))?;
            anyhow::ensure!(
                self.loaded_job()?.is_some(),
                "LaunchAgent was not loaded after bootstrap"
            );
            checked(Command::new("/bin/launchctl").args(["kickstart", &self.target()]))
                .with_context(|| format!("could not start the first sync; inspect `{command} status` or remove it with `{command} uninstall`"))?;
            println!(
                "Installed: {label}\nInterval: {interval}s\nBinary: {}\nPlist: {}\nStdout: {}\nStderr: {}\nFirst sync: started now; macOS may ask for access to multiple folders in sequence (Documents, Downloads, Desktop, or other volumes). Click Allow within {} seconds for each prompt; if the first sync fails, rerun `{command} install`.",
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
                "Not installed: {}\nLogs retained: {}",
                self.job.label(),
                self.stdout.parent().unwrap().display()
            );
            Ok(())
        }

        fn status(&self) -> Result<()> {
            let job = self.loaded_job()?;
            let installed = self.plist.try_exists()?;
            println!(
                "{}: {}\nLoaded: {}\nPlist: {}",
                if installed { "Installed" } else { "Not installed" },
                self.job.label(),
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
        xml_text(text)
    }

    fn xml_text(text: &str) -> Result<String> {
        anyhow::ensure!(
            text.chars().all(|c| matches!(c, '\t' | '\n' | '\r')
                || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}')),
            "LaunchAgent value contains a character unsupported by XML"
        );
        Ok(text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\r', "&#13;"))
    }

    fn job_environment(job: Job) -> Result<BTreeMap<&'static str, String>> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        job_environment_with(job, &home, |name| std::env::var_os(name))
    }

    fn job_environment_with(
        job: Job,
        home: &Path,
        variable: impl Fn(&str) -> Option<OsString>,
    ) -> Result<BTreeMap<&'static str, String>> {
        let mut environment = BTreeMap::from([(
            "HOME",
            home.to_str().context("home directory must be valid UTF-8")?.to_string(),
        )]);
        if job == Job::LocalSync {
            return Ok(environment);
        }
        environment.insert("PATH", "/usr/bin:/bin:/usr/sbin:/sbin".to_string());
        for name in ["AWS_SHARED_CREDENTIALS_FILE", "AWS_CONFIG_FILE"] {
            if let Some(value) = variable(name) {
                environment.insert(name, aws_file_path(Path::new(&value), home)?);
            }
        }
        Ok(environment)
    }

    fn aws_file_path(value: &Path, home: &Path) -> Result<String> {
        let path = if value.as_os_str().is_empty() {
            PathBuf::new()
        } else if let Ok(suffix) = value.strip_prefix("~") {
            home.join(suffix)
        } else {
            std::path::absolute(value)?
        };
        Ok(path.to_str().context("AWS file path must be valid UTF-8")?.to_string())
    }

    fn render_plist(
        job: Job,
        binary: &Path,
        interval: u32,
        stdout: &Path,
        stderr: &Path,
        environment: &BTreeMap<&str, String>,
    ) -> Result<String> {
        anyhow::ensure!(binary.is_absolute(), "Recall executable path must be absolute");
        let binary = xml_path(binary)?;
        let stdout = xml_path(stdout)?;
        let stderr = xml_path(stderr)?;
        let label = job.label();
        let arguments =
            job.arguments().iter().map(|arg| format!("<string>{arg}</string>")).collect::<String>();
        let environment = if environment.is_empty() {
            String::new()
        } else {
            let values = environment
                .iter()
                .map(|(name, value)| {
                    Ok(format!("<key>{name}</key><string>{}</string>", xml_text(value)?))
                })
                .collect::<Result<String>>()?;
            format!("    <key>EnvironmentVariables</key><dict>{values}</dict>\n")
        };
        Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array><string>{binary}</string>{arguments}</array>
{environment}    <key>StartInterval</key><integer>{interval}</integer>
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

    pub(super) fn run(command: ScheduleCommands, job: Job) -> Result<()> {
        let agent = Agent::new(job)?;
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
        fn job_environment_only_preserves_variables_allowed_for_each_job() {
            let home = Path::new("/Users/example");
            for (credentials, config) in [
                (None, None),
                (Some("~/.aws/custom"), None),
                (None, Some("/profiles/config")),
                (Some("~/.aws/custom"), Some("/profiles/config")),
            ] {
                let variables = BTreeMap::from([
                    ("AWS_SHARED_CREDENTIALS_FILE", credentials),
                    ("AWS_CONFIG_FILE", config),
                    ("PATH", Some("/custom/bin")),
                    ("AWS_ACCESS_KEY_ID", Some("ignored")),
                    ("AWS_SECRET_ACCESS_KEY", Some("ignored")),
                ]);
                let variable =
                    |name: &str| variables.get(name).copied().flatten().map(OsString::from);
                let local = job_environment_with(Job::LocalSync, home, variable).unwrap();
                assert_eq!(local, BTreeMap::from([("HOME", "/Users/example".to_string())]));
                let remote = job_environment_with(Job::RemoteSync, home, variable).unwrap();
                let mut expected = BTreeMap::from([
                    ("HOME", "/Users/example".to_string()),
                    ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin".to_string()),
                ]);
                if credentials.is_some() {
                    expected.insert(
                        "AWS_SHARED_CREDENTIALS_FILE",
                        "/Users/example/.aws/custom".to_string(),
                    );
                }
                if config.is_some() {
                    expected.insert("AWS_CONFIG_FILE", "/profiles/config".to_string());
                }
                assert_eq!(remote, expected);
            }
        }

        #[test]
        fn aws_file_overrides_keep_the_same_location_without_a_shell() {
            let home = Path::new("/Users/example");
            assert_eq!(
                aws_file_path(Path::new("~/.aws/custom"), home).unwrap(),
                "/Users/example/.aws/custom"
            );
            assert_eq!(aws_file_path(Path::new("/profiles/r2"), home).unwrap(), "/profiles/r2");
            assert_eq!(aws_file_path(Path::new(""), home).unwrap(), "");
            assert_eq!(
                aws_file_path(Path::new("profiles/r2"), home).unwrap(),
                std::env::current_dir().unwrap().join("profiles/r2").to_str().unwrap()
            );
        }

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
            let environment =
                job_environment_with(Job::LocalSync, Path::new("/Users/example"), |_| None)
                    .unwrap();
            let plist =
                render_plist(Job::LocalSync, binary, 1800, stdout, stderr, &environment).unwrap();
            let file = tempfile::NamedTempFile::new().unwrap();
            fs::write(file.path(), plist).unwrap();
            let output = checked(
                Command::new("/usr/bin/plutil")
                    .args(["-convert", "json", "-o", "-"])
                    .arg(file.path()),
            )
            .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["Label"], "io.github.samzong.recall.sync");
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
            assert_eq!(
                value["EnvironmentVariables"],
                serde_json::json!({"HOME": "/Users/example"})
            );
            assert!(
                render_plist(
                    Job::LocalSync,
                    Path::new("recall"),
                    1800,
                    stdout,
                    stderr,
                    &environment
                )
                .is_err()
            );
            assert!(
                render_plist(
                    Job::LocalSync,
                    Path::new("/bad\u{1}/recall"),
                    1800,
                    stdout,
                    stderr,
                    &environment
                )
                .is_err()
            );
        }

        #[test]
        fn schedules_keep_separate_identity_paths_arguments_and_environment() {
            let home = Path::new("/Users/example");
            let local = Agent::for_home(Job::LocalSync, home, "gui/501".to_string());
            let remote = Agent::for_home(Job::RemoteSync, home, "gui/501".to_string());
            assert_ne!(local.target(), remote.target());
            assert_ne!(local.plist, remote.plist);
            assert_ne!(local.stdout, remote.stdout);
            assert_ne!(local.stderr, remote.stderr);
            assert_eq!(
                local.plist,
                home.join("Library/LaunchAgents/io.github.samzong.recall.sync.plist")
            );
            assert_eq!(local.stdout, home.join("Library/Logs/recall/sync.stdout.log"));
            assert_eq!(local.stderr, home.join("Library/Logs/recall/sync.stderr.log"));
            let environment = job_environment_with(Job::RemoteSync, home, |name| match name {
                "AWS_SHARED_CREDENTIALS_FILE" => Some("/profiles/R2 & <test>\r.ini".into()),
                _ => None,
            })
            .unwrap();
            let plist = render_plist(
                Job::RemoteSync,
                Path::new("/opt/homebrew/bin/recall"),
                120,
                &remote.stdout,
                &remote.stderr,
                &environment,
            )
            .unwrap();
            let file = tempfile::NamedTempFile::new().unwrap();
            fs::write(file.path(), plist).unwrap();
            let output = checked(
                Command::new("/usr/bin/plutil")
                    .args(["-convert", "json", "-o", "-"])
                    .arg(file.path()),
            )
            .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["Label"], Job::RemoteSync.label());
            assert_eq!(
                value["ProgramArguments"],
                serde_json::json!(["/opt/homebrew/bin/recall", "remote", "sync"])
            );
            assert_eq!(value["StartInterval"], 120);
            assert_eq!(value["EnvironmentVariables"], serde_json::to_value(environment).unwrap());
            assert_eq!(value["StandardOutPath"], remote.stdout.to_str().unwrap());
            assert_eq!(value["StandardErrorPath"], remote.stderr.to_str().unwrap());
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
