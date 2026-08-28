use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::args::UpdateCommand;
use crate::config::Paths;
use crate::launch::EnvLookup;

const GITHUB_REPO: &str = "samzong/Recall";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const HOMEBREW_UPDATE_HINT: &str =
    "rx is managed by Homebrew; run `brew upgrade recall`";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseInfo {
    pub version: String,
    pub asset_name: String,
    pub download_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct UpdateState {
    #[serde(default)]
    auto_update: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_check: Option<String>,
}

pub(crate) fn help() -> &'static str {
    concat!(
        "usage: rx update [--yes]\n\n",
        "Download and install the latest rx from GitHub releases.\n",
    )
}

pub(crate) fn run(command: UpdateCommand) -> Result<()> {
    match command {
        UpdateCommand::Help => {
            print!("{}", help());
            Ok(())
        }
        UpdateCommand::Run { yes } => install(yes),
    }
}

fn install(yes: bool) -> Result<()> {
    let current = crate::RELEASE_VERSION;
    let release = fetch_latest_release()?;
    if !update_pending(current, &release) {
        println!("rx {current} is up to date (latest: {})", release.version);
        return Ok(());
    }
    ensure_current_executable_self_update_allowed()?;
    if !yes && !confirm(&format!("Update rx {current} → {}?", release.version))? {
        println!("update cancelled");
        return Ok(());
    }
    eprintln!("Updating rx to {}...", release.version);
    install_release(&release)?;
    println!("Updated rx to {}", release.version);
    Ok(())
}

pub(crate) fn update_pending(current: &str, release: &ReleaseInfo) -> bool {
    version_cmp(current, &release.version) == Ordering::Less
}

pub(crate) fn maybe_before_launch(
    paths: &Paths,
    env: &EnvLookup,
    raw_args: &[std::ffi::OsString],
) -> Result<()> {
    if env.get("RX_NO_UPDATE").is_some_and(|value| !value.is_empty() && value != "0") {
        return Ok(());
    }
    {
        let _lock = lock_state(paths)?;
        if !should_check(&load_state(paths)?) {
            return Ok(());
        }
    }
    let current = crate::RELEASE_VERSION;
    let release = match fetch_latest_release() {
        Ok(release) => release,
        Err(error) => {
            eprintln!("[rx] update check failed: {error:#}");
            stamp_last_check(paths)?;
            return Ok(());
        }
    };
    let state = stamp_last_check(paths)?;
    if !update_pending(current, &release) {
        return Ok(());
    }
    let current_exe = std::env::current_exe().context("cannot resolve rx executable path")?;
    if let Some(notice) = homebrew_launch_update_notice(&current_exe, &release.version) {
        eprintln!("{notice}");
        return Ok(());
    }
    if state.auto_update {
        eprintln!("Updating rx to {}...", release.version);
        install_release(&release)?;
        relaunch(raw_args)?;
    } else if !io::stderr().is_terminal() || !io::stdin().is_terminal() {
        eprintln!("rx {} is available — run `rx update`", release.version);
    } else {
        match prompt(&release.version)? {
            PromptChoice::Always => {
                enable_auto_update(paths)?;
                eprintln!("Updating rx to {}...", release.version);
                install_release(&release)?;
                relaunch(raw_args)?;
            }
            PromptChoice::UpdateNow => {
                eprintln!("Updating rx to {}...", release.version);
                install_release(&release)?;
                relaunch(raw_args)?;
            }
            PromptChoice::NotNow => {}
        }
    }
    Ok(())
}

enum PromptChoice {
    Always,
    UpdateNow,
    NotNow,
}

fn prompt(latest: &str) -> Result<PromptChoice> {
    eprintln!("rx {latest} is available. Update before launching?");
    eprintln!("  1) Always auto-update on launch");
    eprintln!("  2) Update now");
    eprintln!("  3) Not now");
    eprint!("> ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    match line.trim() {
        "1" | "always" => Ok(PromptChoice::Always),
        "2" | "update" | "y" | "yes" => Ok(PromptChoice::UpdateNow),
        _ => Ok(PromptChoice::NotNow),
    }
}

fn confirm(message: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("refusing to update without --yes in non-interactive mode");
    }
    eprint!("{message} [y/N] ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn relaunch(raw_args: &[std::ffi::OsString]) -> Result<()> {
    let exe = std::env::current_exe().context("cannot resolve rx executable path")?;
    let mut command = Command::new(&exe);
    if let Some(harness) = raw_args.first().and_then(|argv0| crate::args::argv0_harness(argv0)) {
        command.arg(harness);
    }
    command.args(raw_args.iter().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("failed to relaunch rx after update")
    }
    #[cfg(not(unix))]
    {
        let status = command.status().context("failed to relaunch rx after update")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

pub(crate) fn fetch_latest_release() -> Result<ReleaseInfo> {
    let asset_name = release_asset_name()?;
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let body = http_get(&url, &[("Accept", "application/vnd.github+json")])?;
    let value: serde_json::Value = serde_json::from_str(&body).context("GitHub release JSON")?;
    let tag = value.get("tag_name").and_then(|v| v.as_str()).context("release missing tag_name")?;
    let version = tag.trim_start_matches('v').to_string();
    let assets =
        value.get("assets").and_then(|v| v.as_array()).context("release missing assets")?;
    let download_url = assets
        .iter()
        .find(|asset| asset.get("name").and_then(|v| v.as_str()) == Some(asset_name))
        .and_then(|asset| asset.get("browser_download_url").and_then(|v| v.as_str()))
        .with_context(|| format!("release asset not found: {asset_name}"))?
        .to_string();
    Ok(ReleaseInfo { version, asset_name: asset_name.to_string(), download_url })
}

fn install_release(release: &ReleaseInfo) -> Result<()> {
    let temp = tempfile::tempdir().context("create temp dir for update")?;
    let archive_path = temp.path().join(&release.asset_name);
    let bytes = http_get_bytes(&release.download_url, &[])?;
    fs::write(&archive_path, bytes).context("write release download")?;
    let extracted = extract_rx_binary(&archive_path, temp.path())?;
    replace_executable(&extracted)
}

fn extract_rx_binary(archive_path: &Path, dest: &Path) -> Result<PathBuf> {
    let file_name = archive_path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    if file_name.ends_with(".tar.gz") {
        extract_tar_gz(archive_path, dest)?;
        Ok(dest.join("rx"))
    } else if file_name.ends_with(".zip") {
        extract_zip(archive_path, dest)?;
        Ok(dest.join("rx.exe"))
    } else {
        bail!("unsupported release archive: {}", archive_path.display())
    }
}

fn extract_tar_gz(archive_path: &Path, dest: &Path) -> Result<()> {
    let file =
        fs::File::open(archive_path).with_context(|| format!("open {}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest).context("unpack release tarball")?;
    Ok(())
}

#[cfg(windows)]
fn extract_zip(archive_path: &Path, dest: &Path) -> Result<()> {
    let file =
        fs::File::open(archive_path).with_context(|| format!("open {}", archive_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("read release zip")?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("read zip entry")?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let out_path = dest.join(name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut outfile = fs::File::create(&out_path)?;
            io::copy(&mut entry, &mut outfile)?;
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn extract_zip(_archive_path: &Path, _dest: &Path) -> Result<()> {
    bail!("zip release archives are only supported on Windows")
}

fn replace_executable(source: &Path) -> Result<()> {
    if !source.is_file() {
        bail!("release archive did not contain rx at {}", source.display());
    }
    let target = std::env::current_exe().context("cannot resolve rx executable path")?;
    ensure_self_update_allowed(&target)?;
    #[cfg(windows)]
    if target.is_file() {
        // Windows locks running executables: move the old binary aside so the
        // staged update can take its place. A leftover backup from a previous
        // update must go first — Windows rename does not replace destinations.
        let backup = target.with_extension("old.exe");
        if backup.exists() {
            let _ = fs::remove_file(&backup);
        }
        fs::rename(&target, &backup)
            .with_context(|| format!("move running executable to {}", backup.display()))?;
    }
    let staging = target.with_extension("update-staging");
    fs::copy(source, &staging).with_context(|| format!("stage update at {}", staging.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&staging, &target)
        .with_context(|| format!("install update to {}", target.display()))?;
    Ok(())
}

fn ensure_current_executable_self_update_allowed() -> Result<()> {
    let target = std::env::current_exe().context("cannot resolve rx executable path")?;
    ensure_self_update_allowed(&target)
}

fn ensure_self_update_allowed(path: &Path) -> Result<()> {
    if let Some(hint) = self_update_blocker(path) {
        bail!(hint);
    }
    Ok(())
}

fn self_update_blocker(path: &Path) -> Option<&'static str> {
    homebrew_update_hint(path).or_else(|| {
        fs::canonicalize(path).ok().and_then(|resolved| homebrew_update_hint(&resolved))
    })
}

fn homebrew_launch_update_notice(path: &Path, latest: &str) -> Option<String> {
    self_update_blocker(path)
        .map(|_| format!("rx {latest} is available — run `brew upgrade recall`"))
}

fn homebrew_update_hint(path: &Path) -> Option<&'static str> {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    components
        .windows(2)
        .any(|pair| pair[0] == OsStr::new("Cellar") && pair[1] == OsStr::new("recall"))
        .then_some(HOMEBREW_UPDATE_HINT)
}

#[cfg(test)]
pub(crate) fn self_update_blocker_for_test(path: &Path) -> Option<&'static str> {
    self_update_blocker(path)
}

#[cfg(test)]
pub(crate) fn homebrew_launch_update_notice_for_test(path: &Path, latest: &str) -> Option<String> {
    homebrew_launch_update_notice(path, latest)
}

fn http_get(url: &str, headers: &[(&str, &str)]) -> Result<String> {
    let bytes = http_get_bytes(url, headers)?;
    String::from_utf8(bytes).context("response is not UTF-8")
}

fn http_get_bytes(url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent.get(url).header("User-Agent", format!("rx/{}", crate::RELEASE_VERSION));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut response = request.call().with_context(|| format!("GET {url}"))?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_vec().context("read response body")?;
    if !(200..300).contains(&status) {
        bail!("HTTP {status}: {}", String::from_utf8_lossy(&body[..body.len().min(300)]));
    }
    Ok(body)
}

pub(crate) fn release_asset_name() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("recall-macos-aarch64.tar.gz"),
        ("macos", "x86_64") => Ok("recall-macos-x86_64.tar.gz"),
        ("linux", "x86_64") => Ok("recall-linux-x86_64.tar.gz"),
        ("windows", "x86_64") => Ok("recall-windows-x86_64.zip"),
        (os, arch) => bail!("unsupported platform for rx update: {os}-{arch}"),
    }
}

pub(crate) fn version_cmp(left: &str, right: &str) -> Ordering {
    let parse = |value: &str| {
        value
            .trim_start_matches('v')
            .split(['.', '-'])
            .map(|part| {
                part.chars()
                    .take_while(|ch| ch.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u64>()
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>()
    };
    let left_parts = parse(left);
    let right_parts = parse(right);
    let len = left_parts.len().max(right_parts.len());
    for index in 0..len {
        match left_parts
            .get(index)
            .copied()
            .unwrap_or(0)
            .cmp(&right_parts.get(index).copied().unwrap_or(0))
        {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

fn state_path(paths: &Paths) -> PathBuf {
    paths.dir.join("rx-update.toml")
}

fn lock_state(paths: &Paths) -> Result<fs::File> {
    exclusive_sidecar(&state_path(paths))
}

fn stamp_last_check(paths: &Paths) -> Result<UpdateState> {
    let _lock = lock_state(paths)?;
    let mut state = load_state(paths)?;
    state.last_check = Some(now_unix_seconds());
    save_state(paths, &state)?;
    Ok(state)
}

fn enable_auto_update(paths: &Paths) -> Result<()> {
    let _lock = lock_state(paths)?;
    let mut state = load_state(paths)?;
    state.auto_update = true;
    save_state(paths, &state)
}

fn exclusive_sidecar(path: &Path) -> Result<fs::File> {
    let parent = path.parent().context("state file has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".rx.lock");
    let lock_path = PathBuf::from(lock_path);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    lock.lock_exclusive().with_context(|| format!("failed to lock {}", lock_path.display()))?;
    Ok(lock)
}

fn load_state(paths: &Paths) -> Result<UpdateState> {
    let path = state_path(paths);
    if !path.is_file() {
        return Ok(UpdateState::default());
    }
    let body = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

fn save_state(paths: &Paths, state: &UpdateState) -> Result<()> {
    let path = state_path(paths);
    let parent = path.parent().context("state file has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let body = toml::to_string_pretty(state).context("serialize rx-update.toml")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary {}", path.display()))?;
    temp.write_all(body.as_bytes())
        .with_context(|| format!("write temporary {}", path.display()))?;
    temp.as_file().sync_all().with_context(|| format!("sync temporary {}", path.display()))?;
    temp.persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn should_check(state: &UpdateState) -> bool {
    let Some(last_check) = &state.last_check else {
        return true;
    };
    let Ok(parsed) = parse_unix_seconds(last_check) else {
        return true;
    };
    SystemTime::now().duration_since(parsed).is_ok_and(|elapsed| elapsed >= CHECK_INTERVAL)
}

fn now_unix_seconds() -> String {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{seconds}")
}

fn parse_unix_seconds(value: &str) -> Result<SystemTime> {
    let seconds: u64 = value.parse().context("parse last_check timestamp")?;
    Ok(UNIX_EPOCH + Duration::from_secs(seconds))
}
