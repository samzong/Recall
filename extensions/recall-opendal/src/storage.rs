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
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| Failure::new("unavailable", error.to_string()))?
        .as_nanos();
    let key = format!("recall-opendal-probe/{}-{stamp}", std::process::id());
    let content = b"recall-opendal configuration probe";
    operator.write(&key, content.to_vec()).map_err(Failure::backend)?;
    let read = operator.read(&key).map_err(Failure::backend);
    let delete = operator.delete(&key).map_err(Failure::backend);
    let bytes = read?;
    delete?;
    if bytes.to_vec() != content {
        return Err(Failure::new("integrity", "configuration probe readback mismatch"));
    }
    Ok(())
}

pub fn execute(operator: &Operator, operation: Operation) -> Result<Value> {
    match operation {
        Operation::Probe => {
            // stat("/") returns a synthetic DIR on several backends without I/O,
            // so probe must do a real listing instead. OpenDAL's check() does a
            // 1-entry list; NotFound on the root itself is mapped to
            // target_missing below.
            operator.check().map_err(|error| {
                let mut failure = Failure::backend(error);
                if failure.code == "object_missing" {
                    failure.code = "target_missing";
                }
                failure
            })?;
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
            if operator.info().capability().write_can_multi {
                let mut writer = operator.writer(&key).map_err(Failure::backend)?;
                loop {
                    let n = snapshot.read(&mut buffer).map_err(Failure::io)?;
                    if n == 0 {
                        break;
                    }
                    writer.write(buffer[..n].to_vec()).map_err(Failure::backend)?;
                }
                writer.close().map_err(Failure::backend)?;
            } else {
                let mut body = Vec::with_capacity(actual_size as usize);
                snapshot.read_to_end(&mut body).map_err(Failure::io)?;
                // Verified-identical conflict: write only when the key is absent.
                // An existing object under an immutable content-addressed key
                // already holds the same verified bytes.
                if operator.info().capability().write_with_if_not_exists {
                    let options = opendal::options::WriteOptions {
                        if_not_exists: true,
                        ..Default::default()
                    };
                    match operator.write_options(&key, body, options) {
                        Ok(_) => {}
                        Err(error) if error.kind() == opendal::ErrorKind::ConditionNotMatch => {
                            // Another writer raced us to the same immutable key;
                            // identical bytes are already published.
                        }
                        Err(error) => return Err(Failure::backend(error)),
                    }
                } else {
                    operator.write(&key, body).map_err(Failure::backend)?;
                }
            }
            Ok(json!({"size":actual_size,"sha256":actual_hash}))
        }
    }
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
    fn memory_listing_returns_correct_lexical_pages() {
        with_operator(|| {
            let op =
                Operator::new(opendal::Operator::via_iter("memory", []).map_err(Failure::backend)?)
                    .map_err(Failure::backend)?;
            for key in ["v1/z", "v1/a", "v1/m"] {
                op.write(key, key.to_owned()).map_err(Failure::backend)?;
            }
            assert_eq!(
                list(&op, "v1/", None, 2)?,
                json!({"objects":[
                {"key":"v1/a","size":4},{"key":"v1/m","size":4}],"next_cursor":"v1/m"})
            );
            assert_eq!(
                list(&op, "v1/", Some("v1/m"), 2)?,
                json!({"objects":[
                {"key":"v1/z","size":4}],"next_cursor":null})
            );
            verify(&op)?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn invalid_backend_keys_fail_without_invalid_page() {
        with_operator(|| {
            let op =
                Operator::new(opendal::Operator::via_iter("memory", []).map_err(Failure::backend)?)
                    .map_err(Failure::backend)?;
            op.write("v1/Upper", "bad").map_err(Failure::backend)?;
            assert_eq!(list(&op, "v1/", None, 1).unwrap_err().code, "integrity");
            Ok(())
        })
        .unwrap();
    }
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
