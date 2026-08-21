use std::path::Path;

use anyhow::{Result, bail};

use crate::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Harness {
    Claude,
    Codex,
    OpenCode,
    Pi,
}

impl Harness {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            "pi" => Some(Self::Pi),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchRequest {
    pub harness: Harness,
    pub gateway: Option<String>,
    pub passthrough: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigCommand {
    SetGateway { name: String },
    SetKey { provider: String, key: String },
    Get { name: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Help,
    Version,
    Launch(LaunchRequest),
    Config(ConfigCommand),
    Debug { subcommand: debug::Subcommand, gateway: Option<String> },
    Update { yes: bool },
}

pub(crate) fn rewrite_argv0(mut args: Vec<String>) -> Vec<String> {
    let Some(argv0) = args.first() else {
        return args;
    };
    let Some(harness) = argv0_harness(argv0) else {
        return args;
    };
    args.insert(1, harness.to_string());
    args
}

/// Harness implied by an invoked alias such as `rxc`; `None` for plain `rx`.
pub(crate) fn argv0_harness(argv0: &str) -> Option<&'static str> {
    let name = Path::new(argv0).file_stem().and_then(|stem| stem.to_str()).unwrap_or("");
    match name {
        "rxc" => Some("claude"),
        "rxx" => Some("codex"),
        "rxo" => Some("opencode"),
        "rxp" => Some("pi"),
        _ => None,
    }
}

pub(crate) fn parse(args: &[String]) -> Result<Command> {
    let rest = args.get(1..).unwrap_or(&[]);
    let (gateway, rest) = extract_gateway(rest)?;
    match rest.first().map(String::as_str) {
        None => bail!("missing harness name\n\n{}", crate::help_text().trim_end()),
        Some("-h" | "--help") => Ok(Command::Help),
        Some("-V" | "--version") => Ok(Command::Version),
        Some("config") => {
            if gateway.is_some() {
                bail!("--gateway is not valid with rx config");
            }
            Ok(Command::Config(parse_config(&rest[1..])?))
        }
        Some("debug") => Ok(Command::Debug { subcommand: debug::parse(&rest[1..])?, gateway }),
        Some("update") => {
            if gateway.is_some() {
                bail!("--gateway is not valid with rx update");
            }
            Ok(Command::Update { yes: parse_update_args(&rest[1..])? })
        }
        Some(name) => {
            let Some(harness) = Harness::parse(name) else {
                bail!("unknown harness: {name}\n\n{}", crate::help_text().trim_end());
            };
            Ok(Command::Launch(LaunchRequest { harness, gateway, passthrough: rest[1..].to_vec() }))
        }
    }
}

fn parse_config(args: &[String]) -> Result<ConfigCommand> {
    match args.first().map(String::as_str) {
        Some("set") => match args.get(1).map(String::as_str) {
            Some("gateway") => {
                let name = args.get(2).ok_or_else(|| {
                    anyhow::anyhow!("usage: rx config set gateway <openrouter|tokener>")
                })?;
                if args.len() != 3 {
                    bail!("usage: rx config set gateway <openrouter|tokener>");
                }
                Ok(ConfigCommand::SetGateway { name: name.to_string() })
            }
            Some("key") => {
                let provider = args.get(2).ok_or_else(|| {
                    anyhow::anyhow!("usage: rx config set key <openrouter|tokener> <key>")
                })?;
                let key = args.get(3).ok_or_else(|| {
                    anyhow::anyhow!("usage: rx config set key <openrouter|tokener> <key>")
                })?;
                if args.len() != 4 {
                    bail!("usage: rx config set key <openrouter|tokener> <key>");
                }
                Ok(ConfigCommand::SetKey { provider: provider.to_string(), key: key.to_string() })
            }
            _ => bail!("usage: rx config set <gateway|key> ..."),
        },
        Some("get") => {
            if args.len() > 2 {
                bail!("usage: rx config get [name]");
            }
            Ok(ConfigCommand::Get { name: args.get(1).cloned() })
        }
        _ => bail!("usage: rx config <set|get> ..."),
    }
}

fn extract_gateway(args: &[String]) -> Result<(Option<String>, Vec<String>)> {
    let mut gateway = None;
    let mut rest = Vec::new();
    let mut i = 0;
    let mut raw = false;
    while i < args.len() {
        let arg = &args[i];
        if !raw && arg == "--" {
            raw = true;
            rest.push(arg.clone());
            i += 1;
            continue;
        }
        if !raw && arg == "--gateway" {
            let value =
                args.get(i + 1).ok_or_else(|| anyhow::anyhow!("--gateway requires a value"))?;
            gateway = Some(value.clone());
            i += 2;
            continue;
        }
        if !raw && let Some(value) = arg.strip_prefix("--gateway=") {
            if value.is_empty() {
                bail!("--gateway requires a value");
            }
            gateway = Some(value.to_string());
            i += 1;
            continue;
        }
        rest.push(arg.clone());
        i += 1;
    }
    Ok((gateway, rest))
}

fn parse_update_args(args: &[String]) -> Result<bool> {
    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            "-h" | "--help" => {
                bail!(
                    "usage: rx update [--yes]\n\n\
                     Download and install the latest rx from GitHub releases."
                );
            }
            other => bail!("unexpected argument: {other}\n\nusage: rx update [--yes]"),
        }
    }
    Ok(yes)
}
