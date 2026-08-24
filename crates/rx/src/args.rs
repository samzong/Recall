use std::path::Path;

use anyhow::{Result, bail};

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
    pub provider: Option<String>,
    pub passthrough: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProvidersCommand {
    Help,
    List,
    Login { provider: Option<String> },
    Logout { provider: Option<String> },
    Use { provider: Option<String> },
    Models(ModelsCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelsCommand {
    Help,
    Update { provider: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Help,
    Version,
    Launch(LaunchRequest),
    PickHarness { provider: Option<String> },
    Providers(ProvidersCommand),
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
    let (provider, rest) = extract_provider(rest)?;
    match rest.first().map(String::as_str) {
        None => Ok(Command::PickHarness { provider }),
        Some("-h" | "--help") => Ok(Command::Help),
        Some("-V" | "--version") => Ok(Command::Version),
        Some("providers") => {
            if provider.is_some() {
                bail!("--provider is not valid with rx providers");
            }
            Ok(Command::Providers(parse_providers(&rest[1..])?))
        }
        Some("update") => {
            if provider.is_some() {
                bail!("--provider is not valid with rx update");
            }
            Ok(Command::Update { yes: parse_update_args(&rest[1..])? })
        }
        Some(name) => {
            let Some(harness) = Harness::parse(name) else {
                bail!("unknown harness: {name}\n\n{}", crate::help_text().trim_end());
            };
            Ok(Command::Launch(LaunchRequest {
                harness,
                provider,
                passthrough: rest[1..].to_vec(),
            }))
        }
    }
}

fn parse_providers(args: &[String]) -> Result<ProvidersCommand> {
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") if args.len() <= 1 => Ok(ProvidersCommand::Help),
        Some("list") if args.len() == 1 => Ok(ProvidersCommand::List),
        Some("models") => Ok(ProvidersCommand::Models(parse_models(&args[1..])?)),
        Some(command @ ("login" | "logout" | "use")) => {
            let provider = parse_provider_argument(command, &args[1..])?;
            Ok(match command {
                "login" => ProvidersCommand::Login { provider },
                "logout" => ProvidersCommand::Logout { provider },
                "use" => ProvidersCommand::Use { provider },
                _ => unreachable!(),
            })
        }
        Some(command) => {
            bail!("unknown providers command: {command}\n\n{}", crate::providers::help())
        }
        None => unreachable!(),
    }
}

fn parse_models(args: &[String]) -> Result<ModelsCommand> {
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") if args.len() <= 1 => Ok(ModelsCommand::Help),
        Some("update") => Ok(ModelsCommand::Update {
            provider: parse_provider_argument("models update", &args[1..])?,
        }),
        Some(command) => {
            bail!(
                "unknown providers models command: {command}\n\n{}",
                crate::providers::models_help()
            )
        }
        None => unreachable!(),
    }
}

fn parse_provider_argument(command: &str, args: &[String]) -> Result<Option<String>> {
    if args.len() > 1 {
        bail!("usage: rx providers {command} [provider]");
    }
    Ok(args.first().cloned())
}

fn extract_provider(args: &[String]) -> Result<(Option<String>, Vec<String>)> {
    let mut provider = None;
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
        if !raw && arg == "--provider" {
            let value =
                args.get(i + 1).ok_or_else(|| anyhow::anyhow!("--provider requires a value"))?;
            provider = Some(value.clone());
            i += 2;
            continue;
        }
        if !raw && let Some(value) = arg.strip_prefix("--provider=") {
            if value.is_empty() {
                bail!("--provider requires a value");
            }
            provider = Some(value.to_string());
            i += 1;
            continue;
        }
        rest.push(arg.clone());
        i += 1;
    }
    Ok((provider, rest))
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
