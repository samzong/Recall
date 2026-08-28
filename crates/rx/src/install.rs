use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

#[cfg(unix)]
use std::fs;

use crate::args::Harness;
use crate::launch::EnvLookup;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InstallSpec {
    pub program: &'static str,
    pub display: &'static str,
    pub url: &'static str,
    pub shell: &'static str,
}

pub(crate) fn spec(harness: Harness) -> InstallSpec {
    match harness {
        Harness::Claude => InstallSpec {
            program: "claude",
            display: "Claude Code",
            url: "https://claude.ai/install.sh",
            shell: "bash",
        },
        Harness::Codex => InstallSpec {
            program: "codex",
            display: "Codex",
            url: "https://chatgpt.com/codex/install.sh",
            shell: "sh",
        },
        Harness::OpenCode => InstallSpec {
            program: "opencode",
            display: "OpenCode",
            url: "https://opencode.ai/install",
            shell: "bash",
        },
        Harness::Pi => InstallSpec {
            program: "pi",
            display: "Pi",
            url: "https://pi.dev/install.sh",
            shell: "sh",
        },
        Harness::Dsh => InstallSpec {
            program: "dsh",
            display: "DeepSeek Harness",
            url: "https://www.npmjs.com/package/@deepseek-ai/dsh",
            shell: "sh",
        },
        Harness::Kimi => InstallSpec {
            program: "kimi",
            display: "Kimi Code",
            url: "https://code.kimi.com/kimi-code/install.sh",
            shell: "bash",
        },
    }
}

pub(crate) fn command_line(spec: &InstallSpec) -> String {
    format!("curl -fsSL {} | {}", spec.url, spec.shell)
}

pub(crate) fn ensure(harness: Harness, env: &EnvLookup) -> Result<PathBuf> {
    if matches!(harness, Harness::Dsh) {
        return ensure_dsh(env);
    }
    if !env.is_real() {
        return Ok(PathBuf::from(harness.as_str()));
    }
    let spec = spec(harness);
    if let Some(path) = lookup(spec.program) {
        return Ok(path);
    }
    let cmd = command_line(&spec);
    offer_install(spec.display, &cmd, env)?;
    eprintln!("[rx] {cmd}");
    run_official_installer(&spec)?;
    lookup(spec.program).ok_or_else(|| {
        anyhow::anyhow!(
            "{} finished installing but {} was not found. Add ~/.local/bin and ~/.opencode/bin to PATH, then retry.",
            spec.display,
            spec.program
        )
    })
}

pub(crate) fn lookup(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let extensions = executable_extensions();
    lookup_with(program, &path, &extra_bin_dirs(), extensions.as_deref())
}

pub(crate) fn lookup_with(
    program: &str,
    path: impl AsRef<std::ffi::OsStr>,
    extra: &[PathBuf],
    extensions: Option<&OsStr>,
) -> Option<PathBuf> {
    let names = executable_names(program, extensions);
    for dir in std::env::split_paths(path.as_ref()).chain(extra.iter().cloned()) {
        for name in &names {
            let candidate = dir.join(name);
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_names(program: &str, extensions: Option<&OsStr>) -> Vec<OsString> {
    let mut names = vec![OsString::from(program)];
    if Path::new(program).extension().is_some() {
        return names;
    }
    let Some(extensions) = extensions else {
        return names;
    };
    for extension in
        extensions.to_string_lossy().split(';').map(str::trim).filter(|value| !value.is_empty())
    {
        let extension = if extension.starts_with('.') {
            extension.to_string()
        } else {
            format!(".{extension}")
        };
        let candidate = OsString::from(format!("{program}{extension}"));
        if !names.contains(&candidate) {
            names.push(candidate);
        }
    }
    names
}

#[cfg(windows)]
fn executable_extensions() -> Option<OsString> {
    Some(std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD")))
}

#[cfg(not(windows))]
fn executable_extensions() -> Option<OsString> {
    None
}

pub(crate) fn extra_bin_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".local/bin"),
        home.join(".opencode/bin"),
        home.join(".claude/local/bin"),
        home.join(".kimi-code/bin"),
    ]
}

fn run_official_installer(spec: &InstallSpec) -> Result<()> {
    #[cfg(not(unix))]
    {
        bail!(
            "{} official installer is a Unix shell script. Install with:\n  {}",
            spec.display,
            command_line(spec)
        );
    }
    #[cfg(unix)]
    {
        let status = Command::new(spec.shell)
            .arg("-c")
            .arg(command_line(spec))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("failed to start {}", spec.shell))?;
        if !status.success() {
            bail!("{} installer exited with {status}", spec.display);
        }
        Ok(())
    }
}

fn ensure_dsh(env: &EnvLookup) -> Result<PathBuf> {
    if !env.is_real() {
        return Ok(PathBuf::from("dsh"));
    }
    if let Some(path) = lookup_program("dsh") {
        if crate::dsh::profile_ready(env) {
            return Ok(path);
        }
        offer_install("DeepSeek Harness TUI profile", &crate::dsh::profile_hint(), env)?;
        ensure_pnpm()?;
        install_dsh_profile(&path, env)?;
        return Ok(path);
    }
    offer_install("DeepSeek Harness", &crate::dsh::install_hint(), env)?;
    ensure_pnpm()?;
    install_dsh_packages()?;
    let path = lookup_program("dsh").ok_or_else(|| {
        anyhow::anyhow!(
            "DeepSeek Harness finished installing but dsh was not found. Add npm's global bin to PATH, then retry."
        )
    })?;
    if !crate::dsh::profile_ready(env) {
        install_dsh_profile(&path, env)?;
    }
    Ok(path)
}

fn install_dsh_packages() -> Result<()> {
    let cmd = crate::dsh::npm_install_cmd();
    eprintln!("[rx] {cmd}");
    run_npm(
        &[
            "install",
            "-g",
            "--legacy-peer-deps",
            crate::dsh::CLI_PACKAGE,
            crate::dsh::PLUGIN_PACKAGE,
        ],
        &cmd,
    )?;
    Ok(())
}

fn ensure_pnpm() -> Result<()> {
    if lookup_program("pnpm").is_some() {
        return Ok(());
    }
    let cmd = "npm install -g pnpm";
    eprintln!("[rx] {cmd}");
    run_npm(&["install", "-g", "pnpm"], cmd)
}

fn install_dsh_profile(dsh: &Path, env: &EnvLookup) -> Result<()> {
    let cmd = crate::dsh::profile_hint();
    eprintln!("[rx] {cmd}");
    run_command(
        dsh,
        &["plugin", "--profile", crate::dsh::PROFILE, "add", "-w", crate::dsh::PLUGIN_SPEC],
        "dsh plugin",
    )?;
    if crate::dsh::profile_ready(env) {
        return Ok(());
    }
    bail!("dsh plugin finished but the dsh-tui profile is still missing. Install with:\n  {cmd}")
}

fn offer_install(display: &str, hint: &str, env: &EnvLookup) -> Result<()> {
    if env.get("RX_NO_INSTALL").is_some_and(|value| !value.is_empty() && value != "0") {
        bail!("{display} is not installed (RX_NO_INSTALL=1). Install with:\n  {hint}");
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("{display} is not installed. Install with:\n  {hint}");
    }
    if !confirm(&format!("{display} is not installed. Install now?"))? {
        bail!("install cancelled. Install with:\n  {hint}");
    }
    Ok(())
}

fn lookup_program(program: &str) -> Option<PathBuf> {
    if let Some(path) = lookup(program) {
        return Some(path);
    }
    let dir = lookup("npm")?.parent()?.to_path_buf();
    lookup_with(program, "", &[dir], executable_extensions().as_deref())
}

fn run_npm(args: &[&str], label: &str) -> Result<()> {
    let npm = lookup_program("npm").context("npm is required to install DeepSeek Harness")?;
    run_command(&npm, args, label)
}

fn run_command(program: &Path, args: &[&str], label: &str) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to start {label}"))?;
    if !status.success() {
        bail!("{label} exited with {status}");
    }
    Ok(())
}

fn confirm(message: &str) -> Result<bool> {
    eprint!("{message} [y/N] ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn is_runnable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}
