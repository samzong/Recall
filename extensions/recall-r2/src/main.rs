mod config;
mod protocol;
mod transport;

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use protocol::{Failure, MESSAGE_LIMIT, Request, Result};
use serde_json::Value;

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_default();
    let mode = args.next();
    match mode.as_deref().and_then(|mode| mode.to_str()) {
        Some("--recall-remote-configure") => {
            match config::Configure::try_parse_from(std::iter::once(program).chain(args)) {
                Ok(configure) => match configure.run() {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => {
                        eprintln!("{}", error.message);
                        ExitCode::FAILURE
                    }
                },
                Err(error) => {
                    let _ = error.print();
                    ExitCode::from(2)
                }
            }
        }
        Some("--recall-remote-transport") if args.next().is_none() => {
            let result = run_transport();
            let (bytes, success) = protocol::response(result);
            let mut stdout = std::io::stdout().lock();
            if stdout.write_all(&bytes).and_then(|()| stdout.flush()).is_err() {
                return ExitCode::FAILURE;
            }
            if success { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Some("--recall-extension-manifest") => {
            println!(
                "{}",
                serde_json::json!({
                    "name": "r2",
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol": 2,
                    "min_recall": "0.6.0"
                })
            );
            ExitCode::SUCCESS
        }
        Some("--version" | "-V") => {
            println!("recall-r2 {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        None | Some("--help" | "-h") => {
            println!(
                "Cloudflare R2 transport for Recall\n\nConnect interactively with: recall remote connect\nSynchronize with: recall remote sync\n\nProvider configuration: --endpoint --bucket --prefix --credential-profile\nCredentials come from an existing local AWS credential profile."
            );
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("Unsupported recall-r2 invocation; use recall remote connect.");
            ExitCode::FAILURE
        }
    }
}

fn run_transport() -> Result<Value> {
    let mut bytes = Vec::new();
    std::io::stdin().take(MESSAGE_LIMIT + 1).read_to_end(&mut bytes).map_err(|_| Failure::io())?;
    let request = Request::parse(&bytes)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Failure::io())?;
    let result = runtime.block_on(async {
        let operation = tokio::task::spawn_blocking(move || {
            let target = config::Target::load(&config::path()?)?;
            tokio::runtime::Handle::current()
                .block_on(transport::execute(&target, request.operation))
        });
        tokio::time::timeout(Duration::from_millis(request.timeout_ms), operation)
            .await
            .map_err(|_| {
                Failure::new(
                    "transient",
                    "operation deadline expired; remote write outcome may be unknown",
                )
            })?
            .map_err(|_| Failure::io())?
    });
    std::mem::forget(runtime);
    result
}
