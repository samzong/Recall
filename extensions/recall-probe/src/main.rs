use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MIN_RECALL: &str = "0.2.10";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--recall-extension-manifest") => {
            println!(
                "{{\"name\":\"probe\",\"version\":\"{VERSION}\",\"protocol\":1,\"min_recall\":\"{MIN_RECALL}\",\"commands\":[\"probe\"]}}"
            );
            ExitCode::SUCCESS
        }
        Some("-h" | "--help") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("-V" | "--version") => {
            println!("recall-probe {VERSION}");
            ExitCode::SUCCESS
        }
        Some(arg) => {
            eprintln!("unexpected argument: {arg}");
            ExitCode::from(2)
        }
        None => {
            println!("Recall extension host probe OK");
            ExitCode::SUCCESS
        }
    }
}

fn print_help() {
    println!(
        "Official Recall extension host probe\n\nUsage: recall probe [OPTIONS]\n\nOptions:\n      --recall-extension-manifest  Print Recall extension manifest JSON\n  -h, --help                       Print help\n  -V, --version                    Print version"
    );
}
