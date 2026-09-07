use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::db::store::Store;
use crate::extension::transport::{Operation, Reply};
use crate::host::Host;
use crate::project_scope::ProjectScope;

#[derive(Subcommand)]
pub(crate) enum RemoteCommands {
    #[command(about = "Configure the remote destination, upload scope, and host name")]
    Connect {
        #[arg(long, help = "Storage provider (r2)")]
        provider: Option<String>,
        #[arg(long, help = "Upload scope: a path, owner/repo, remote URL, or all")]
        project: Option<String>,
        #[arg(long, help = "Readable name of this machine")]
        host_name: Option<String>,
        #[arg(last = true, help = "Provider configuration arguments")]
        provider_args: Vec<OsString>,
    },
    #[command(about = "Disconnect while retaining local sessions and remote objects")]
    Disconnect,
    #[command(about = "Scan the saved scope and exchange indexed sessions")]
    Sync {
        #[arg(long, value_enum, default_value_t = crate::info::InfoFormat::Text)]
        format: crate::info::InfoFormat,
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
        ensure!(connection.provider == "r2", "unsupported remote provider");
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
        RemoteCommands::Disconnect => {
            let _lock = crate::utils::acquire_sync_lock()?;
            if let Some(mut settings) = load_settings()? {
                settings.connection = None;
                settings.save_to(&settings_path()?)?;
            }
            eprintln!("Remote disconnected. Indexed sessions and remote objects were retained.");
            Ok(())
        }
        RemoteCommands::Sync { format } => synchronize(format),
    }
}

const UPLOAD_CONCURRENCY: usize = 16;

#[derive(Default, Serialize)]
struct SyncSummary {
    downloaded: usize,
    uploaded: usize,
    sessions_with_alternatives: usize,
}

fn synchronize(format: crate::info::InfoFormat) -> Result<()> {
    let _lock = crate::utils::acquire_sync_lock()?;
    let settings =
        load_settings()?.context("remote is not connected; run recall remote connect")?;
    let connection =
        settings.connection.context("remote is disconnected; run recall remote connect")?;
    let mut progress = crate::sync_progress::SyncProgress::for_phases();
    progress.phase("Scanning sources");
    crate::sync::scan_remote_scope(connection.scope.clone())?;
    let store = Store::open()?;
    let summary = exchange(&store, &connection, &mut progress, &|operation| {
        crate::extension::transport::invoke(
            &connection.provider,
            operation,
            Duration::from_secs(120),
        )
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
            let path = temporary.path().join("download.json");
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            let Reply::Downloaded(size) = transport(&Operation::Get {
                key: object.key.clone(),
                output_path: path.clone(),
                max_bytes: crate::db::remote_store::OBJECT_LIMIT as u64,
            })?
            else {
                anyhow::bail!("provider returned an invalid get result");
            };
            ensure!(size == object.size, "remote object changed during download");
            let body = std::fs::read(&path)?;
            ensure!(body.len() as u64 == size, "downloaded object length mismatch");
            store.cache_remote(&object.key, &body)?;
            summary.downloaded += 1;
            progress.detail(format!(
                "Remote objects: {} listed, {} downloaded",
                remote_keys.len(),
                summary.downloaded
            ));
        }
        cursor = page.next_cursor;
        if let Some(cursor) = &cursor {
            ensure!(cursors.insert(cursor.clone()), "remote pagination cursor repeated");
        } else {
            break;
        }
    }
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

struct Upload {
    key: String,
    path: PathBuf,
    size: u64,
    sha256: String,
}

fn stage_upload(store: &Store, directory: &Path, index: usize, key: String) -> Result<Upload> {
    let body = store.cached_remote(&key)?.context("remote recovery object is missing locally")?;
    let path = directory.join(format!("upload-{index}.json"));
    std::fs::write(&path, &body)?;
    let sha256 = crate::db::remote_store::digest(&body);
    Ok(Upload { key, path, size: body.len() as u64, sha256 })
}

fn upload_objects(
    store: &Store,
    progress: &mut crate::sync_progress::SyncProgress,
    transport: &(dyn Fn(&Operation) -> Result<Reply> + Sync),
    uploads: Vec<String>,
    directory: &Path,
) -> Result<usize> {
    let pending = uploads.len();
    if pending == 0 {
        return Ok(0);
    }
    let workers = UPLOAD_CONCURRENCY.min(pending);
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Upload>(workers);
    let receiver = Mutex::new(receiver);
    let uploaded = AtomicUsize::new(0);
    let transferred = AtomicU64::new(0);
    let aborted = AtomicBool::new(false);
    let failure = Mutex::new(None);
    let line = |done: usize, bytes: u64| {
        format!(
            "Uploading objects {done}/{pending} ({})",
            crate::sync_progress::format_bytes(bytes)
        )
    };

    std::thread::scope(|scope| -> Result<usize> {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    loop {
                        let next = receiver.lock().expect("upload queue").recv();
                        let Ok(upload) = next else { break };
                        if aborted.load(Ordering::Relaxed) {
                            continue;
                        }
                        let published = (|| -> Result<()> {
                            let reply = transport(&Operation::Put {
                                key: upload.key,
                                input_path: upload.path.clone(),
                                size: upload.size,
                                sha256: upload.sha256,
                            })?;
                            ensure!(
                                reply == Reply::Published,
                                "provider returned an invalid put result"
                            );
                            std::fs::remove_file(&upload.path)?;
                            Ok(())
                        })();
                        match published {
                            Ok(()) => {
                                uploaded.fetch_add(1, Ordering::Relaxed);
                                transferred.fetch_add(upload.size, Ordering::Relaxed);
                            }
                            Err(error) => {
                                aborted.store(true, Ordering::Relaxed);
                                failure.lock().expect("upload failure").get_or_insert(error);
                            }
                        }
                    }
                })
            })
            .collect();

        let mut staging = Ok(());
        for (index, key) in uploads.into_iter().enumerate() {
            if aborted.load(Ordering::Relaxed) {
                break;
            }
            match stage_upload(store, directory, index, key) {
                Ok(upload) => {
                    if sender.send(upload).is_err() {
                        break;
                    }
                    progress.detail(line(
                        uploaded.load(Ordering::Relaxed),
                        transferred.load(Ordering::Relaxed),
                    ));
                }
                Err(error) => {
                    staging = Err(error);
                    break;
                }
            }
        }
        drop(sender);

        while !handles.iter().all(|handle| handle.is_finished()) {
            progress.detail(line(
                uploaded.load(Ordering::Relaxed),
                transferred.load(Ordering::Relaxed),
            ));
            std::thread::sleep(Duration::from_millis(100));
        }
        for handle in handles {
            handle.join().map_err(|_| anyhow::anyhow!("upload worker panicked"))?;
        }
        staging?;
        if let Some(error) = failure.lock().expect("upload failure").take() {
            return Err(error);
        }
        progress
            .detail(line(uploaded.load(Ordering::Relaxed), transferred.load(Ordering::Relaxed)));
        Ok(uploaded.load(Ordering::Relaxed))
    })
}

fn connect(
    provider: Option<String>,
    project: Option<String>,
    host_name: Option<String>,
    provider_args: &[OsString],
) -> Result<()> {
    let provider = provider.unwrap_or_else(|| "r2".into());
    ensure!(provider == "r2", "only the r2 provider is currently supported");
    let binary = crate::extension::remote_binary(&provider)?;
    let _lock = crate::utils::acquire_sync_lock()?;
    let existing = load_settings()?;
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    eprintln!("Storage: Cloudflare R2");
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
    let configured = std::process::Command::new(binary)
        .arg("--recall-remote-configure")
        .args(provider_args)
        .status()
        .context("cannot start provider configuration; remote remains disconnected")?;
    ensure!(configured.success(), "provider configuration failed; remote remains disconnected");
    ensure!(
        crate::extension::transport::invoke(&provider, &Operation::Probe, Duration::from_secs(30))
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
mod tests {
    use super::*;
    use crate::db::remote_store::{Revision, digest, revision_heads};
    use crate::extension::transport::{Object, Page};
    use std::collections::BTreeMap;

    fn seed(store: &Store, text: &str, native: bool) -> String {
        let record = serde_json::json!({
            "schema_version": 7, "record_type": "session",
            "session": {"source":"codex", "source_id":"shared-native-id", "title":"Synthetic remote fixture", "started_at":100, "directory":"/synthetic/project", "source_file_path":"/synthetic/session.jsonl"},
            "messages":[{"seq":0,"role":"user","timestamp":100,"content":text}],
            "usage_events":[{"event_key":"usage-1","event_seq":0,"timestamp":100,"model":"synthetic","provider":"test","input_tokens":3,"output_tokens":2,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0,"token_source":"observed"}],
            "events":[{"event_seq":0,"kind":"tool","actor":"assistant","name":"synthetic-tool","summary":"synthetic event"}]
        });
        crate::import::import_jsonl(store, false, record.to_string().as_bytes()).unwrap();
        if native {
            store.clear_import_marker("codex", "shared-native-id").unwrap();
        }
        store.get_session_by_source_id("codex", "shared-native-id").unwrap().unwrap().id
    }

    fn transfer(store: &Store, cloud: &mut BTreeMap<String, Vec<u8>>) -> Result<SyncSummary> {
        let cloud = Mutex::new(cloud);
        exchange(
            store,
            &Connection { provider: "r2".into(), scope: ProjectScope::Global },
            &mut crate::sync_progress::SyncProgress::disabled(),
            &|operation| {
                let mut cloud = cloud.lock().expect("synthetic cloud");
                Ok(match operation {
                    Operation::List { cursor, .. } => {
                        if cursor.is_none() {
                            Reply::Listed(Page {
                                objects: Vec::new(),
                                next_cursor: Some("nonempty-next-page".into()),
                            })
                        } else {
                            Reply::Listed(Page {
                                objects: cloud
                                    .iter()
                                    .map(|(key, value)| Object {
                                        key: key.clone(),
                                        size: value.len() as u64,
                                    })
                                    .collect(),
                                next_cursor: None,
                            })
                        }
                    }
                    Operation::Get { key, output_path, .. } => {
                        let body = cloud.get(key).context("synthetic object missing")?;
                        std::fs::write(output_path, body)?;
                        Reply::Downloaded(body.len() as u64)
                    }
                    Operation::Put { key, input_path, size, sha256 } => {
                        let body = std::fs::read(input_path)?;
                        ensure!(
                            body.len() as u64 == *size && digest(&body) == *sha256,
                            "bad upload"
                        );
                        if let Some(existing) = cloud.get(key) {
                            ensure!(existing == &body, "immutable overwrite");
                        }
                        cloud.insert(key.clone(), body);
                        Reply::Published
                    }
                    Operation::Probe => Reply::Readable,
                })
            },
        )
    }

    #[test]
    fn remote_large_revision_preserves_complete_events_and_recovers_objects() {
        crate::db::schema::register_sqlite_vec();
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        let id = seed(&a, "large event snapshot", true);
        let attrs = serde_json::json!({"payload": "x".repeat(64 * 1024 * 1024 + 1)}).to_string();
        a.conn
            .execute(
                "UPDATE session_events SET attrs_json = ?1 WHERE session_id = ?2",
                rusqlite::params![attrs, id],
            )
            .unwrap();
        let mut cloud = BTreeMap::new();
        transfer(&a, &mut cloud).unwrap();
        assert!(cloud.values().any(|body| body.len() > 64 * 1024 * 1024));
        transfer(&b, &mut cloud).unwrap();
        let received = b.list_recent_sessions(1).unwrap().pop().unwrap();
        let actual: String = b
            .conn
            .query_row(
                "SELECT attrs_json FROM session_events WHERE session_id = ?1",
                [&received.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual, attrs);
        assert_eq!(transfer(&b, &mut cloud).unwrap().uploaded, 0);
        let key = cloud.iter().max_by_key(|(_, body)| body.len()).unwrap().0.clone();
        let lost = cloud.remove(&key).unwrap();
        assert_eq!(transfer(&b, &mut cloud).unwrap().uploaded, 1);
        assert_eq!(cloud[&key], lost);
    }

    #[test]
    fn remote_roundtrip_updates_replicas_without_native_ownership_and_recovers_objects() {
        crate::db::schema::register_sqlite_vec();
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        let a_id = seed(&a, "original synthetic message", true);
        let mut host =
            Host { id: uuid::Uuid::new_v4().to_string(), name: "synthetic-a".into(), revision: 1 };
        host.observe(&a.conn, "codex", "shared-native-id").unwrap();
        let mut cloud = BTreeMap::new();
        assert!(transfer(&a, &mut cloud).unwrap().uploaded > 0);
        transfer(&b, &mut cloud).unwrap();
        let b_session = b.list_recent_sessions(10).unwrap().pop().unwrap();
        assert_ne!(a_id, b_session.id);
        assert!(!b.has_native_binding(&b_session.id).unwrap());
        assert_eq!(
            b.remote_snapshot(b_session.clone()).unwrap(),
            a.remote_snapshot(a.get_session_by_id(&a_id).unwrap().unwrap()).unwrap()
        );
        assert_eq!(b_session.locations[0].host, host);
        assert_eq!(b.list_usage_events_for_session(&b_session.id).unwrap().len(), 1);
        assert_eq!(b.list_session_events_for_session(&b_session.id).unwrap().len(), 1);
        let count = a.session_revisions(&a_id).unwrap().len();
        host.name = "renamed-a".into();
        host.revision += 1;
        host.observe(&a.conn, "codex", "shared-native-id").unwrap();
        transfer(&a, &mut cloud).unwrap();
        transfer(&b, &mut cloud).unwrap();
        assert_eq!(a.session_revisions(&a_id).unwrap().len(), count);
        assert_eq!(b.get_session_by_id(&b_session.id).unwrap().unwrap().locations[0].host, host);
        a.conn
            .execute(
                "UPDATE messages SET content = 'updated synthetic message' WHERE session_id = ?1",
                [&a_id],
            )
            .unwrap();
        transfer(&a, &mut cloud).unwrap();
        transfer(&b, &mut cloud).unwrap();
        assert_eq!(b.get_messages(&b_session.id).unwrap()[0].content, "updated synthetic message");
        assert_eq!(transfer(&b, &mut cloud).unwrap().uploaded, 0);
        let lost = cloud.keys().find(|key| key.starts_with("v1/revisions/")).unwrap().clone();
        let expected = cloud.remove(&lost).unwrap();
        assert_eq!(transfer(&b, &mut cloud).unwrap().uploaded, 1);
        assert_eq!(cloud[&lost], expected);
        assert_eq!(b.stats().unwrap().0, 1);
    }

    #[test]
    fn remote_legacy_exact_match_propagates_association_and_keeps_independent_id_collision() {
        crate::db::schema::register_sqlite_vec();
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        let c = Store::open_in_memory().unwrap();
        let a_id = seed(&a, "same exported message", true);
        let b_id = seed(&b, "same exported message", false);
        let mut cloud = BTreeMap::new();
        transfer(&b, &mut cloud).unwrap();
        transfer(&c, &mut cloud).unwrap();
        transfer(&a, &mut cloud).unwrap();
        transfer(&b, &mut cloud).unwrap();
        transfer(&c, &mut cloud).unwrap();
        transfer(&a, &mut cloud).unwrap();
        assert_eq!(a.stats().unwrap().0, 1);
        assert_eq!(b.stats().unwrap().0, 1);
        assert_eq!(c.stats().unwrap().0, 1);
        assert!(a.has_native_binding(&a_id).unwrap());
        assert!(b.get_session_by_id(&b_id).unwrap().is_some());
        a.conn
            .execute(
                "UPDATE messages SET content = 'subsequent source update' WHERE session_id = ?1",
                [&a_id],
            )
            .unwrap();
        transfer(&a, &mut cloud).unwrap();
        transfer(&b, &mut cloud).unwrap();
        assert_eq!(b.get_messages(&b_id).unwrap()[0].content, "subsequent source update");
        let independent = Store::open_in_memory().unwrap();
        seed(&independent, "independent same-native-id session", true);
        transfer(&independent, &mut cloud).unwrap();
        transfer(&a, &mut cloud).unwrap();
        assert_eq!(a.stats().unwrap().0, 2);
        assert_eq!(a.get_native_session("codex", "shared-native-id").unwrap().unwrap().id, a_id);
        assert!(a.get_session_by_source_id("codex", "shared-native-id").is_err());
    }

    #[test]
    fn remote_concurrent_revisions_keep_one_projection_and_all_full_snapshots() {
        crate::db::schema::register_sqlite_vec();
        let source = Store::open_in_memory().unwrap();
        let receiver = Store::open_in_memory().unwrap();
        seed(&source, "base revision", true);
        let mut cloud = BTreeMap::new();
        transfer(&source, &mut cloud).unwrap();
        transfer(&receiver, &mut cloud).unwrap();
        let original = cloud.iter().find(|(key, _)| key.starts_with("v1/revisions/")).unwrap();
        let parent = digest(original.1);
        let body = original.1.clone();
        for text in ["concurrent left", "concurrent right"] {
            let mut revision: Revision = serde_json::from_slice(&body).unwrap();
            revision.parent = Some(parent.clone());
            revision.record["messages"][0]["content"] = text.into();
            let body = serde_json::to_vec(&revision).unwrap();
            cloud.insert(format!("v1/revisions/{}.json", digest(&body)), body);
        }
        assert_eq!(transfer(&receiver, &mut cloud).unwrap().sessions_with_alternatives, 1);
        let session = receiver.list_recent_sessions(10).unwrap().pop().unwrap();
        assert_eq!(receiver.stats().unwrap().0, 1);
        assert_eq!(receiver.get_messages(&session.id).unwrap()[0].content, "base revision");
        let revisions = receiver.session_revisions(&session.id).unwrap();
        assert_eq!(revisions.len(), 3);
        assert_eq!(revision_heads(&revisions).len(), 2);
        assert_eq!(receiver.list_usage_events_for_session(&session.id).unwrap().len(), 1);
        assert_eq!(session.alternative_versions, 2);
        let fresh = Store::open_in_memory().unwrap();
        transfer(&fresh, &mut cloud).unwrap();
        let fresh_session = fresh.list_recent_sessions(10).unwrap().pop().unwrap();
        assert!(
            fresh.get_messages(&fresh_session.id).unwrap()[0].content.starts_with("concurrent ")
        );
        assert_eq!(fresh_session.alternative_versions, 1);
    }
}
