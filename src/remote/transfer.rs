use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, ensure};

use crate::db::store::Store;
use crate::extension::transport::{ErrorCode, Object, Operation, RemoteError, Reply};

use super::object::{self, Name};

const TRANSFER_CONCURRENCY: usize = 16;
const GET_ATTEMPTS: usize = 3;

fn fetch(
    transport: &(dyn Fn(&Operation) -> Result<Reply> + Sync),
    object: &Object,
    output_path: &Path,
    aborted: &AtomicBool,
) -> Result<Reply> {
    let get = Operation::Get {
        key: object.key.clone(),
        output_path: output_path.to_path_buf(),
        max_bytes: object.size,
    };
    let mut attempt = 1;
    loop {
        match transport(&get) {
            Err(error)
                if attempt < GET_ATTEMPTS
                    && is_transient(&error)
                    && !aborted.load(Ordering::Relaxed) =>
            {
                attempt += 1;
                match std::fs::remove_file(output_path) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(error.into());
                    }
                    _ => {}
                }
            }
            reply => return reply,
        }
    }
}

fn is_transient(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RemoteError>().is_some_and(|error| error.code == ErrorCode::Transient)
}

pub(super) fn download_objects(
    store: &Store,
    progress: &mut crate::sync_progress::SyncProgress,
    transport: &(dyn Fn(&Operation) -> Result<Reply> + Sync),
    downloads: Vec<Object>,
    directory: &Path,
) -> Result<usize> {
    let pending = downloads.len();
    if pending == 0 {
        return Ok(0);
    }
    let workers = TRANSFER_CONCURRENCY.min(pending);
    let queue = Mutex::new(downloads.into_iter().enumerate());
    let aborted = AtomicBool::new(false);
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Result<(Object, PathBuf)>>(workers);

    std::thread::scope(|scope| -> Result<usize> {
        for _ in 0..workers {
            let sender = sender.clone();
            let queue = &queue;
            let aborted = &aborted;
            scope.spawn(move || {
                while !aborted.load(Ordering::Relaxed) {
                    let next = queue.lock().expect("download queue").next();
                    let Some((index, object)) = next else { break };
                    let path = directory.join(format!("download-{index}.json"));
                    let fetched = fetch(transport, &object, &path, aborted).and_then(|reply| {
                        let Reply::Downloaded(size) = reply else {
                            anyhow::bail!("provider returned an invalid get result");
                        };
                        ensure!(size == object.size, "remote object changed during download");
                        Ok((object, path))
                    });
                    if fetched.is_err() {
                        aborted.store(true, Ordering::Relaxed);
                    }
                    if sender.send(fetched).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        let mut downloaded = 0;
        let mut transferred = 0;
        let mut waiting: HashMap<String, Vec<(Name, Vec<u8>)>> = HashMap::new();
        let cached = (|| -> Result<()> {
            for fetched in &receiver {
                let (object, path) = fetched?;
                let body = std::fs::read(&path)?;
                ensure!(body.len() as u64 == object.size, "downloaded object length mismatch");
                std::fs::remove_file(&path)?;
                transferred += object.size;
                let mut ready = vec![(Name::parse(&object.key)?, body)];
                while let Some((name, body)) = ready.pop() {
                    let base = match name.base() {
                        Some(base) => match store.cached_remote(base)? {
                            Some(raw) => Some(raw),
                            None => {
                                waiting.entry(base.to_string()).or_default().push((name, body));
                                continue;
                            }
                        },
                        None => None,
                    };
                    let raw = name.decode(&body, base.as_deref())?;
                    let stored = (name.encoding == object::Encoding::Zstd).then_some(&body[..]);
                    store.cache_encoded(&name.canonical, &raw, stored)?;
                    downloaded += 1;
                    ready.extend(waiting.remove(&name.canonical).unwrap_or_default());
                }
                progress.detail(format!(
                    "Downloading objects {downloaded}/{pending} ({})",
                    crate::sync_progress::format_bytes(transferred)
                ));
            }
            let unresolved: usize = waiting.values().map(Vec::len).sum();
            if unresolved > 0 {
                tracing::warn!(
                    "skipped {unresolved} remote revisions whose base revision is not available yet"
                );
            }
            Ok(())
        })();
        if cached.is_err() {
            aborted.store(true, Ordering::Relaxed);
        }
        drop(receiver);
        cached.map(|()| downloaded)
    })
}

struct Upload {
    key: String,
    path: PathBuf,
    size: u64,
    sha256: String,
}

fn stage_upload(
    store: &Store,
    directory: &Path,
    index: usize,
    canonical: &str,
    available: &HashSet<String>,
) -> Result<Upload> {
    let (key, body) = object::encode(store, canonical, &|key| available.contains(key))?;
    let path = directory.join(format!("upload-{index}"));
    std::fs::write(&path, &body)?;
    let sha256 = crate::db::remote_store::digest(&body);
    Ok(Upload { key, path, size: body.len() as u64, sha256 })
}

pub(super) fn upload_objects(
    store: &Store,
    progress: &mut crate::sync_progress::SyncProgress,
    transport: &(dyn Fn(&Operation) -> Result<Reply> + Sync),
    uploads: Vec<String>,
    available: &HashSet<String>,
    directory: &Path,
) -> Result<usize> {
    let pending = uploads.len();
    if pending == 0 {
        return Ok(0);
    }
    let workers = TRANSFER_CONCURRENCY.min(pending);
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
            match stage_upload(store, directory, index, &key, available) {
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
