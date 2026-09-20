mod catalog_cache;
mod claude_catalog;
mod cli;
mod dsh;
mod install_update;
mod launch_routes;
mod pi;
mod provider_config;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use crate::args::{
    Command, CompletionShell, CompletionsCommand, Harness, LaunchRequest, ProviderIdFilter,
    ProvidersCommand, UpdateCommand, parse, rewrite_argv0,
};
use serde_json::{Value, json};

use crate::config::{self, Paths};
use crate::launch::{self, EnvLookup};
use crate::{catalog, claude_catalog as claude, provider};

fn os(argv: &[&str]) -> Vec<OsString> {
    argv.iter().map(|arg| OsString::from(*arg)).collect()
}

fn arg_str(arg: &OsString) -> &str {
    arg.to_str().expect("test args are utf-8")
}

fn parse_line(argv: &[&str]) -> Command {
    parse(&rewrite_argv0(os(argv))).unwrap()
}

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path().to_path_buf());
    (dir, paths)
}

fn accept_connection(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("test server did not receive a connection: {error}"),
        }
    }
}

fn serve_openai_models(body: &str) -> (String, thread::JoinHandle<()>) {
    serve_openai_models_times(body, 1)
}

fn serve_openai_models_times(body: &str, times: usize) -> (String, thread::JoinHandle<()>) {
    serve_openai(&vec![(200, body); times])
}

fn serve_openai_error(status: u16) -> (String, thread::JoinHandle<()>) {
    serve_openai(&[(status, "fail")])
}

fn serve_openai_models_then_error(body: &str, status: u16) -> (String, thread::JoinHandle<()>) {
    serve_openai(&[(200, body), (status, "fail")])
}

fn serve_openai(responses: &[(u16, &str)]) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let responses: Vec<_> =
        responses.iter().map(|(status, body)| (*status, body.to_string())).collect();
    let server = thread::spawn(move || {
        for (status, body) in responses {
            let mut stream = accept_connection(&listener);
            let mut request = [0; 2048];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("GET /v1/models "), "{request}");
            write!(stream,
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ).unwrap();
        }
    });
    (base_url, server)
}

fn request(harness: Harness, provider: Option<&str>, argv: &[&str]) -> LaunchRequest {
    LaunchRequest { harness, provider: provider.map(str::to_string), passthrough: os(argv) }
}

fn fixture_config(base_url: &str) -> String {
    format!("[provider.acme]\nbase_url = \"{base_url}\"\nenv = \"ACME_API_KEY\"\nauth = \"env\"\n")
}

pub(crate) fn fixture_provider(base_url: &str) -> provider::Provider {
    provider::resolve(
        "acme",
        Some(&config::ProviderConfig {
            base_url: Some(base_url.to_string()),
            env: Some("ACME_API_KEY".to_string()),
            ..config::ProviderConfig::default()
        }),
    )
    .unwrap()
}

fn isolated(values: &[(&str, &str)]) -> EnvLookup {
    EnvLookup::isolated(
        values.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect(),
    )
}

fn read_json<T: serde::de::DeserializeOwned>(path: impl AsRef<Path>) -> T {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn write_json(path: impl AsRef<Path>, value: &impl serde::Serialize) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn model(id: &str, name: &str, context: i64, prices: Option<(&str, &str)>) -> claude::UserModel {
    claude::UserModel {
        id: id.to_string(),
        name: Some(name.to_string()),
        context_length: Some(context),
        canonical_slug: None,
        pricing: prices.map(|(prompt, completion)| claude::Pricing {
            prompt: Some(prompt.to_string()),
            completion: Some(completion.to_string()),
            input_cache_read: None,
            input_cache_write: None,
            web_search: None,
        }),
    }
}

fn assert_env(plan: &launch::LaunchPlan, expected: &[(&str, &str)]) {
    for (key, value) in expected {
        assert!(
            plan.env_set.iter().any(|(k, v)| k == key && v == value),
            "missing environment variable {key}"
        );
    }
}

fn entries<'a>(document: &'a Value, field: &str) -> &'a [Value] {
    document[field].as_array().unwrap()
}
