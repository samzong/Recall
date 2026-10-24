mod transfer;

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::db::store::Store;
use crate::extension::transport::{Operation, Reply};
use crate::host::Host;
use crate::project_scope::ProjectScope;
use transfer::{download_objects, upload_objects};

#[derive(Subcommand)]
pub(crate) enum RemoteCommands {
    #[command(about = "Configure the remote destination, upload scope, and host name")]
    Connect {
        #[arg(long, help = "Installed storage provider (default: r2)")]
        provider: Option<String>,
        #[arg(long, help = "Upload scope: a path, owner/repo, remote URL, or all")]
        project: Option<String>,
        #[arg(long, help = "Readable name of this machine")]
        host_name: Option<String>,
        #[arg(last = true, help = "Provider configuration arguments")]
        provider_args: Vec<OsString>,
    },
    #[command(about = "Show the saved remote connection without contacting storage")]
    Status {
        #[arg(long, value_enum, default_value_t = crate::info::InfoFormat::Text)]
        format: crate::info::InfoFormat,
    },
    #[command(about = "Disconnect while retaining local sessions and remote objects")]
    Disconnect,
    #[command(
        about = "Scan the saved scope and exchange indexed sessions",
        args_conflicts_with_subcommands = true
    )]
    Sync {
        #[command(subcommand)]
        command: Option<RemoteSyncCommands>,
        #[arg(long, value_enum, default_value_t = crate::info::InfoFormat::Text)]
        format: crate::info::InfoFormat,
    },
}

#[derive(Subcommand)]
pub(crate) enum RemoteSyncCommands {
    #[command(about = "Manage scheduled remote sync (macOS only)")]
    Schedule {
        #[command(subcommand)]
        command: crate::schedule::ScheduleCommands,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Connection {
    pub(crate) provider: String,
    pub(crate) scope: ProjectScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub(crate) host: Host,
    pub(crate) connection: Option<Connection>,
}

fn settings_path() -> Result<PathBuf> {
    Ok(crate::config::config_path()?.with_file_name("remote.json"))
}

pub(crate) fn load_settings() -> Result<Option<Settings>> {
    load_from(&settings_path()?)
}

fn load_from(path: &Path) -> Result<Option<Settings>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot read remote configuration"),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "remote configuration is too large");
    let settings: Settings =
        serde_json::from_slice(&bytes).context("invalid remote configuration")?;
    settings.host.validate()?;
    if let Some(connection) = &settings.connection {
        crate::extension::validate_extension_name(&connection.provider)?;
        validate_scope(&connection.scope)?;
    }
    Ok(Some(settings))
}

fn validate_scope(scope: &ProjectScope) -> Result<()> {
    let path = match scope {
        ProjectScope::Directory(path) => Some(path),
        ProjectScope::Repository { local_root, .. } => local_root.as_ref(),
        ProjectScope::Global => None,
    };
    ensure!(
        path.is_none_or(|path| Path::new(path).is_absolute()),
        "saved upload scope must use an absolute path"
    );
    Ok(())
}

impl Settings {
    fn save_to(&self, path: &Path) -> Result<()> {
        self.host.validate()?;
        let parent = path.parent().context("remote configuration has no parent directory")?;
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temporary, self)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

pub(crate) fn run(command: RemoteCommands) -> Result<()> {
    match command {
        RemoteCommands::Connect { provider, project, host_name, provider_args } => {
            connect(provider, project, host_name, &provider_args)
        }
        RemoteCommands::Status { format } => status(format),
        RemoteCommands::Disconnect => {
            let _lock = crate::utils::acquire_sync_lock()?;
            if let Some(mut settings) = load_settings()? {
                settings.connection = None;
                settings.save_to(&settings_path()?)?;
            }
            eprintln!("Remote disconnected. Indexed sessions and remote objects were retained.");
            Ok(())
        }
        RemoteCommands::Sync {
            command: Some(RemoteSyncCommands::Schedule { command }), ..
        } => crate::schedule::run(command, crate::schedule::Job::RemoteSync),
        RemoteCommands::Sync { command: None, format } => synchronize(format),
    }
}

fn status(format: crate::info::InfoFormat) -> Result<()> {
    let settings = load_settings()?;
    match format {
        crate::info::InfoFormat::Json => println!("{}", serde_json::to_string(&settings)?),
        crate::info::InfoFormat::Text => match settings {
            Some(settings) => {
                if let Some(connection) = settings.connection {
                    println!(
                        "Remote: {}\nUpload scope: {}",
                        connection.provider,
                        connection.scope.value().unwrap_or("all")
                    );
                } else {
                    println!("Remote: disconnected");
                }
                println!("Host: {}", settings.host.name);
            }
            None => println!("Remote: not configured"),
        },
    }
    Ok(())
}

#[derive(Default, Serialize)]
struct SyncSummary {
    downloaded: usize,
    uploaded: usize,
    sessions_with_alternatives: usize,
}

const TRANSPORT_DEADLINE: Duration = Duration::from_secs(120);
const TRANSFER_RATE_FLOOR: u64 = 64 * 1024;

fn transport_budget(operation: &Operation) -> Duration {
    let bytes = match operation {
        Operation::Get { max_bytes, .. } => *max_bytes,
        Operation::Put { size, .. } => *size,
        Operation::Probe | Operation::List { .. } => 0,
    };
    TRANSPORT_DEADLINE + Duration::from_secs(bytes / TRANSFER_RATE_FLOOR)
}

fn synchronize(format: crate::info::InfoFormat) -> Result<()> {
    let _lock = crate::utils::acquire_sync_lock()?;
    let settings =
        load_settings()?.context("remote is not connected; run recall remote connect")?;
    let connection =
        settings.connection.context("remote is disconnected; run recall remote connect")?;
    let binary = crate::extension::remote_binary(&connection.provider)?;
    let mut progress = crate::sync_progress::SyncProgress::for_phases();
    progress.phase("Scanning sources");
    crate::sync::scan_remote_scope(connection.scope.clone())?;
    let store = Store::open()?;
    let summary = exchange(&store, &connection, &mut progress, &|operation| {
        crate::extension::transport::invoke(&binary, operation, transport_budget(operation))
    })?;
    progress.finish();
    match format {
        crate::info::InfoFormat::Json => println!("{}", serde_json::to_string(&summary)?),
        crate::info::InfoFormat::Text => println!(
            "Downloaded {} objects; uploaded {} objects; {} sessions have alternative versions.",
            summary.downloaded, summary.uploaded, summary.sessions_with_alternatives
        ),
    }
    Ok(())
}

fn exchange(
    store: &Store,
    connection: &Connection,
    progress: &mut crate::sync_progress::SyncProgress,
    transport: &(dyn Fn(&Operation) -> Result<Reply> + Sync),
) -> Result<SyncSummary> {
    progress.phase("Preparing sessions");
    store.prepare_remote(&connection.scope, &mut |done, total| {
        progress.detail(format!("Preparing sessions {done}/{total}"))
    })?;
    progress.phase("Enumerating remote objects");
    let mut summary = SyncSummary::default();
    let mut cursor = None;
    let mut cursors = HashSet::new();
    let mut remote_keys = HashSet::new();
    let mut downloads = Vec::new();
    let temporary = tempfile::tempdir()?;
    loop {
        let Reply::Listed(page) =
            transport(&Operation::List { prefix: "v1/".into(), cursor, page_size: 1000 })?
        else {
            anyhow::bail!("provider returned an invalid list result");
        };
        for object in page.objects {
            ensure!(
                object.size <= crate::db::remote_store::OBJECT_LIMIT as u64,
                "remote object exceeds 512 MiB"
            );
            if !remote_keys.insert(object.key.clone()) {
                continue;
            }
            if let Some(cached) = store.cached_remote_size(&object.key)? {
                ensure!(
                    cached == object.size,
                    "remote object size differs from its cached content"
                );
                continue;
            }
            downloads.push(object);
        }
        progress.detail(format!(
            "Remote objects: {} listed, {} new",
            remote_keys.len(),
            downloads.len()
        ));
        cursor = page.next_cursor;
        if let Some(cursor) = &cursor {
            ensure!(cursors.insert(cursor.clone()), "remote pagination cursor repeated");
        } else {
            break;
        }
    }
    progress.phase("Downloading objects");
    summary.downloaded = download_objects(store, progress, transport, downloads, temporary.path())?;
    progress.phase("Merging revisions");
    summary.sessions_with_alternatives = store.merge_remote()?;
    let uploads: Vec<String> = store
        .remote_uploads(&connection.scope)?
        .into_iter()
        .filter(|key| !remote_keys.contains(key))
        .collect();
    progress.phase("Uploading objects");
    summary.uploaded = upload_objects(store, progress, transport, uploads, temporary.path())?;
    Ok(summary)
}

fn connect(
    provider: Option<String>,
    project: Option<String>,
    host_name: Option<String>,
    provider_args: &[OsString],
) -> Result<()> {
    let provider = provider.unwrap_or_else(|| "r2".into());
    let binary = crate::extension::remote_binary(&provider)?;
    let _lock = crate::utils::acquire_sync_lock()?;
    let existing = load_settings()?;
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    eprintln!("Storage: {provider}");
    let project = match project {
        Some(project) => project,
        None => {
            ensure!(interactive, "non-interactive connect requires --project");
            prompt("Upload scope (path, owner/repo, remote URL, or all)", None)?
        }
    };
    ensure!(!project.trim().is_empty(), "choose an explicit upload scope");
    let store = Store::open()?;
    let mut scope = store.resolve_project_selector(&project)?;
    if let ProjectScope::Directory(path) = &mut scope {
        *path = std::path::absolute(&*path)?.to_string_lossy().into_owned();
    }
    validate_scope(&scope)?;
    let name = match host_name {
        Some(name) => name,
        None => {
            ensure!(interactive, "non-interactive connect requires --host-name");
            let suggested = existing
                .as_ref()
                .map(|settings| settings.host.name.clone())
                .unwrap_or_else(suggested_host_name);
            prompt("This machine's name", Some(&suggested))?
        }
    };
    let mut host = existing.map(|settings| settings.host).unwrap_or_else(|| Host {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.clone(),
        revision: 1,
    });
    if host.name != name {
        host.revision = host.revision.checked_add(1).context("host revision overflow")?;
        host.name = name;
    }
    host.validate()?;
    if interactive {
        eprintln!("Upload scope: {}\nThis machine: {}", scope.value().unwrap_or("all"), host.name);
        let answer = prompt("Continue to destination setup? [y/N]", None)?;
        ensure!(
            matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"),
            "connection cancelled"
        );
    }
    let mut settings = Settings { host, connection: None };
    let path = settings_path()?;
    settings.save_to(&path)?;
    let configured = std::process::Command::new(&binary)
        .arg("--recall-remote-configure")
        .args(provider_args)
        .status()
        .context("cannot start provider configuration; remote remains disconnected")?;
    ensure!(configured.success(), "provider configuration failed; remote remains disconnected");
    ensure!(
        crate::extension::transport::invoke(&binary, &Operation::Probe, Duration::from_secs(30))
            .context("remote probe failed; remote remains disconnected")?
            == Reply::Readable,
        "provider is not readable; remote remains disconnected"
    );
    settings.connection = Some(Connection { provider, scope });
    settings.save_to(&path)?;
    eprintln!(
        "Remote configured. No sessions were uploaded; run recall remote sync to synchronize."
    );
    Ok(())
}

fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    if let Some(default) = default {
        eprint!("{label} [{default}]: ");
    } else {
        eprint!("{label}: ");
    }
    std::io::stderr().flush()?;
    let mut line = String::new();
    ensure!(std::io::stdin().read_line(&mut line)? > 0, "connection cancelled");
    let line = line.trim();
    Ok(if line.is_empty() { default.unwrap_or_default().to_string() } else { line.to_string() })
}

fn suggested_host_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|name| name.trim().to_string())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this-machine".into())
}

#[cfg(test)]
mod tests;
