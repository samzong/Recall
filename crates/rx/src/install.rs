use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::process::{Command, Stdio};

#[cfg(unix)]
use anyhow::Context;

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
    }
}

pub(crate) fn command_line(spec: &InstallSpec) -> String {
    format!("curl -fsSL {} | {}", spec.url, spec.shell)
}

pub(crate) fn ensure(harness: Harness, env: &EnvLookup) -> Result<PathBuf> {
    if !env.is_real() {
        return Ok(PathBuf::from(harness.as_str()));
    }
    let spec = spec(harness);
    if let Some(path) = lookup(spec.program) {
        return Ok(path);
    }
    let cmd = command_line(&spec);
    if env.get("RX_NO_INSTALL").is_some_and(|value| !value.is_empty() && value != "0") {
        bail!("{} is not installed (RX_NO_INSTALL=1). Install with:\n  {cmd}", spec.display);
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("{} is not installed. Install with:\n  {cmd}", spec.display);
    }
    if !confirm(&format!("{} is not installed. Install now?", spec.display))? {
        bail!("install cancelled. Install with:\n  {cmd}");
    }
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
    vec![home.join(".local/bin"), home.join(".opencode/bin"), home.join(".claude/local/bin")]
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
