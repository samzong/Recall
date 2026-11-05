use crate::error::{Failure, Result};
use crate::protocol::{Operation, validate_key};
use opendal::blocking::Operator;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};

const CHUNK: usize = 4 * 1024 * 1024;

pub fn with_operator<T>(action: impl FnOnce() -> Result<T>) -> Result<T> {
    // OpenDAL's blocking API uses Tokio internally, including for HTTP backends.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(Failure::io)?;
    let _guard = runtime.enter();
    action()
}

pub fn verify(operator: &Operator) -> Result<()> {
    check_fs_root(operator)?;
    let options = opendal::options::ListOptions { limit: Some(1), ..Default::default() };
    // The pinned blocking check() swallows NotFound and has no limit. Consume
    // the actual bounded list response so missing/ambiguous targets fail.
    let mut entries = operator.lister_options("", options).map_err(Failure::backend)?;
    if let Some(entry) = entries.next() {
        entry.map_err(Failure::backend)?;
    }
    Ok(())
}

fn check_fs_root(operator: &Operator) -> Result<()> {
    if operator.info().scheme() == "fs" {
        let metadata = std::fs::metadata(operator.info().root()).map_err(Failure::io)?;
        if !metadata.is_dir() {
            return Err(Failure::new("target_missing", "filesystem root is not a directory"));
        }
    }
    Ok(())
}

pub fn execute(operator: &Operator, operation: Operation) -> Result<Value> {
    check_fs_root(operator)?;
    match operation {
        Operation::Probe => {
            verify(operator)?;
            Ok(json!({"readable":true}))
        }
        Operation::List { prefix, cursor, page_size } => {
            list(operator, &prefix, cursor.as_deref(), usize::from(page_size))
        }
        Operation::Get { key, output_path, max_bytes } => {
            let metadata = operator.stat(&key).map_err(Failure::backend)?;
            if !metadata.is_file() || metadata.content_length() > max_bytes {
                return Err(Failure::new("integrity", "object exceeds max_bytes or is not a file"));
            }
            let parent = output_path
                .parent()
                .ok_or_else(|| Failure::new("target_missing", "missing output directory"))?;
            let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(Failure::io)?;
            let reader = operator.reader(&key).map_err(Failure::backend)?;
            let mut reader = reader.into_std_read(..).map_err(Failure::backend)?;
            let mut size = 0u64;
            let mut buffer = vec![0; CHUNK];
            loop {
                let n = reader.read(&mut buffer).map_err(Failure::io)?;
                if n == 0 {
                    break;
                }
                size = size
                    .checked_add(n as u64)
                    .ok_or_else(|| Failure::new("integrity", "download size overflow"))?;
                if size > max_bytes {
                    return Err(Failure::new("integrity", "object exceeds max_bytes"));
                }
                temp.write_all(&buffer[..n]).map_err(Failure::io)?;
            }
            if size != metadata.content_length() {
                return Err(Failure::new("integrity", "object changed length during download"));
            }
            temp.as_file_mut().sync_all().map_err(Failure::io)?;
            temp.persist_noclobber(output_path).map_err(|error| Failure::io(error.error))?;
            Ok(json!({"size":size}))
        }
        Operation::Put { key, input_path, size, sha256 } => {
            // Snapshot while hashing, so a changing caller file cannot alter uploaded bytes.
            let mut source = std::fs::File::open(input_path).map_err(Failure::io)?;
            let mut snapshot = tempfile::tempfile().map_err(Failure::io)?;
            let mut digest = Sha256::new();
            let mut actual_size = 0u64;
            let mut buffer = vec![0; CHUNK];
            loop {
                let n = source.read(&mut buffer).map_err(Failure::io)?;
                if n == 0 {
                    break;
                }
                actual_size = actual_size
                    .checked_add(n as u64)
                    .ok_or_else(|| Failure::new("integrity", "upload size overflow"))?;
                if actual_size > size {
                    return Err(Failure::new("integrity", "upload size mismatch"));
                }
                digest.update(&buffer[..n]);
                snapshot.write_all(&buffer[..n]).map_err(Failure::io)?;
            }
            let actual_hash = format!("{:x}", digest.finalize());
            if actual_size != size || actual_hash != sha256 {
                return Err(Failure::new("integrity", "upload size or SHA-256 mismatch"));
            }
            snapshot.rewind().map_err(Failure::io)?;
            if operator.info().scheme() == "fs" {
                match publish_fs(operator, &key, &mut snapshot) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        verify_existing(operator, &key, actual_size, &actual_hash)?;
                    }
                    Err(error) => return Err(Failure::io(error)),
                }
            } else {
                match publish_remote(operator, &key, &mut snapshot, actual_size, &mut buffer) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            opendal::ErrorKind::ConditionNotMatch
                                | opendal::ErrorKind::AlreadyExists
                                | opendal::ErrorKind::Conflict
                        ) =>
                    {
                        verify_existing(operator, &key, actual_size, &actual_hash)?;
                    }
                    Err(error) => return Err(Failure::backend(error)),
                }
            }
            Ok(json!({"size":actual_size,"sha256":actual_hash}))
        }
    }
}

fn publish_fs(operator: &Operator, key: &str, snapshot: &mut std::fs::File) -> std::io::Result<()> {
    let root = std::path::PathBuf::from(operator.info().root());
    let staging = root.join(".recall-opendal-tmp");
    create_directory(&staging)?;
    let mut parent = root;
    let segments: Vec<_> = key.split('/').collect();
    for segment in &segments[..segments.len() - 1] {
        parent.push(segment);
        // Create one level at a time: a removed root must not be recreated.
        create_directory(&parent)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(&staging)?;
    std::io::copy(snapshot, file.as_file_mut())?;
    file.as_file_mut().sync_all()?;
    file.persist_noclobber(parent.join(segments[segments.len() - 1]))
        .map_err(|error| error.error)?;
    Ok(())
}

fn create_directory(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

fn publish_remote(
    operator: &Operator,
    key: &str,
    snapshot: &mut std::fs::File,
    size: u64,
    buffer: &mut [u8],
) -> opendal::Result<()> {
    let capability = operator.info().capability();
    if !capability.write_with_if_not_exists {
        return Err(opendal::Error::new(
            opendal::ErrorKind::Unsupported,
            "backend does not support conditional publication",
        ));
    }
    let options = opendal::options::WriteOptions { if_not_exists: true, ..Default::default() };
    if capability.write_can_multi {
        let mut writer = operator.writer_options(key, options)?;
        let result = (|| {
            loop {
                let n = snapshot.read(buffer).map_err(local_upload_error)?;
                if n == 0 {
                    break;
                }
                writer.write(buffer[..n].to_vec())?;
            }
            writer.close()?;
            Ok(())
        })();
        // Dropping the blocking writer never retries an unconditional publication.
        result
    } else {
        let capacity = usize::try_from(size).map_err(|_| {
            opendal::Error::new(opendal::ErrorKind::Unsupported, "upload cannot fit in memory")
        })?;
        let mut body = Vec::with_capacity(capacity);
        snapshot.read_to_end(&mut body).map_err(local_upload_error)?;
        operator.write_options(key, body, options)?;
        Ok(())
    }
}

fn local_upload_error(_: std::io::Error) -> opendal::Error {
    opendal::Error::new(opendal::ErrorKind::Unexpected, "cannot read upload snapshot")
}

fn verify_existing(operator: &Operator, key: &str, size: u64, sha256: &str) -> Result<()> {
    let conflict = || Failure::new("conflict", "immutable key contains different bytes");
    let metadata = operator.stat(key).map_err(Failure::backend)?;
    if !metadata.is_file() || metadata.content_length() != size {
        return Err(conflict());
    }
    let mut reader = operator
        .reader(key)
        .map_err(Failure::backend)?
        .into_std_read(..)
        .map_err(Failure::backend)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    let mut actual = 0u64;
    loop {
        let n = reader.read(&mut buffer).map_err(Failure::io)?;
        if n == 0 {
            break;
        }
        actual = actual.checked_add(n as u64).ok_or_else(conflict)?;
        if actual > size {
            return Err(conflict());
        }
        digest.update(&buffer[..n]);
    }
    if actual != size || format!("{:x}", digest.finalize()) != sha256 {
        return Err(conflict());
    }
    Ok(())
}

fn list(
    operator: &Operator,
    prefix: &str,
    cursor: Option<&str>,
    page_size: usize,
) -> Result<Value> {
    let capability = operator.info().capability();
    if !capability.list {
        return Err(Failure::new("unavailable", "backend must support list for remote listing"));
    }
    let options = opendal::options::ListOptions {
        recursive: true,
        limit: Some(page_size + 1),
        start_after: if capability.list_with_start_after {
            cursor.map(str::to_owned)
        } else {
            None
        },
        ..Default::default()
    };
    // OpenDAL's limit is a backend page-size hint, not a total-result limit.
    // Backends need not return sorted entries. Retain the smallest page+1 keys
    // across the iterator, using bounded memory and stable lexical cursors.
    let ordered = operator.info().scheme() == "s3" && capability.list_with_start_after;
    let mut keys = BTreeMap::new();
    for entry in operator.lister_options(prefix, options).map_err(Failure::backend)? {
        let entry = entry.map_err(Failure::backend)?;
        let key = entry.path();
        let metadata = entry.metadata();
        if metadata.is_dir() || !key.starts_with(prefix) || cursor.is_some_and(|c| key <= c) {
            continue;
        }
        validate_key(key)
            .map_err(|_| Failure::new("integrity", "backend returned an invalid remote key"))?;
        let size = if metadata.is_file() { Some(metadata.content_length()) } else { None };
        keys.insert(key.to_owned(), size);
        if ordered && keys.len() == page_size + 1 {
            break;
        }
        if keys.len() > page_size + 1 {
            keys.pop_last();
        }
    }
    let has_more = keys.len() > page_size;
    if has_more {
        keys.pop_last();
    }
    let next_cursor =
        if has_more { keys.last_key_value().map(|(key, _)| key.clone()) } else { None };
    let mut objects = Vec::with_capacity(keys.len());
    for (key, listed_size) in keys {
        // Most backends carry content_length in listing metadata; stat is a
        // fallback for those that do not.
        let size = match listed_size {
            Some(size) => size,
            None => {
                if !capability.stat {
                    return Err(Failure::new(
                        "integrity",
                        "listed object has no size and the backend cannot stat it",
                    ));
                }
                let metadata = operator.stat(&key).map_err(Failure::backend)?;
                if !metadata.is_file() {
                    return Err(Failure::new("integrity", "listed object is not a file"));
                }
                metadata.content_length()
            }
        };
        objects.push(json!({"key":key,"size":size}));
    }
    Ok(json!({"objects":objects,"next_cursor":next_cursor}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_rejects_backend_without_list_capability() {
        with_operator(|| {
            for disable_stat in [false, true] {
                let op = opendal::Operator::via_iter("memory", [])
                    .map_err(Failure::backend)?
                    .layer(opendal::layers::CapabilityOverrideLayer::new(move |mut cap| {
                        if disable_stat {
                            cap.stat = false;
                        } else {
                            cap.list = false;
                        }
                        cap
                    }));
                let op = Operator::new(op).map_err(Failure::backend)?;
                op.write("v1/a", "data").map_err(Failure::backend)?;
                let result = list(&op, "v1/", None, 1);
                if disable_stat {
                    // Sizes come from listing metadata, so stat is not needed.
                    assert!(result.is_ok(), "{result:?}");
                } else {
                    assert_eq!(result.unwrap_err().code, "unavailable");
                }
            }
            Ok(())
        })
        .unwrap();
    }
}
