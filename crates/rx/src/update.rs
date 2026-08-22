use std::cmp::Ordering;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Paths;
use crate::launch::EnvLookup;

const GITHUB_REPO: &str = "samzong/Recall";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_installed: Option<String>,
}

pub(crate) fn run(yes: bool, paths: &Paths) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let release = fetch_latest_release()?;
    let state = load_state(paths)?;
    if !update_pending(current, &release, &state) {
        println!("rx {current} is up to date (latest: {})", release.version);
        return Ok(());
    }
    if !yes && !confirm(&format!("Update rx {current} → {}?", release.version))? {
        println!("update cancelled");
        return Ok(());
    }
    eprintln!("Updating rx to {}...", release.version);
    install_release(&release)?;
    record_installed(paths, &release.version)?;
    println!("Updated rx to {}", release.version);
    Ok(())
}

/// rx shares the core release stream but its own crate version does not advance,
/// so the installed release tag — not `CARGO_PKG_VERSION` — decides whether the
/// fetched release is already installed.
pub(crate) fn update_pending(current: &str, release: &ReleaseInfo, state: &UpdateState) -> bool {
    if state.last_installed.as_deref() == Some(release.version.as_str()) {
        return false;
    }
    version_cmp(current, &release.version) == Ordering::Less
}

fn record_installed(paths: &Paths, version: &str) -> Result<()> {
    let mut state = load_state(paths)?;
    state.last_installed = Some(version.to_string());
    save_state(paths, &state)
}

#[cfg(test)]
pub(crate) fn state_with_installed(version: Option<&str>) -> UpdateState {
    UpdateState {
        auto_update: false,
        last_check: None,
        last_installed: version.map(str::to_string),
    }
}

pub(crate) fn maybe_before_launch(
    paths: &Paths,
    env: &EnvLookup,
    raw_args: &[String],
) -> Result<()> {
    if env.get("RX_NO_UPDATE").is_some_and(|value| !value.is_empty() && value != "0") {
        return Ok(());
    }
    let mut state = load_state(paths)?;
    if !should_check(&state) {
        return Ok(());
    }
    let current = env!("CARGO_PKG_VERSION");
    let release = match fetch_latest_release() {
        Ok(release) => release,
        Err(error) => {
            eprintln!("[rx] update check failed: {error:#}");
            return Ok(());
        }
    };
    state.last_check = Some(now_unix_seconds());
    save_state(paths, &state)?;
    if !update_pending(current, &release, &state) {
        return Ok(());
    }
    if state.auto_update {
        eprintln!("Updating rx to {}...", release.version);
        install_release(&release)?;
        record_installed(paths, &release.version)?;
        relaunch(raw_args)?;
    } else if !io::stderr().is_terminal() || !io::stdin().is_terminal() {
        eprintln!("rx {} is available — run `rx update`", release.version);
    } else {
        match prompt(&release.version)? {
            PromptChoice::Always => {
                state.auto_update = true;
                save_state(paths, &state)?;
                eprintln!("Updating rx to {}...", release.version);
                install_release(&release)?;
                record_installed(paths, &release.version)?;
                relaunch(raw_args)?;
            }
            PromptChoice::UpdateNow => {
                eprintln!("Updating rx to {}...", release.version);
                install_release(&release)?;
                record_installed(paths, &release.version)?;
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

fn relaunch(raw_args: &[String]) -> Result<()> {
    let exe = std::env::current_exe().context("cannot resolve rx executable path")?;
    let mut command = Command::new(&exe);
    // current_exe() resolves aliases (rxc/rxx/rxo/rxp) to the plain rx binary,
    // so the relaunched process would lose the argv0-selected harness.
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
    let mut request =
        agent.get(url).header("User-Agent", format!("rx/{}", env!("CARGO_PKG_VERSION")));
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

fn load_state(paths: &Paths) -> Result<UpdateState> {
    let path = state_path(paths);
    if !path.is_file() {
        return Ok(UpdateState::default());
    }
    let body = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

fn save_state(paths: &Paths, state: &UpdateState) -> Result<()> {
    fs::create_dir_all(&paths.dir).with_context(|| format!("create {}", paths.dir.display()))?;
    let body = toml::to_string_pretty(state).context("serialize rx-update.toml")?;
    let path = state_path(paths);
    fs::write(&path, body).with_context(|| format!("write {}", path.display()))
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
