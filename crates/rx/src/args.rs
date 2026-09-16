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
    pub(crate) const ALL: [Self; 6] =
        [Self::Claude, Self::Codex, Self::OpenCode, Self::Pi, Self::Dsh, Self::Kimi];

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

    pub(crate) fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|harness| harness.as_str() == name)
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
    ModelsHelp,
    ModelsUpdate { provider: Option<String> },
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
    Host { passthrough: Vec<OsString> },
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

pub(crate) fn has_flags(args: &[OsString], flags: &[&str]) -> bool {
    before_double_dash(args)
        .iter()
        .any(|arg| flags.iter().any(|flag| arg == *flag || os_prefix(arg, &format!("{flag}="))))
}

pub(crate) fn parse(args: &[OsString]) -> Result<Command> {
    let (provider, rest) = extract_provider(args.get(1..).unwrap_or(&[]))?;
    let Some(first) = rest.first() else {
        return Ok(Command::PickHarness { provider });
    };
    let name = first.to_str();
    if provider.is_some() && matches!(name, Some("providers" | "update" | "completions" | "host")) {
        bail!("--provider is not valid with rx {}", first.to_string_lossy());
    }
    let args = &rest[1..];
    match name {
        Some("-h" | "--help") => Ok(Command::Help),
        Some("-V" | "--version") => Ok(Command::Version),
        Some("providers") => Ok(Command::Providers(parse_providers(args)?)),
        Some("update") => Ok(Command::Update(parse_update(args)?)),
        Some("completions") => Ok(Command::Completions(parse_completions(args)?)),
        Some("host") => match args {
            [] => Ok(Command::Host { passthrough: Vec::new() }),
            [separator, rest @ ..] if separator == "--" => {
                Ok(Command::Host { passthrough: rest.to_vec() })
            }
            [arg, ..] => bail!(
                "unexpected argument: {}\n\nusage: rx host [-- native harness args...]",
                arg.to_string_lossy()
            ),
        },
        _ => {
            let harness = name.and_then(Harness::parse).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown harness: {}\n\n{}",
                    first.to_string_lossy(),
                    crate::help_text().trim_end()
                )
            })?;
            Ok(Command::Launch(LaunchRequest { harness, provider, passthrough: args.to_vec() }))
        }
    }
}

fn parse_providers(args: &[OsString]) -> Result<ProvidersCommand> {
    match args.first().and_then(|arg| arg.to_str()) {
        None if args.is_empty() => Ok(ProvidersCommand::Help),
        Some("-h" | "--help" | "help") if args.len() <= 1 => Ok(ProvidersCommand::Help),
        Some("list") if args.len() == 1 => Ok(ProvidersCommand::List),
        Some("models") => parse_models(&args[1..]),
        Some(command @ ("login" | "logout" | "use")) => {
            let provider = parse_provider_argument(command, &args[1..])?;
            Ok(match command {
                "login" => ProvidersCommand::Login { provider },
                "logout" => ProvidersCommand::Logout { provider },
                "use" => ProvidersCommand::Use { provider },
                _ => unreachable!(),
            })
        }
        _ => {
            bail!(
                "unknown providers command: {}\n\n{}",
                args[0].to_string_lossy(),
                crate::providers::help()
            )
        }
    }
}

fn parse_models(args: &[OsString]) -> Result<ProvidersCommand> {
    match args.first().and_then(|arg| arg.to_str()) {
        None if args.is_empty() => Ok(ProvidersCommand::ModelsHelp),
        Some("-h" | "--help" | "help") if args.len() <= 1 => Ok(ProvidersCommand::ModelsHelp),
        Some("update") => Ok(ProvidersCommand::ModelsUpdate {
            provider: parse_provider_argument("models update", &args[1..])?,
        }),
        _ => {
            bail!(
                "unknown providers models command: {}\n\n{}",
                args[0].to_string_lossy(),
                crate::providers::models_help()
            )
        }
    }
}

fn parse_provider_argument(command: &str, args: &[OsString]) -> Result<Option<String>> {
    match args {
        [] => Ok(None),
        [arg] if arg.to_str().is_some() => Ok(arg.to_str().map(str::to_string)),
        _ => bail!("usage: rx providers {command} [provider]"),
    }
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
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            rest.push(arg.clone());
            rest.extend(args.cloned());
            break;
        }
        let value = if arg == "--provider" {
            args.next().and_then(|arg| arg.to_str())
        } else if os_prefix(arg, "--provider=") {
            arg.to_str().and_then(|arg| arg.strip_prefix("--provider=")).filter(|s| !s.is_empty())
        } else {
            rest.push(arg.clone());
            continue;
        };
        provider =
            Some(value.ok_or_else(|| anyhow::anyhow!("--provider requires a value"))?.to_string());
    }
    Ok((provider, rest))
}

fn parse_completions(args: &[OsString]) -> Result<CompletionsCommand> {
    let name = args.first().and_then(|arg| arg.to_str());
    let command = match name {
        None if args.is_empty() => return Ok(CompletionsCommand::Help),
        Some("-h" | "--help" | "help") if args.len() == 1 => return Ok(CompletionsCommand::Help),
        Some("--providers") => CompletionsCommand::ListProviders(ProviderIdFilter::All),
        Some("--configured") => CompletionsCommand::ListProviders(ProviderIdFilter::Configured),
        Some("--targets") => CompletionsCommand::ListProviders(ProviderIdFilter::Targets),
        Some("bash") => CompletionsCommand::Generate { shell: CompletionShell::Bash },
        Some("zsh") => CompletionsCommand::Generate { shell: CompletionShell::Zsh },
        Some("fish") => CompletionsCommand::Generate { shell: CompletionShell::Fish },
        _ => bail!(
            "unknown completions command: {}\n\n{}",
            args[0].to_string_lossy(),
            crate::completions::help()
        ),
    };
    if args.len() > 1 {
        bail!(
            "unexpected argument: {}\n\n{}",
            args[1].to_string_lossy(),
            crate::completions::help()
        );
    }
    Ok(command)
}

fn parse_update(args: &[OsString]) -> Result<UpdateCommand> {
    if args.len() == 1 && matches!(args[0].to_str(), Some("-h" | "--help")) {
        return Ok(UpdateCommand::Help);
    }
    if let Some(arg) = args.iter().find(|arg| !matches!(arg.to_str(), Some("--yes" | "-y"))) {
        bail!("unexpected argument: {}\n\nusage: rx update [--yes]", arg.to_string_lossy());
    }
    Ok(UpdateCommand::Run { yes: !args.is_empty() })
}
