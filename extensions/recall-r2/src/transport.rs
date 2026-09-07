use std::path::Path;

use aws_config::profile::ProfileFileCredentialsProvider;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, ProvideCredentials, Region, RequestChecksumCalculation};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::Target;
use crate::protocol::{Failure, Operation, Result, validate_key};

pub(crate) async fn execute(target: &Target, operation: Operation) -> Result<Value> {
    let credentials = ProfileFileCredentialsProvider::builder()
        .profile_name(&target.credential_profile).build().provide_credentials().await
        .map_err(|_| Failure::new("authentication", "cannot load the local R2 credential profile; configure its access key in the AWS credentials file"))?;
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("auto"))
        .endpoint_url(&target.endpoint)
        .force_path_style(true)
        .credentials_provider(credentials)
        .retry_config(RetryConfig::standard().with_max_attempts(3))
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .build();
    run(&Client::from_conf(config), target, operation).await
}

async fn run(client: &Client, target: &Target, operation: Operation) -> Result<Value> {
    match operation {
        Operation::Probe => {
            client
                .list_objects_v2()
                .bucket(&target.bucket)
                .prefix(&target.prefix)
                .max_keys(1)
                .send()
                .await
                .map_err(sdk_failure)?;
            Ok(json!({"readable": true}))
        }
        Operation::List { prefix, cursor, page_size } => {
            if !(1..=1000).contains(&page_size) {
                return Err(Failure::invalid("page_size must be between 1 and 1000"));
            }
            let full_prefix = target.full_key(&prefix, true)?;
            let page = client
                .list_objects_v2()
                .bucket(&target.bucket)
                .prefix(&full_prefix)
                .set_continuation_token(cursor)
                .max_keys(i32::from(page_size))
                .send()
                .await
                .map_err(sdk_failure)?;
            if page.contents().len() > usize::from(page_size) {
                return Err(Failure::integrity("R2 returned more objects than requested"));
            }
            let mut objects = Vec::with_capacity(page.contents().len());
            for object in page.contents() {
                let full_key =
                    object.key().ok_or_else(|| Failure::integrity("R2 object key is missing"))?;
                if !full_key.starts_with(&full_prefix) || full_key.len() > 1024 {
                    return Err(Failure::integrity(
                        "R2 returned a key outside the requested directory",
                    ));
                }
                let key = full_key.strip_prefix(&target.prefix).ok_or_else(|| {
                    Failure::integrity("R2 returned a key outside the target directory")
                })?;
                validate_key(key, false)
                    .map_err(|_| Failure::integrity("R2 returned an invalid relative key"))?;
                let size = length(object.size())?;
                objects.push(json!({"key": key, "size": size}));
            }
            let next_cursor = page.next_continuation_token();
            let truncated = page.is_truncated().ok_or_else(|| {
                Failure::integrity("R2 did not identify whether the page is complete")
            })?;
            if truncated != next_cursor.is_some() || next_cursor.is_some_and(str::is_empty) {
                return Err(Failure::integrity("R2 returned inconsistent pagination metadata"));
            }
            Ok(json!({"objects": objects, "next_cursor": next_cursor}))
        }
        Operation::Get { key, output_path, max_bytes } => {
            absolute_path(&output_path)?;
            let key = target.full_key(&key, false)?;
            let object = client
                .get_object()
                .bucket(&target.bucket)
                .key(key)
                .send()
                .await
                .map_err(sdk_failure)?;
            let expected = length(object.content_length())?;
            if expected > max_bytes {
                return Err(Failure::integrity("R2 object exceeds the requested download limit"));
            }
            let parent = output_path.parent().ok_or_else(Failure::io)?;
            let temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| Failure::io())?;
            let mut file = File::from_std(temporary.reopen().map_err(|_| Failure::io())?);
            let (size, _) = read_body(object.body, expected, max_bytes, Some(&mut file)).await?;
            file.flush().await.map_err(|_| Failure::io())?;
            file.sync_all().await.map_err(|_| Failure::io())?;
            drop(file);
            temporary.persist(&output_path).map_err(|_| Failure::io())?;
            Ok(json!({"size": size}))
        }
        Operation::Put { key, input_path, size, sha256 } => {
            absolute_path(&input_path)?;
            let key = target.full_key(&key, false)?;
            if sha256.len() != 64
                || !sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Failure::invalid("sha256 must be 64 lowercase hexadecimal characters"));
            }
            let content_length =
                i64::try_from(size).map_err(|_| Failure::invalid("upload size is too large"))?;
            if !tokio::fs::metadata(&input_path).await.map_err(|_| Failure::io())?.is_file() {
                return Err(Failure::invalid("upload input must be a regular file"));
            }
            let mut input = File::open(&input_path).await.map_err(|_| Failure::io())?;
            let mut digest = Sha256::new();
            let mut count = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = input.read(&mut buffer).await.map_err(|_| Failure::io())?;
                if read == 0 {
                    break;
                }
                count = count
                    .checked_add(read as u64)
                    .ok_or_else(|| Failure::integrity("upload size overflow"))?;
                if count > size {
                    return Err(Failure::integrity("upload length does not match request"));
                }
                digest.update(&buffer[..read]);
            }
            if count != size || format!("{:x}", digest.finalize()) != sha256 {
                return Err(Failure::integrity("upload length or SHA-256 does not match request"));
            }
            drop(input);
            let body = ByteStream::from_path(&input_path).await.map_err(|_| Failure::io())?;
            let uploaded = client
                .put_object()
                .bucket(&target.bucket)
                .key(&key)
                .if_none_match("*")
                .content_length(content_length)
                .body(body)
                .send()
                .await;
            if let Err(error) = uploaded {
                if error.raw_response().is_some_and(|response| response.status().as_u16() == 412) {
                    let existing = client
                        .get_object()
                        .bucket(&target.bucket)
                        .key(&key)
                        .send()
                        .await
                        .map_err(sdk_failure)?;
                    let expected = length(existing.content_length())?;
                    if expected != size {
                        return Err(Failure::new(
                            "conflict",
                            "immutable key contains different bytes",
                        ));
                    }
                    let (_, digest) = read_body(existing.body, expected, size, None).await?;
                    if digest != sha256 {
                        return Err(Failure::new(
                            "conflict",
                            "immutable key contains different bytes",
                        ));
                    }
                } else {
                    return Err(sdk_failure(error));
                }
            }
            Ok(json!({"size": size, "sha256": sha256}))
        }
    }
}

fn absolute_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Failure::invalid("transfer paths must be absolute"));
    }
    Ok(())
}

fn length(value: Option<i64>) -> Result<u64> {
    value
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| Failure::integrity("R2 returned a missing or invalid object length"))
}

async fn read_body(
    mut body: ByteStream,
    expected: u64,
    maximum: u64,
    mut file: Option<&mut File>,
) -> Result<(u64, String)> {
    let mut size = 0_u64;
    let mut digest = Sha256::new();
    while let Some(bytes) = body.next().await {
        let bytes = bytes.map_err(|_| Failure::new("transient", "R2 download stream failed"))?;
        size = size
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Failure::integrity("download size overflow"))?;
        if size > maximum || size > expected {
            return Err(Failure::integrity("R2 download exceeds its length or limit"));
        }
        digest.update(&bytes);
        if let Some(file) = file.as_mut() {
            file.write_all(&bytes).await.map_err(|_| Failure::io())?;
        }
    }
    if size != expected {
        return Err(Failure::integrity("R2 download is incomplete"));
    }
    Ok((size, format!("{:x}", digest.finalize())))
}

fn sdk_failure<E: ProvideErrorMetadata>(error: SdkError<E>) -> Failure {
    let code = error.as_service_error().and_then(ProvideErrorMetadata::code);
    match code {
        Some("NoSuchBucket") => Failure::new("target_missing", "R2 bucket does not exist"),
        Some("NoSuchKey") => Failure::new("object_missing", "R2 object does not exist"),
        Some(
            "Unauthorized"
            | "InvalidAccessKeyId"
            | "SignatureDoesNotMatch"
            | "ExpiredToken"
            | "ExpiredRequest"
            | "InvalidToken",
        ) => Failure::new("authentication", "R2 credentials or request signature were rejected"),
        Some("AccessDenied" | "NotEntitled") => {
            Failure::new("permission", "R2 denied the requested operation")
        }
        Some("PreconditionFailed" | "ConditionalRequestConflict") => {
            Failure::new("transient", "R2 conditional request conflicted; retry synchronization")
        }
        Some("EntityTooLarge" | "InvalidArgument" | "InvalidBucketName" | "InvalidObjectName") => {
            Failure::invalid("R2 rejected the request parameters")
        }
        Some("BadDigest" | "InvalidDigest") => Failure::integrity("R2 rejected the content digest"),
        _ => {
            let status = error.raw_response().map(|response| response.status().as_u16());
            if matches!(error, SdkError::TimeoutError(_) | SdkError::DispatchFailure(_))
                || matches!(status, Some(408 | 429 | 500..=599))
            {
                Failure::new(
                    "transient",
                    "R2 request failed or timed out; remote write outcome may be unknown",
                )
            } else {
                Failure::new("unavailable", "R2 request failed without a conclusive service error")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    fn target() -> Target {
        Target {
            endpoint: String::new(),
            bucket: "recall-test".into(),
            prefix: "foo/".into(),
            credential_profile: String::new(),
        }
    }

    fn reply(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nContent-Type: application/xml\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn server(replies: Vec<String>) -> (Client, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut requests = Vec::new();
                for reply in replies {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let mut buffer = [0_u8; 4096];
                    loop {
                        let count = stream.read(&mut buffer).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&buffer[..count]);
                        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let headers =
                                String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                            let length: usize = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .unwrap_or("0")
                                .trim()
                                .parse()
                                .unwrap();
                            if bytes.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    requests.push(String::from_utf8(bytes).unwrap());
                    stream.write_all(reply.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
                requests
            })
            .await
            .unwrap()
        });
        (client(&endpoint), task)
    }

    fn client(endpoint: &str) -> Client {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("auto"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test-id",
                "test-secret",
                None,
                None,
                "test",
            ))
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .build();
        Client::from_conf(config)
    }

    #[tokio::test]
    async fn list_preserves_empty_pages_and_cursor_and_scopes_keys() {
        let xml = "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>opaque+/=</NextContinuationToken></ListBucketResult>";
        let last = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>foo/revisions/abc.json</Key><Size>3</Size></Contents></ListBucketResult>";
        let (client, task) = server(vec![reply(200, xml), reply(200, last)]).await;
        let page = run(
            &client,
            &target(),
            Operation::List { prefix: "revisions/".into(), cursor: None, page_size: 10 },
        )
        .await
        .unwrap();
        assert_eq!(page["objects"], json!([]));
        assert_eq!(page["next_cursor"], "opaque+/=");
        let page = run(
            &client,
            &target(),
            Operation::List {
                prefix: "revisions/".into(),
                cursor: Some("opaque+/=".into()),
                page_size: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(page["objects"], json!([{"key":"revisions/abc.json","size":3}]));
        assert!(page["next_cursor"].is_null());
        let requests = task.await.unwrap();
        assert!(requests[0].contains("prefix=foo%2Frevisions%2F"));
        assert!(requests[1].contains("continuation-token=opaque%2B%2F%3D"));
    }

    #[tokio::test]
    async fn list_cannot_report_unverified_completion_or_foreign_keys() {
        for xml in [
            "<ListBucketResult></ListBucketResult>",
            "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>",
            "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>foobar/abc</Key><Size>3</Size></Contents></ListBucketResult>",
        ] {
            let (client, task) = server(vec![reply(200, xml)]).await;
            let error = run(
                &client,
                &target(),
                Operation::List { prefix: String::new(), cursor: None, page_size: 10 },
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, "integrity");
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn service_errors_preserve_missing_and_denied_distinctions() {
        for (status, code, expected) in [
            (404, "NoSuchBucket", "target_missing"),
            (404, "NoSuchKey", "object_missing"),
            (403, "AccessDenied", "permission"),
            (403, "SignatureDoesNotMatch", "authentication"),
            (404, "", "unavailable"),
            (429, "SlowDown", "transient"),
            (409, "ConditionalRequestConflict", "transient"),
        ] {
            let body = format!(
                "<Error><Code>{code}</Code><Message>private-service-detail</Message></Error>"
            );
            let (client, task) = server(vec![reply(status, &body)]).await;
            let error = run(&client, &target(), Operation::Probe).await.unwrap_err();
            assert_eq!(error.code, expected);
            assert!(!error.message.contains("private-service-detail"));
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn download_only_replaces_output_after_a_complete_bounded_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("record");
        std::fs::write(&output, "previous").unwrap();
        for response in [
            reply(200, "too-large"),
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nx".into(),
        ] {
            let (client, task) = server(vec![response]).await;
            assert!(
                run(
                    &client,
                    &target(),
                    Operation::Get {
                        key: "record".into(),
                        output_path: output.clone(),
                        max_bytes: 3
                    }
                )
                .await
                .is_err()
            );
            assert_eq!(std::fs::read(&output).unwrap(), b"previous");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
            task.await.unwrap();
        }
        let (client, task) = server(vec![reply(200, "new")]).await;
        assert_eq!(
            run(
                &client,
                &target(),
                Operation::Get { key: "record".into(), output_path: output.clone(), max_bytes: 3 }
            )
            .await
            .unwrap(),
            json!({"size":3})
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"new");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn immutable_put_verifies_actual_existing_bytes_after_precondition_failure() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("record");
        std::fs::write(&input, "content").unwrap();
        let sha256 = format!("{:x}", Sha256::digest(b"content"));
        let precondition = reply(412, "<Error><Code>PreconditionFailed</Code></Error>");
        for (existing, expected) in [("content", None), ("changed", Some("conflict"))] {
            let (client, task) = server(vec![precondition.clone(), reply(200, existing)]).await;
            let result = run(
                &client,
                &target(),
                Operation::Put {
                    key: format!("revisions/{sha256}"),
                    input_path: input.clone(),
                    size: 7,
                    sha256: sha256.clone(),
                },
            )
            .await;
            if let Some(expected) = expected {
                assert_eq!(result.unwrap_err().code, expected);
            } else {
                assert_eq!(result.unwrap()["sha256"], sha256);
            }
            let requests = task.await.unwrap();
            assert!(requests[0].to_ascii_lowercase().contains("if-none-match: *\r\n"));
            assert!(requests[0].ends_with("content"));
            assert!(requests[1].starts_with("GET "));
        }
    }

    #[tokio::test]
    async fn invalid_upload_bytes_never_reach_the_service() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("record");
        std::fs::write(&input, "content").unwrap();
        let (client, task) = server(vec![]).await;
        let error = run(
            &client,
            &target(),
            Operation::Put {
                key: "record".into(),
                input_path: input,
                size: 7,
                sha256: "0".repeat(64),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "integrity");
        assert!(task.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn lost_put_response_retries_identical_bytes_and_verifies_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("record");
        std::fs::write(&input, "content").unwrap();
        let sha256 = format!("{:x}", Sha256::digest(b"content"));
        let (client, task) = server(vec![
            String::new(),
            reply(412, "<Error><Code>PreconditionFailed</Code></Error>"),
            reply(200, "content"),
        ])
        .await;
        let client = Client::from_conf(
            client
                .config()
                .to_builder()
                .retry_config(RetryConfig::standard().with_max_attempts(3))
                .build(),
        );
        let result = run(
            &client,
            &target(),
            Operation::Put {
                key: "record".into(),
                input_path: input,
                size: 7,
                sha256: sha256.clone(),
            },
        )
        .await
        .unwrap();
        assert_eq!(result["sha256"], sha256);
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 3);
        for request in &requests[..2] {
            assert!(request.starts_with("PUT "));
            assert!(request.ends_with("content"));
            assert!(request.to_ascii_lowercase().contains("if-none-match: *\r\n"));
        }
        assert!(requests[2].starts_with("GET "));
    }

    #[tokio::test]
    async fn concurrent_identical_puts_converge_after_one_conditional_create() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("record");
        std::fs::write(&input, "content").unwrap();
        let sha256 = format!("{:x}", Sha256::digest(b"content"));
        let (client, task) = server(vec![
            reply(200, ""),
            reply(412, "<Error><Code>PreconditionFailed</Code></Error>"),
            reply(200, "content"),
        ])
        .await;
        let first = Operation::Put {
            key: "record".into(),
            input_path: input.clone(),
            size: 7,
            sha256: sha256.clone(),
        };
        let second = Operation::Put {
            key: "record".into(),
            input_path: input,
            size: 7,
            sha256: sha256.clone(),
        };
        let target = target();
        let (first, second) =
            tokio::join!(run(&client, &target, first), run(&client, &target, second));
        assert_eq!(first.unwrap()["sha256"], sha256);
        assert_eq!(second.unwrap()["sha256"], sha256);
        let requests = task.await.unwrap();
        assert_eq!(requests.iter().filter(|r| r.starts_with("PUT ")).count(), 2);
        assert_eq!(requests.iter().filter(|r| r.starts_with("GET ")).count(), 1);
    }

    #[tokio::test]
    async fn interrupted_body_obeys_the_deadline_without_publishing_output() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("record");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(&format!("http://{}", listener.local_addr().unwrap()));
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nx").await.unwrap();
            std::future::pending::<()>().await;
        });
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            run(
                &client,
                &target(),
                Operation::Get { key: "record".into(), output_path: output.clone(), max_bytes: 3 },
            ),
        )
        .await;
        assert!(result.is_err());
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
}
