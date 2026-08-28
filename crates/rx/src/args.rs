use std::ffi::{OsStr, OsString};
use std::path::Path;

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Harness {
    Claude,
    Codex,
    OpenCode,
    Pi,
    Dsh,
    Kimi,
}

impl Harness {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::Dsh => "dsh",
            Self::Kimi => "kimi",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            "pi" => Some(Self::Pi),
            "dsh" => Some(Self::Dsh),
            "kimi" => Some(Self::Kimi),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchRequest {
    pub harness: Harness,
    pub provider: Option<String>,
    pub passthrough: Vec<OsString>,
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
pub(crate) enum UpdateCommand {
    Help,
    Run { yes: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderIdFilter {
    All,
    Configured,
    Targets,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompletionsCommand {
    Help,
    Generate { shell: CompletionShell },
    ListProviders(ProviderIdFilter),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Help,
    Version,
    Launch(LaunchRequest),
    PickHarness { provider: Option<String> },
    Providers(ProvidersCommand),
    Update(UpdateCommand),
    Completions(CompletionsCommand),
}

pub(crate) fn rewrite_argv0(mut args: Vec<OsString>) -> Vec<OsString> {
    let Some(argv0) = args.first() else {
        return args;
    };
    let Some(harness) = argv0_harness(argv0) else {
        return args;
    };
    args.insert(1, OsString::from(harness));
    args
}

pub(crate) fn argv0_harness(argv0: impl AsRef<OsStr>) -> Option<&'static str> {
    let name = Path::new(argv0.as_ref()).file_stem().and_then(|stem| stem.to_str()).unwrap_or("");
    match name {
        "rxc" => Some("claude"),
        "rxx" => Some("codex"),
        "rxo" => Some("opencode"),
        "rxp" => Some("pi"),
        "rxd" => Some("dsh"),
        "rxk" => Some("kimi"),
        _ => None,
    }
}

pub(crate) fn os_prefix(arg: impl AsRef<OsStr>, prefix: &str) -> bool {
    arg.as_ref().as_encoded_bytes().starts_with(prefix.as_bytes())
}

pub(crate) fn parse(args: &[OsString]) -> Result<Command> {
    let rest = args.get(1..).unwrap_or(&[]);
    let (provider, rest) = extract_provider(rest)?;
    match rest.first().and_then(|arg| arg.to_str()) {
        None if rest.is_empty() => Ok(Command::PickHarness { provider }),
        None => {
            bail!(
                "unknown harness: {}\n\n{}",
                rest[0].to_string_lossy(),
                crate::help_text().trim_end()
            )
        }
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
            Ok(Command::Update(parse_update(&rest[1..])?))
        }
        Some("completions") => {
            if provider.is_some() {
                bail!("--provider is not valid with rx completions");
            }
            Ok(Command::Completions(parse_completions(&rest[1..])?))
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

fn parse_providers(args: &[OsString]) -> Result<ProvidersCommand> {
    match args.first().and_then(|arg| arg.to_str()) {
        None if args.is_empty() => Ok(ProvidersCommand::Help),
        Some("-h" | "--help" | "help") if args.len() <= 1 => Ok(ProvidersCommand::Help),
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
        None => {
            bail!(
                "unknown providers command: {}\n\n{}",
                args[0].to_string_lossy(),
                crate::providers::help()
            )
        }
    }
}

fn parse_models(args: &[OsString]) -> Result<ModelsCommand> {
    match args.first().and_then(|arg| arg.to_str()) {
        None if args.is_empty() => Ok(ModelsCommand::Help),
        Some("-h" | "--help" | "help") if args.len() <= 1 => Ok(ModelsCommand::Help),
        Some("update") => Ok(ModelsCommand::Update {
            provider: parse_provider_argument("models update", &args[1..])?,
        }),
        Some(command) => {
            bail!(
                "unknown providers models command: {command}\n\n{}",
                crate::providers::models_help()
            )
        }
        None => {
            bail!(
                "unknown providers models command: {}\n\n{}",
                args[0].to_string_lossy(),
                crate::providers::models_help()
            )
        }
    }
}

fn parse_provider_argument(command: &str, args: &[OsString]) -> Result<Option<String>> {
    if args.len() > 1 {
        bail!("usage: rx providers {command} [provider]");
    }
    let Some(arg) = args.first() else {
        return Ok(None);
    };
    let Some(provider) = arg.to_str() else {
        bail!("usage: rx providers {command} [provider]");
    };
    Ok(Some(provider.to_string()))
}

pub(crate) fn before_double_dash(args: &[OsString]) -> &[OsString] {
    match args.iter().position(|arg| arg == "--") {
        Some(index) => &args[..index],
        None => args,
    }
}

fn extract_provider(args: &[OsString]) -> Result<(Option<String>, Vec<OsString>)> {
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
            let Some(value) = value.to_str() else {
                bail!("--provider requires a value");
            };
            provider = Some(value.to_string());
            i += 2;
            continue;
        }
        if !raw && os_prefix(arg, "--provider=") {
            let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--provider=")) else {
                bail!("--provider requires a value");
            };
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

fn parse_completions(args: &[OsString]) -> Result<CompletionsCommand> {
    match args.first().and_then(|arg| arg.to_str()) {
        None if args.is_empty() => Ok(CompletionsCommand::Help),
        Some("-h" | "--help" | "help") if args.len() == 1 => Ok(CompletionsCommand::Help),
        Some("--providers") if args.len() == 1 => {
            Ok(CompletionsCommand::ListProviders(ProviderIdFilter::All))
        }
        Some("--configured") if args.len() == 1 => {
            Ok(CompletionsCommand::ListProviders(ProviderIdFilter::Configured))
        }
        Some("--targets") if args.len() == 1 => {
            Ok(CompletionsCommand::ListProviders(ProviderIdFilter::Targets))
        }
        Some("bash") if args.len() == 1 => {
            Ok(CompletionsCommand::Generate { shell: CompletionShell::Bash })
        }
        Some("zsh") if args.len() == 1 => {
            Ok(CompletionsCommand::Generate { shell: CompletionShell::Zsh })
        }
        Some("fish") if args.len() == 1 => {
            Ok(CompletionsCommand::Generate { shell: CompletionShell::Fish })
        }
        Some("bash" | "zsh" | "fish" | "--providers" | "--configured" | "--targets") => {
            bail!(
                "unexpected argument: {}\n\n{}",
                args[1].to_string_lossy(),
                crate::completions::help()
            )
        }
        Some(command) => {
            bail!("unknown completions command: {command}\n\n{}", crate::completions::help())
        }
        None => {
            bail!(
                "unknown completions command: {}\n\n{}",
                args[0].to_string_lossy(),
                crate::completions::help()
            )
        }
    }
}

fn parse_update(args: &[OsString]) -> Result<UpdateCommand> {
    match args.first().and_then(|arg| arg.to_str()) {
        Some("-h" | "--help") if args.len() == 1 => Ok(UpdateCommand::Help),
        _ => Ok(UpdateCommand::Run { yes: parse_update_args(args)? }),
    }
}

fn parse_update_args(args: &[OsString]) -> Result<bool> {
    let mut yes = false;
    for arg in args {
        match arg.to_str() {
            Some("--yes" | "-y") => yes = true,
            Some(other) => bail!("unexpected argument: {other}\n\nusage: rx update [--yes]"),
            None => {
                bail!("unexpected argument: {}\n\nusage: rx update [--yes]", arg.to_string_lossy())
            }
        }
    }
    Ok(yes)
}
