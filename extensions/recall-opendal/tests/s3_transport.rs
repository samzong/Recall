//! Local HTTP protocol fixtures, not an S3 service or MinIO certification.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
}

#[derive(Default)]
struct State {
    object: Option<Vec<u8>>,
    parts: BTreeMap<usize, Vec<u8>>,
    requests: Vec<Request>,
    invalid_lookahead: bool,
    list_failure: Option<ListFailure>,
}

#[derive(Clone, Copy)]
enum ListFailure {
    Empty404,
}

struct Fixture {
    endpoint: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Fixture {
    fn new(object: Option<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State { object, ..State::default() }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !thread_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
                        let Some((request, body)) = read_request(&mut stream) else { continue };
                        let mut state = thread_state.lock().unwrap();
                        state.requests.push(request.clone());
                        respond(&mut stream, &request, body, &mut state);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            }
        });
        Self { endpoint, state, stop, thread: Some(thread) }
    }

    fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let joined = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            joined.unwrap();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(Request, Vec<u8>)> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let end = loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) if bytes.is_empty() => return None,
            Ok(count) => count,
            Err(error)
                if bytes.is_empty()
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
            {
                return None;
            }
            Err(error) => panic!("fixture read: {error}"),
        };
        assert!(count > 0, "fixture request ended before headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(offset) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            break offset + 4;
        }
        assert!(bytes.len() < 64 * 1024, "fixture request headers too large");
    };
    let head = std::str::from_utf8(&bytes[..end]).unwrap();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let target = first.next().unwrap().to_owned();
    let headers: BTreeMap<_, _> = lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (key, value) = line.split_once(':').unwrap();
            (key.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    assert!(!headers.contains_key("transfer-encoding"), "fixture requires Content-Length");
    let size = headers.get("content-length").map(|s| s.parse::<usize>().unwrap()).unwrap_or(0);
    if headers.get("expect").is_some_and(|s| s.eq_ignore_ascii_case("100-continue")) {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
    }
    let mut body = bytes[end..].to_vec();
    while body.len() < size {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "fixture request ended before body");
        body.extend_from_slice(&buffer[..count]);
    }
    assert_eq!(body.len(), size);
    let mut headers = headers;
    headers.remove("authorization");
    Some((Request { method, target, headers }, body))
}

fn reply(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, String)],
    body: &[u8],
    head: bool,
) {
    write!(stream, "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len())
        .unwrap();
    for (key, value) in headers {
        write!(stream, "{key}: {value}\r\n").unwrap();
    }
    stream.write_all(b"\r\n").unwrap();
    if !head {
        stream.write_all(body).unwrap();
    }
}

fn query(request: &Request) -> BTreeMap<String, String> {
    url::Url::parse(&format!("http://fixture{}", request.target))
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

fn respond(stream: &mut TcpStream, request: &Request, body: Vec<u8>, state: &mut State) {
    let query = query(request);
    if query.get("list-type").is_some_and(|s| s == "2") {
        if let Some(failure) = state.list_failure {
            let body: &[u8] = match failure {
                ListFailure::Empty404 => b"",
            };
            reply(stream, "404 Not Found", &[], body, false);
            return;
        }
        let (keys, truncated) = if query.contains_key("continuation-token") {
            (vec!["v1/d", "v1/e"], false)
        } else if query.get("start-after").is_some_and(|s| s == "v1/b") {
            (vec!["v1/c", "v1/d", "v1/e"], false)
        } else {
            (vec!["v1/a", "v1/b", if state.invalid_lookahead { "v1/c~" } else { "v1/c" }], true)
        };
        let contents: String = keys.into_iter().map(|key| format!(
            "<Contents><Key>{key}</Key><Size>4</Size><LastModified>2026-01-01T00:00:00Z</LastModified><ETag>\"opaque\"</ETag></Contents>"
        )).collect();
        let xml = format!(
            "<ListBucketResult><IsTruncated>{truncated}</IsTruncated>{contents}{}\
            </ListBucketResult>",
            if truncated {
                "<NextContinuationToken>second-page</NextContinuationToken>"
            } else {
                ""
            }
        );
        reply(stream, "200 OK", &[], xml.as_bytes(), false);
    } else if request.method == "POST" && query.contains_key("uploads") {
        state.parts.clear();
        reply(stream, "200 OK", &[], b"<InitiateMultipartUploadResult><UploadId>fixture-upload</UploadId></InitiateMultipartUploadResult>", false);
    } else if request.method == "PUT" && query.contains_key("partNumber") {
        state.parts.insert(query["partNumber"].parse().unwrap(), body);
        reply(stream, "200 OK", &[("ETag", "\"opaque-part\"".into())], b"", false);
    } else if request.method == "PUT"
        || (request.method == "POST" && query.contains_key("uploadId"))
    {
        if state.object.is_some() && request.headers.get("if-none-match").is_some_and(|s| s == "*")
        {
            // An opaque, deliberately unhelpful ETag must never substitute for reading bytes.
            reply(
                stream,
                "412 Precondition Failed",
                &[("ETag", "\"opaque\"".into())],
                b"<Error><Code>PreconditionFailed</Code><Message>exists</Message></Error>",
                false,
            );
        } else {
            state.object = Some(if request.method == "POST" {
                state.parts.values().flatten().copied().collect()
            } else {
                body
            });
            let response: &[u8] = if request.method == "POST" {
                b"<CompleteMultipartUploadResult><ETag>\"opaque\"</ETag></CompleteMultipartUploadResult>"
            } else {
                b""
            };
            reply(stream, "200 OK", &[("ETag", "\"opaque\"".into())], response, false);
        }
    } else if request.method == "HEAD" || request.method == "GET" {
        if let Some(object) = &state.object {
            let mut headers = vec![
                ("ETag", "\"opaque\"".into()),
                ("Last-Modified", "Thu, 01 Jan 2026 00:00:00 GMT".into()),
            ];
            if let Some(range) = request.headers.get("range") {
                let (start, end) = range.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
                let start: usize = start.parse().unwrap();
                let end: usize =
                    if end.is_empty() { object.len() - 1 } else { end.parse().unwrap() };
                headers.push(("Content-Range", format!("bytes {start}-{end}/{}", object.len())));
                reply(stream, "206 Partial Content", &headers, &object[start..=end], false);
            } else {
                reply(stream, "200 OK", &headers, object, request.method == "HEAD");
            }
        } else {
            reply(stream, "404 Not Found", &[], b"", request.method == "HEAD");
        }
    } else if request.method == "PROPFIND" {
        reply(stream, "404 Not Found", &[], b"", false);
    } else if request.method == "MKCOL" {
        reply(stream, "201 Created", &[], b"", false);
    } else if request.method == "DELETE" {
        reply(stream, "204 No Content", &[], b"", false);
    } else {
        panic!("unexpected fixture request: {request:?}");
    }
}

struct Provider {
    temp: tempfile::TempDir,
    config: std::path::PathBuf,
}

impl Provider {
    fn new(fixture: &Fixture, scheme: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("provider.json");
        let value = match scheme {
            "s3" => {
                json!({"scheme":"s3", "bucket":"fixture-bucket", "region":"us-east-1", "endpoint":fixture.endpoint, "disable_ec2_metadata":"true"})
            }
            "webdav" => json!({"scheme":"webdav", "endpoint":fixture.endpoint}),
            _ => panic!("unsupported fixture scheme"),
        };
        std::fs::write(&config, value.to_string()).unwrap();
        Self { temp, config }
    }

    fn command(&self) -> Command {
        let binary = std::env::var_os("RECALL_OPENDAL_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_recall-opendal").into());
        let mut command = Command::new(binary);
        // All environment changes are child-only; no host SDK profiles or proxies.
        command
            .env_clear()
            .env("RECALL_OPENDAL_CONFIG", &self.config)
            .env("HOME", self.temp.path())
            .env("AWS_SHARED_CREDENTIALS_FILE", self.temp.path().join("no-credentials"))
            .env("AWS_CONFIG_FILE", self.temp.path().join("no-aws-config"))
            .env("AWS_ACCESS_KEY_ID", "fixture-access")
            .env("AWS_SECRET_ACCESS_KEY", "fixture-secret")
            .env("AWS_EC2_METADATA_DISABLED", "true");
        command
    }

    fn request(&self, mut value: Value) -> Value {
        value["transport_version"] = json!(1);
        value["timeout_ms"] = json!(10_000);
        let mut child = self
            .command()
            .arg("--recall-remote-transport")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&serde_json::to_vec(&value).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "transport exceeded fixture deadline: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        let response: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{error}: stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(response["transport_version"], 1);
        assert_eq!(output.status.success(), response.get("result").is_some(), "{response}");
        response
    }

    fn put(&self, bytes: &[u8]) -> Value {
        let source = self.temp.path().join("source");
        std::fs::write(&source, bytes).unwrap();
        self.request(json!({"operation":"put", "key":"v1/object", "input_path":source,
            "size":bytes.len(), "sha256":format!("{:x}", Sha256::digest(bytes))}))
    }
}

fn assert_conditional_publication(requests: &[Request], multipart: bool) {
    let publications: Vec<_> = requests
        .iter()
        .filter(|r| {
            (r.method == "PUT" && !query(r).contains_key("partNumber"))
                || (r.method == "POST" && query(r).contains_key("uploadId"))
        })
        .collect();
    assert_eq!(publications.len(), 1, "{requests:?}");
    assert_eq!(
        publications[0].headers.get("if-none-match").map(String::as_str),
        Some("*"),
        "{requests:?}"
    );
    assert_eq!(publications[0].method, if multipart { "POST" } else { "PUT" });
    if multipart {
        assert!(
            requests
                .iter()
                .filter(|r| r.method == "PUT" && query(r).contains_key("partNumber"))
                .count()
                >= 2,
            "{requests:?}"
        );
    }
}

fn immutable_put(bytes: Vec<u8>, existing: Option<Vec<u8>>, multipart: bool) {
    let fixture = Fixture::new(existing.clone());
    let provider = Provider::new(&fixture, "s3");
    let response = provider.put(&bytes);
    let expected = existing.as_ref().map_or(&bytes, |existing| existing);
    let requests = fixture.requests();
    assert_conditional_publication(&requests, multipart);
    assert_eq!(
        fixture.state.lock().unwrap().object.as_ref().unwrap(),
        expected,
        "immutable object was overwritten"
    );
    if existing.as_ref().is_some_and(|existing| existing != &bytes) {
        assert_eq!(response["error"]["code"], "conflict", "{response}");
    } else {
        assert_eq!(
            response["result"],
            json!({"size":bytes.len(), "sha256":format!("{:x}", Sha256::digest(&bytes))}),
            "{response}"
        );
    }
    assert_eq!(
        requests.iter().filter(|r| r.method == "GET").count(),
        usize::from(existing.is_some()),
        "existing bytes must be read after a conditional collision: {requests:?}"
    );
    if existing.is_some() {
        let publish = requests
            .iter()
            .position(|r| r.headers.get("if-none-match").is_some_and(|s| s == "*"))
            .unwrap();
        let get = requests.iter().position(|r| r.method == "GET").unwrap();
        assert!(get > publish, "verification must follow the actual collision");
    }
}

#[test]
fn s3_absent_object_uses_conditional_single_put() {
    immutable_put(b"new-object".to_vec(), None, false);
}

#[test]
fn s3_existing_identical_object_is_read_and_hashed_after_collision() {
    let bytes = b"same-object".to_vec();
    immutable_put(bytes.clone(), Some(bytes), false);
}

#[test]
fn s3_same_length_different_object_conflicts_without_overwrite() {
    immutable_put(b"new-object".to_vec(), Some(b"old-object".to_vec()), false);
}

#[test]
fn s3_multipart_completion_is_conditional_and_collision_reads_bytes() {
    // Above both the provider's 4 MiB chunk and S3's 5 MiB minimum part size.
    let bytes: Vec<u8> = (0..12 * 1024 * 1024 + 913).map(|i| (i % 251) as u8).collect();
    immutable_put(bytes.clone(), None, true);
    immutable_put(bytes.clone(), Some(bytes.clone()), true);
    let mut different = bytes.clone();
    different[bytes.len() / 2] ^= 1;
    immutable_put(bytes, Some(different), true);
}

#[test]
fn s3_listing_stops_after_one_page_and_passes_cursor_as_start_after() {
    let fixture = Fixture::new(None);
    let provider = Provider::new(&fixture, "s3");
    let first = provider.request(json!({"operation":"list", "prefix":"v1/", "page_size":2}));
    assert_eq!(
        first["result"],
        json!({"objects":[{"key":"v1/a","size":4},{"key":"v1/b","size":4}],"next_cursor":"v1/b"})
    );
    let requests = fixture.requests();
    assert_eq!(
        requests.len(),
        1,
        "lookahead must not fetch the available second S3 page: {requests:?}"
    );
    assert_eq!(query(&requests[0]).get("max-keys").map(String::as_str), Some("3"));
    assert!(!query(&requests[0]).contains_key("start-after"));
    let second = provider.request(json!({"operation":"list", "prefix":"v1/", "page_size":2,"cursor":first["result"]["next_cursor"]}));
    assert_eq!(
        second["result"],
        json!({"objects":[{"key":"v1/c","size":4},{"key":"v1/d","size":4}],"next_cursor":"v1/d"})
    );
    let requests = fixture.requests();
    assert_eq!(
        requests.len(),
        2,
        "each transport page must make only one list request: {requests:?}"
    );
    assert_eq!(query(&requests[1]).get("start-after").map(String::as_str), Some("v1/b"));
    assert!(!query(&requests[1]).contains_key("continuation-token"));
}

#[test]
fn s3_invalid_lookahead_key_fails_integrity_before_returning_page() {
    let fixture = Fixture::new(None);
    fixture.state.lock().unwrap().invalid_lookahead = true;
    let provider = Provider::new(&fixture, "s3");
    let response = provider.request(json!({"operation":"list", "prefix":"v1/", "page_size":2}));
    assert_eq!(response["error"]["code"], "integrity", "{response}");
    assert_eq!(fixture.requests().len(), 1);
}

#[test]
fn unsupported_conditional_backend_fails_before_any_http_publication() {
    let fixture = Fixture::new(None);
    let provider = Provider::new(&fixture, "webdav");
    let response = provider.put(b"must-not-publish");
    assert_eq!(response["error"]["code"], "unavailable", "{response}");
    assert!(fixture.requests().is_empty(), "unsupported conditional backend must fail closed");
    assert!(fixture.state.lock().unwrap().object.is_none());
}

fn assert_one_bounded_probe_request(fixture: &Fixture) {
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].method, "GET");
    let query = query(&requests[0]);
    assert_eq!(query.get("list-type").map(String::as_str), Some("2"));
    assert_eq!(
        query.get("max-keys").map(String::as_str),
        Some("1"),
        "probe must request only one entry: {requests:?}"
    );
}

fn rejected_probe(failure: ListFailure) {
    let fixture = Fixture::new(None);
    fixture.state.lock().unwrap().list_failure = Some(failure);
    let provider = Provider::new(&fixture, "s3");
    let response = provider.request(json!({"operation":"probe"}));
    assert!(response.get("error").is_some(), "failed list must never report readable: {response}");
    assert_one_bounded_probe_request(&fixture);
}

fn rejected_configuration(failure: ListFailure) {
    let fixture = Fixture::new(None);
    fixture.state.lock().unwrap().list_failure = Some(failure);
    let provider = Provider::new(&fixture, "s3");
    let original = std::fs::read(&provider.config).unwrap();
    let mut candidate: Value = serde_json::from_slice(&original).unwrap();
    candidate["root"] = json!("replacement-root/");
    let output = provider
        .command()
        .args(["--recall-remote-configure", "--config-json", &candidate.to_string()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "failed destination list must reject configuration: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert_eq!(
        std::fs::read(&provider.config).unwrap(),
        original,
        "failed verification must preserve previous config bytes"
    );
    assert_one_bounded_probe_request(&fixture);
}

#[test]
fn s3_missing_bucket_empty_404_probe_fails_with_bounded_list() {
    rejected_probe(ListFailure::Empty404);
}

#[test]
fn s3_missing_bucket_empty_404_configuration_preserves_previous_config() {
    rejected_configuration(ListFailure::Empty404);
}
