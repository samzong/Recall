mod config;
mod error;
mod protocol;
mod storage;

use error::{Failure, Result};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--recall-extension-manifest") if args.len() == 1 => {
            println!(
                "{}",
                json!({"name":"opendal", "version":env!("CARGO_PKG_VERSION"),
                "protocol":2,"min_recall":"0.6.0",
                "description":"Remote storage through Apache OpenDAL"})
            );
            ExitCode::SUCCESS
        }
        Some("--recall-remote-configure") => match config::configure(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        Some("--recall-remote-transport") => {
            let result = if args.len() == 1 {
                transport()
            } else {
                Err(Failure::new("invalid_request", "transport takes no arguments"))
            };
            let success = result.is_ok();
            let response = match result {
                Ok(result) => json!({"transport_version":1,"result":result}),
                Err(error) => json!({"transport_version":1,"error":error}),
            };
            if let Err(error) = emit(response) {
                eprintln!("cannot write transport response: {error}");
                return ExitCode::FAILURE;
            }
            if success { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        _ => {
            eprintln!(
                "usage: recall-opendal --recall-extension-manifest | --recall-remote-configure [--config-json JSON | --config-file PATH] | --recall-remote-transport"
            );
            ExitCode::from(64)
        }
    }
}

fn emit(response: Value) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(&response)?;
    anyhow::ensure!(bytes.len() <= protocol::CONTROL_LIMIT, "response exceeds control limit");
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn transport() -> Result<Value> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(protocol::CONTROL_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(Failure::io)?;
    if bytes.len() > protocol::CONTROL_LIMIT {
        return Err(Failure::new("invalid_request", "request exceeds 2 MiB control limit"));
    }
    let request = protocol::parse(&bytes)?;
    let timeout = Duration::from_millis(request.timeout_ms);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = storage::with_operator(|| {
            let operator = config::load()?.operator()?;
            storage::execute(&operator, request.operation)
        });
        let _ = sender.send(result);
    });
    let result = receiver.recv_timeout(timeout).map_err(|error| match error {
        std::sync::mpsc::RecvTimeoutError::Timeout => Failure::new(
            "transient",
            "provider deadline expired; a remote write may already have completed",
        ),
        std::sync::mpsc::RecvTimeoutError::Disconnected => {
            Failure::new("unavailable", "provider worker stopped")
        }
    })??;
    if serde_json::to_vec(&json!({"transport_version":1,"result":result}))
        .map_err(|e| Failure::new("unavailable", e.to_string()))?
        .len()
        > protocol::CONTROL_LIMIT
    {
        return Err(Failure::new("invalid_request", "response exceeds 2 MiB control limit"));
    }
    Ok(result)
}
