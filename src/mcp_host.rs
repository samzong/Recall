use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const SERVER_NAME: &str = "recall";
const DEFAULT_BIN: &str = "recall";
const SERVER_ARG: &str = "mcp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Host {
    Claude,
    Codex,
}

impl Host {
    const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn binary(self) -> &'static str {
        self.id()
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => bail!("unknown MCP host '{other}'; supported hosts: claude, codex"),
        }
    }
}

enum HostAction {
    Install { bin: String },
    Uninstall,
}

pub(crate) fn install(agents: &[String], dry_run: bool, bin: Option<PathBuf>) -> Result<()> {
    let hosts = resolve_hosts(agents)?;
    let bin = resolve_bin(bin)?;
    run_hosts(hosts, dry_run, HostAction::Install { bin })
}

pub(crate) fn uninstall(agents: &[String], dry_run: bool) -> Result<()> {
    run_hosts(resolve_hosts(agents)?, dry_run, HostAction::Uninstall)
}

fn resolve_hosts(agents: &[String]) -> Result<Vec<Host>> {
    if agents.is_empty() || agents.iter().any(|agent| agent.trim() == "*") {
        return Ok(Host::ALL.to_vec());
    }

    let mut hosts = Vec::new();
    for agent in agents {
        let host = Host::parse(agent)?;
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    Ok(hosts)
}

fn resolve_bin(bin: Option<PathBuf>) -> Result<String> {
    let Some(path) = bin else {
        return Ok(DEFAULT_BIN.to_string());
    };
    if path.as_os_str().is_empty() {
        bail!("--bin must not be empty");
    }
    if path.is_absolute() {
        ensure_bin_file(&path)?;
        return Ok(path.to_string_lossy().into_owned());
    }
    if path.components().count() == 1 {
        return Ok(path.to_string_lossy().into_owned());
    }

    let absolute = std::env::current_dir()
        .context("failed to resolve current directory for --bin")?
        .join(path);
    ensure_bin_file(&absolute)?;
    Ok(absolute.to_string_lossy().into_owned())
}

fn ensure_bin_file(path: &Path) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    bail!("--bin {} is not a file", path.display());
}

fn add_args(host: Host, bin: &str) -> Vec<String> {
    match host {
        Host::Claude => vec![
            "mcp".into(),
            "add".into(),
            "--scope".into(),
            "user".into(),
            SERVER_NAME.into(),
            "--".into(),
            bin.into(),
            SERVER_ARG.into(),
        ],
        Host::Codex => {
            vec![
                "mcp".into(),
                "add".into(),
                SERVER_NAME.into(),
                "--".into(),
                bin.into(),
                SERVER_ARG.into(),
            ]
        }
    }
}

fn remove_args(host: Host) -> Vec<String> {
    match host {
        Host::Claude => {
            vec!["mcp".into(), "remove".into(), SERVER_NAME.into(), "-s".into(), "user".into()]
        }
        Host::Codex => vec!["mcp".into(), "remove".into(), SERVER_NAME.into()],
    }
}

fn run_hosts(hosts: Vec<Host>, dry_run: bool, action: HostAction) -> Result<()> {
    let mut changed = 0usize;
    let mut errors = Vec::new();

    for host in hosts {
        if !binary_on_path(host.binary()) {
            eprintln!("skipped {}: `{}` is not on PATH", host.id(), host.binary());
            continue;
        }
        match apply_host(host, dry_run, &action) {
            Ok(()) => changed += 1,
            Err(error) => errors.push(error),
        }
    }

    if changed == 0 && errors.is_empty() {
        bail!("no supported MCP hosts found on PATH (claude, codex)");
    }
    if !errors.is_empty() {
        let detail = errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
        if changed == 0 {
            bail!("failed to update Recall MCP: {detail}");
        }
        bail!("updated some hosts, but failed: {detail}");
    }
    Ok(())
}

fn apply_host(host: Host, dry_run: bool, action: &HostAction) -> Result<()> {
    match action {
        HostAction::Install { bin } => {
            let args = add_args(host, bin);
            if dry_run {
                println!("{}", display_command(host.binary(), &args));
                return Ok(());
            }
            install_host(host, bin)
        }
        HostAction::Uninstall => {
            let args = remove_args(host);
            if dry_run {
                println!("{}", display_command(host.binary(), &args));
                return Ok(());
            }
            uninstall_host(host)
        }
    }
}

fn install_host(host: Host, bin: &str) -> Result<()> {
    let args = add_args(host, bin);
    let add = run_host(host.binary(), &args)?;
    if add.success {
        println!("installed {}", host.id());
        return Ok(());
    }
    if !looks_like_already_exists(&add.combined()) {
        bail!("{}: {}", display_command(host.binary(), &args), add.summary());
    }

    remove_host(host)?;
    let retry = run_host(host.binary(), &args)?;
    if retry.success {
        println!("installed {}", host.id());
        return Ok(());
    }
    bail!("{}: {}", display_command(host.binary(), &args), retry.summary())
}

fn uninstall_host(host: Host) -> Result<()> {
    remove_host(host)?;
    println!("uninstalled {}", host.id());
    Ok(())
}

fn remove_host(host: Host) -> Result<()> {
    let args = remove_args(host);
    let output = run_host(host.binary(), &args)?;
    if output.success || looks_like_not_found(&output.combined()) {
        return Ok(());
    }
    bail!("{}: {}", display_command(host.binary(), &args), output.summary())
}

struct HostOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

impl HostOutput {
    fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }

    fn summary(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_string();
        }
        let stdout = self.stdout.trim();
        if stdout.is_empty() { "host command failed".to_string() } else { stdout.to_string() }
    }
}

fn run_host(program: &str, args: &[String]) -> Result<HostOutput> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    let result = HostOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
        if !result.stdout.ends_with('\n') {
            println!();
        }
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
        if !result.stderr.ends_with('\n') {
            eprintln!();
        }
    }
    Ok(result)
}

fn looks_like_already_exists(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("duplicate")
        || (lower.contains("already")
            && (lower.contains("exist") || lower.contains("registered") || lower.contains("added")))
}

fn looks_like_not_found(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("not found")
        || lower.contains("does not exist")
        || lower.contains("not registered")
        || lower.contains("not configured")
        || lower.contains("no mcp server")
}

fn display_command(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(quote_arg)
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_arg(arg: &str) -> String {
    if arg.bytes().any(|b| b.is_ascii_whitespace() || b == b'\'') {
        format!("'{}'", arg.replace('\'', "'\\''"))
    } else {
        arg.to_string()
    }
}

fn binary_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| host_binary_in(&dir, name))
}

#[cfg(unix)]
fn host_binary_in(dir: &Path, name: &str) -> bool {
    is_unix_executable(&dir.join(name))
}

#[cfg(windows)]
fn host_binary_in(dir: &Path, name: &str) -> bool {
    dir.join(name).is_file() || dir.join(format!("{name}.exe")).is_file()
}

#[cfg(not(any(unix, windows)))]
fn host_binary_in(dir: &Path, name: &str) -> bool {
    dir.join(name).is_file()
}

#[cfg(unix)]
fn is_unix_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path.metadata().ok().is_some_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::{
        Host, add_args, display_command, looks_like_already_exists, looks_like_not_found,
        quote_arg, remove_args, resolve_bin, resolve_hosts,
    };
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn resolve_hosts_defaults_to_all() {
        assert_eq!(resolve_hosts(&[]).unwrap(), Host::ALL.to_vec());
        assert_eq!(resolve_hosts(&["*".into()]).unwrap(), Host::ALL.to_vec());
    }

    #[test]
    fn resolve_hosts_accepts_aliases_and_dedups() {
        assert_eq!(
            resolve_hosts(&["claude-code".into(), "CLAUDE".into(), "codex".into()]).unwrap(),
            vec![Host::Claude, Host::Codex]
        );
    }

    #[test]
    fn resolve_hosts_rejects_unknown() {
        let error = resolve_hosts(&["cursor".into()]).unwrap_err().to_string();
        assert!(error.contains("unknown MCP host 'cursor'"));
    }

    #[test]
    fn add_args_match_host_clis() {
        assert_eq!(
            add_args(Host::Claude, "recall"),
            ["mcp", "add", "--scope", "user", "recall", "--", "recall", "mcp"]
        );
        assert_eq!(
            add_args(Host::Codex, "recall"),
            ["mcp", "add", "recall", "--", "recall", "mcp"]
        );
        assert_eq!(
            add_args(Host::Claude, "/tmp/recall"),
            ["mcp", "add", "--scope", "user", "recall", "--", "/tmp/recall", "mcp"]
        );
    }

    #[test]
    fn remove_args_match_host_clis() {
        assert_eq!(remove_args(Host::Claude), ["mcp", "remove", "recall", "-s", "user"]);
        assert_eq!(remove_args(Host::Codex), ["mcp", "remove", "recall"]);
    }

    #[test]
    fn resolve_bin_defaults_to_path_name() {
        assert_eq!(resolve_bin(None).unwrap(), "recall");
        assert_eq!(resolve_bin(Some(PathBuf::from("recall"))).unwrap(), "recall");
    }

    #[test]
    fn resolve_bin_rejects_missing_absolute_file() {
        let error = resolve_bin(Some(PathBuf::from("/definitely-missing-recall-bin"))).unwrap_err();
        assert!(error.to_string().contains("is not a file"));
    }

    #[test]
    fn resolve_bin_keeps_existing_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("recall");
        fs::write(&bin, "").unwrap();
        assert_eq!(resolve_bin(Some(bin.clone())).unwrap(), bin.to_string_lossy());
    }

    #[test]
    fn display_command_quotes_whitespace() {
        assert_eq!(
            display_command("claude", &add_args(Host::Claude, "/tmp/My Recall/recall")),
            "claude mcp add --scope user recall -- '/tmp/My Recall/recall' mcp"
        );
        assert_eq!(quote_arg("plain"), "plain");
    }

    #[test]
    fn host_error_text_classifies_upsert_and_missing() {
        assert!(looks_like_already_exists("MCP server recall already exists"));
        assert!(looks_like_already_exists("duplicate server name"));
        assert!(!looks_like_already_exists("permission denied"));
        assert!(looks_like_not_found("MCP server recall not found"));
        assert!(!looks_like_not_found("connection refused"));
    }
}
