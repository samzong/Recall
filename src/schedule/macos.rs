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
        let plist =
            render_plist(self.job, &binary, interval, &self.stdout, &self.stderr, &environment)?;
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
        anyhow::ensure!(self.loaded_job()?.is_some(), "LaunchAgent was not loaded after bootstrap");
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
            let binary =
                definition.program_arguments.first().context("LaunchAgent has no binary path")?;
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
        Ok(metadata) => chrono::DateTime::<chrono::Utc>::from(metadata.modified()?).to_rfc3339(),
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
        text.chars()
            .all(|c| matches!(c, '\t' | '\n' | '\r')
                || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}')),
        "LaunchAgent value contains a character unsupported by XML"
    );
    Ok(text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\r', "&#13;"))
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
    for name in ["AWS_PROFILE", "AWS_DEFAULT_PROFILE"] {
        if let Some(value) = variable(name) {
            environment.insert(
                name,
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("AWS profile must be valid UTF-8"))?,
            );
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
    let values = environment
        .iter()
        .map(|(name, value)| Ok(format!("<key>{name}</key><string>{}</string>", xml_text(value)?)))
        .collect::<Result<String>>()?;
    let environment = format!("    <key>EnvironmentVariables</key><dict>{values}</dict>\n");
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
mod tests;
