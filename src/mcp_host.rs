use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::utils::binary_on_path;

const SERVER_NAME: &str = "recall";
const DEFAULT_BIN: &str = "recall";
const SERVER_ARG: &str = "mcp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Host {
    Claude,
    Codex,
    Cursor,
}

impl Host {
    const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Cursor];

    fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor-agent",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "cursor" | "cursor-agent" => Ok(Self::Cursor),
            other => {
                bail!("unknown MCP host '{other}'; supported hosts: claude, codex, cursor-agent")
            }
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

fn add_args(host: Host, bin: &str) -> Option<Vec<String>> {
    match host {
        Host::Claude => Some(vec![
            "mcp".into(),
            "add".into(),
            "--scope".into(),
            "user".into(),
            SERVER_NAME.into(),
            "--".into(),
            bin.into(),
            SERVER_ARG.into(),
        ]),
        Host::Codex => Some(vec![
            "mcp".into(),
            "add".into(),
            SERVER_NAME.into(),
            "--".into(),
            bin.into(),
            SERVER_ARG.into(),
        ]),
        Host::Cursor => None,
    }
}

fn remove_args(host: Host) -> Option<Vec<String>> {
    match host {
        Host::Claude => Some(vec![
            "mcp".into(),
            "remove".into(),
            SERVER_NAME.into(),
            "-s".into(),
            "user".into(),
        ]),
        Host::Codex => Some(vec!["mcp".into(), "remove".into(), SERVER_NAME.into()]),
        Host::Cursor => None,
    }
}

fn run_hosts(hosts: Vec<Host>, dry_run: bool, action: HostAction) -> Result<()> {
    let mut changed = 0usize;
    let mut errors = Vec::new();

    for host in hosts {
        if !binary_on_path(host.id()) {
            eprintln!("skipped {}: `{}` is not on PATH", host.id(), host.id());
            continue;
        }
        match apply_host(host, dry_run, &action) {
            Ok(()) => changed += 1,
            Err(error) => errors.push(error),
        }
    }

    if changed == 0 && errors.is_empty() {
        bail!("no supported MCP hosts found on PATH (claude, codex, cursor-agent)");
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
        HostAction::Install { bin } => match add_args(host, bin) {
            Some(args) => run_host_command(host, &args, dry_run, action, &mut run_host),
            None => {
                let path = cursor_config_path()?;
                if dry_run {
                    println!("write {} ({})", path.display(), SERVER_NAME);
                    return Ok(());
                }
                write_cursor_config(&path, bin)?;
                println!("installed {}", host.id());
                Ok(())
            }
        },
        HostAction::Uninstall => match remove_args(host) {
            Some(args) => run_host_command(host, &args, dry_run, action, &mut run_host),
            None => {
                let path = cursor_config_path()?;
                if dry_run {
                    println!("remove {} ({})", path.display(), SERVER_NAME);
                    return Ok(());
                }
                remove_cursor_config(&path)?;
                println!("uninstalled {}", host.id());
                Ok(())
            }
        },
    }
}

fn cursor_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".cursor").join("mcp.json"))
}

fn update_cursor_config(path: &Path, mutate: impl FnOnce(&mut Value) -> Result<()>) -> Result<()> {
    let mut config = read_cursor_config(path)?;
    mutate(&mut config)?;
    write_cursor_config_file(path, &config)
}

fn uses_non_stdio_transport(entry: &Map<String, Value>) -> bool {
    entry.contains_key("url") || entry.get("type").is_some_and(|transport| transport != "stdio")
}

fn write_cursor_config(path: &Path, bin: &str) -> Result<()> {
    update_cursor_config(path, |config| {
        let servers = config
            .as_object_mut()
            .and_then(|root| {
                root.entry("mcpServers")
                    .or_insert_with(|| Value::Object(Map::new()))
                    .as_object_mut()
            })
            .context("invalid ~/.cursor/mcp.json: mcpServers must be an object")?;
        let entry = servers
            .entry(SERVER_NAME)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .context("invalid ~/.cursor/mcp.json: mcpServers.recall must be an object")?;
        if uses_non_stdio_transport(entry) {
            bail!(
                "cannot install cursor-agent: ~/.cursor/mcp.json mcpServers.recall uses a non-stdio transport"
            );
        }
        entry.insert("type".to_string(), Value::String("stdio".to_string()));
        entry.insert("command".to_string(), Value::String(bin.to_string()));
        entry.insert("args".to_string(), serde_json::json!([SERVER_ARG]));
        Ok(())
    })
}

fn remove_cursor_config(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    update_cursor_config(path, |config| {
        if let Some(servers) = config.get_mut("mcpServers").and_then(Value::as_object_mut) {
            if servers
                .get(SERVER_NAME)
                .and_then(Value::as_object)
                .is_some_and(uses_non_stdio_transport)
            {
                bail!(
                    "cannot uninstall cursor-agent: ~/.cursor/mcp.json mcpServers.recall uses a non-stdio transport"
                );
            }
            servers.remove(SERVER_NAME);
        }
        Ok(())
    })
}

fn read_cursor_config(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(serde_json::json!({ "mcpServers": {} }))
        }
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn write_cursor_config_file(path: &Path, config: &Value) -> Result<()> {
    let resolved_path = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::canonicalize(path)
                .with_context(|| format!("failed to resolve symbolic link {}", path.display()))?;
            if !target.is_file() {
                bail!("symbolic link {} does not resolve to a file", path.display());
            }
            Some(target)
        }
        Ok(_) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    let path = resolved_path.as_deref().unwrap_or(path);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cursor config path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let body = format!("{}\n", serde_json::to_string_pretty(config)?);
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    temp.write_all(body.as_bytes())
        .with_context(|| format!("failed to write temporary config in {}", parent.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync temporary config in {}", parent.display()))?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn run_host_command(
    host: Host,
    args: &[String],
    dry_run: bool,
    action: &HostAction,
    run: &mut impl FnMut(&str, &[String]) -> Result<HostOutput>,
) -> Result<()> {
    if dry_run {
        println!("{}", display_command(host.id(), args));
        return Ok(());
    }
    let removing = matches!(action, HostAction::Uninstall);
    let output = run(host.id(), args)?;
    if output.success
        || (removing && looks_like_not_found(&format!("{}\n{}", output.stdout, output.stderr)))
    {
        println!("{} {}", if removing { "uninstalled" } else { "installed" }, host.id());
        return Ok(());
    }
    bail!("{}: {}", display_command(host.id(), args), output.summary())
}

struct HostOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

impl HostOutput {
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

#[cfg(test)]
mod tests {
    use super::{
        Host, HostAction, HostOutput, SERVER_ARG, SERVER_NAME, add_args, display_command,
        looks_like_not_found, quote_arg, read_cursor_config, remove_args, remove_cursor_config,
        resolve_bin, resolve_hosts, run_host_command, write_cursor_config,
        write_cursor_config_file,
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
        assert_eq!(
            resolve_hosts(&["cursor".into(), "cursor-agent".into()]).unwrap(),
            vec![Host::Cursor]
        );
    }

    #[test]
    fn resolve_hosts_rejects_unknown() {
        let error = resolve_hosts(&["pi".into()]).unwrap_err().to_string();
        assert!(error.contains("unknown MCP host 'pi'"));
    }

    #[test]
    fn add_args_match_host_clis() {
        assert_eq!(
            add_args(Host::Claude, "recall").unwrap(),
            ["mcp", "add", "--scope", "user", "recall", "--", "recall", "mcp"]
        );
        assert_eq!(
            add_args(Host::Codex, "recall").unwrap(),
            ["mcp", "add", "recall", "--", "recall", "mcp"]
        );
        assert!(add_args(Host::Cursor, "recall").is_none());
        assert_eq!(
            add_args(Host::Claude, "/tmp/recall").unwrap(),
            ["mcp", "add", "--scope", "user", "recall", "--", "/tmp/recall", "mcp"]
        );
    }

    #[test]
    fn remove_args_match_host_clis() {
        assert_eq!(remove_args(Host::Claude).unwrap(), ["mcp", "remove", "recall", "-s", "user"]);
        assert_eq!(remove_args(Host::Codex).unwrap(), ["mcp", "remove", "recall"]);
        assert!(remove_args(Host::Cursor).is_none());
    }

    #[test]
    fn cursor_config_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        assert_eq!(read_cursor_config(&path).unwrap()["mcpServers"], serde_json::json!({}));

        write_cursor_config_file(
            &path,
            &serde_json::json!({
                "mcpServers": {
                    SERVER_NAME: { "command": "/tmp/recall", "args": [SERVER_ARG] }
                }
            }),
        )
        .unwrap();
        let parsed = read_cursor_config(&path).unwrap();
        assert_eq!(parsed["mcpServers"][SERVER_NAME]["command"], "/tmp/recall");
        assert_eq!(parsed["mcpServers"][SERVER_NAME]["args"], serde_json::json!([SERVER_ARG]));
    }

    #[test]
    fn install_preserves_user_owned_server_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        write_cursor_config_file(
            &path,
            &serde_json::json!({
                "mcpServers": {
                    "other": { "command": "other-bin" },
                    SERVER_NAME: {
                        "command": "/opt/recall",
                        "args": [SERVER_ARG],
                        "env": { "RECALL_DB": "/data/recall.db" }
                    }
                }
            }),
        )
        .unwrap();

        write_cursor_config(&path, "/usr/local/bin/recall").unwrap();

        let parsed = read_cursor_config(&path).unwrap();
        assert_eq!(parsed["mcpServers"][SERVER_NAME]["command"], "/usr/local/bin/recall");
        assert_eq!(parsed["mcpServers"][SERVER_NAME]["type"], "stdio");
        assert_eq!(parsed["mcpServers"][SERVER_NAME]["env"]["RECALL_DB"], "/data/recall.db");
        assert_eq!(parsed["mcpServers"]["other"]["command"], "other-bin");
    }

    #[test]
    fn remote_server_is_never_mutated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        write_cursor_config_file(
            &path,
            &serde_json::json!({
                "mcpServers": {
                    SERVER_NAME: {
                        "type": "http",
                        "url": "https://example.com/mcp",
                        "headers": { "Authorization": "Bearer preserved" }
                    }
                }
            }),
        )
        .unwrap();
        let before = fs::read_to_string(&path).unwrap();

        assert!(write_cursor_config(&path, "/usr/local/bin/recall").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert!(remove_cursor_config(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn uninstall_removes_malformed_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        write_cursor_config_file(
            &path,
            &serde_json::json!({ "mcpServers": { SERVER_NAME: null } }),
        )
        .unwrap();

        remove_cursor_config(&path).unwrap();

        assert!(read_cursor_config(&path).unwrap()["mcpServers"].get(SERVER_NAME).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cursor_config_write_preserves_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("owned-mcp.json");
        let path = dir.path().join("mcp.json");
        write_cursor_config_file(&target, &serde_json::json!({ "owner": "user" })).unwrap();
        symlink(&target, &path).unwrap();

        write_cursor_config_file(&path, &serde_json::json!({ "owner": "recall" })).unwrap();

        assert!(path.is_symlink());
        assert_eq!(read_cursor_config(&target).unwrap()["owner"], "recall");
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
            display_command("claude", &add_args(Host::Claude, "/tmp/My Recall/recall").unwrap()),
            "claude mcp add --scope user recall -- '/tmp/My Recall/recall' mcp"
        );
        assert_eq!(quote_arg("plain"), "plain");
    }

    #[test]
    fn host_error_text_classifies_missing() {
        assert!(looks_like_not_found("MCP server recall not found"));
        assert!(!looks_like_not_found("connection refused"));
    }

    #[test]
    fn failed_install_keeps_existing_registration() {
        let mut registered = true;
        let args = add_args(Host::Claude, "recall").unwrap();
        let action = HostAction::Install { bin: "recall".into() };
        let error =
            run_host_command(Host::Claude, &args, false, &action, &mut |_, args| match args[1]
                .as_str()
            {
                "add" if registered => Ok(HostOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "MCP server recall already exists".into(),
                }),
                "add" => Ok(HostOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "simulated transient add failure".into(),
                }),
                "remove" => {
                    registered = false;
                    Ok(HostOutput { success: true, stdout: String::new(), stderr: String::new() })
                }
                action => panic!("unexpected host action: {action}"),
            })
            .unwrap_err();

        assert!(registered, "existing registration was removed: {error}");
    }
}
