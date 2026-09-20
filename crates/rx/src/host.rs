use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::args::{self, Harness, LaunchRequest};
use crate::config::Paths;
use crate::launch::{EnvLookup, ProviderTarget};
use crate::provider::{Provider, Setup};

const REQUEST_ENV: &str = "RX_HOST_REQUEST";

#[derive(Debug, Serialize)]
struct Capabilities {
    protocol: Protocol,
    version: &'static str,
    harnesses: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct Protocol {
    major: u16,
    minor: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostRequest {
    #[serde(default)]
    harness: Option<String>,
    gateway: GatewayProfile,
    state_dir: PathBuf,
    install_policy: InstallPolicy,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayProfile {
    provider_id: String,
    name: String,
    endpoint: String,
    credential_env: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InstallPolicy {
    Prompt,
    Deny,
}

pub(crate) fn run(passthrough: Vec<OsString>, env: &EnvLookup) -> Result<()> {
    let Some(raw) = env.get(REQUEST_ENV) else {
        println!("{}", capabilities_json()?);
        return Ok(());
    };
    let request = parse_request(&raw)?;
    let harness = match request.harness.as_deref() {
        Some(name) => {
            Harness::parse(name).ok_or_else(|| anyhow::anyhow!("unknown harness: {name}"))?
        }
        None => match crate::pick::harness(env)? {
            Some(harness) => harness,
            None => return Ok(()),
        },
    };
    validate_route_args(harness, &passthrough, &request.gateway.provider_id)?;
    let key = env
        .get(&request.gateway.credential_env)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "credential environment variable ${} is not set",
                request.gateway.credential_env
            )
        })?;
    let target = target(&request.gateway, key);
    let paths = Paths::in_dir(request.state_dir);
    let launch_request = LaunchRequest { harness, provider: None, passthrough };
    let install_env = EnvLookup::real_with(install_overrides(request.install_policy));
    let program = crate::install::ensure(harness, &install_env)?;
    let mut plan = crate::launch::plan_target(&launch_request, &paths, env, &target)?;
    plan.program = program;
    if let Some(note) = &plan.stderr_note {
        eprintln!("{note}");
    }
    crate::launch::exec(&plan)
}

fn capabilities_json() -> Result<String> {
    serde_json::to_string(&Capabilities {
        protocol: Protocol { major: 1, minor: 1 },
        version: crate::RELEASE_VERSION,
        harnesses: Harness::ALL.iter().map(|harness| harness.as_str()).collect(),
    })
    .context("failed to serialize host capabilities")
}

fn parse_request(raw: &str) -> Result<HostRequest> {
    if raw.is_empty() {
        bail!("RX_HOST_REQUEST is empty");
    }
    let request: HostRequest =
        serde_json::from_str(raw).context("RX_HOST_REQUEST is not valid JSON")?;
    if request.state_dir.as_os_str().is_empty() {
        bail!("host state_dir is empty");
    }
    if !request.state_dir.is_absolute() {
        bail!("host state_dir must be absolute");
    }
    crate::provider::validate_id(&request.gateway.provider_id)?;
    if request.gateway.name.trim().is_empty() {
        bail!("host gateway name is empty");
    }
    validate_endpoint(&request.gateway.endpoint)?;
    validate_env_name(&request.gateway.credential_env)?;
    Ok(request)
}

fn validate_endpoint(endpoint: &str) -> Result<()> {
    if !(endpoint.starts_with("https://") || endpoint.starts_with("http://"))
        || endpoint.chars().any(|character| {
            character.is_whitespace() || character.is_control() || matches!(character, '"' | '\\')
        })
    {
        bail!("host gateway endpoint must be an HTTP(S) URL");
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        bail!("host credential_env is not a valid environment variable name");
    }
    Ok(())
}

fn install_overrides(policy: InstallPolicy) -> HashMap<String, String> {
    HashMap::from([(
        "RX_NO_INSTALL".to_string(),
        match policy {
            InstallPolicy::Prompt => "0",
            InstallPolicy::Deny => "1",
        }
        .to_string(),
    )])
}

fn target(profile: &GatewayProfile, key: String) -> ProviderTarget {
    let provider = Provider {
        id: profile.provider_id.clone(),
        name: profile.name.clone(),
        endpoint: profile.endpoint.clone(),
        anthropic_base: None,
        default_context: None,
        env: profile.credential_env.clone(),
        setup: Setup::Generated,
        default_model: None,
        claude_default_model: None,
    };
    ProviderTarget { provider, key, model: None }
}

fn validate_route_args(
    harness: Harness,
    passthrough: &[OsString],
    provider_id: &str,
) -> Result<()> {
    let args = args::before_double_dash(passthrough);
    match harness {
        Harness::Claude => reject_flags(args, &["--settings", "--setting-sources"]),
        Harness::Codex => validate_codex(args),
        Harness::OpenCode => validate_scoped_values(args, &["-m", "--model"], provider_id),
        Harness::Pi => {
            reject_flags(args, &["--api-key"])?;
            validate_exact_values(args, &["--provider"], provider_id)?;
            validate_scoped_values(args, &["-m", "--model"], provider_id)?;
            validate_list_values(args, &["--models"], provider_id)
        }
        Harness::Dsh => reject_flags(args, &["--profile", "--patch"]),
        Harness::Kimi => Ok(()),
    }
}

fn validate_codex(args: &[OsString]) -> Result<()> {
    reject_flags(args, &["--oss", "--local-provider"])?;
    for value in flag_values(args, &["-c", "--config"]) {
        let key = value.split_once('=').map_or(value, |(key, _)| key).trim();
        if key == "model_provider"
            || key == "openai_base_url"
            || key == "model_providers"
            || key.starts_with("model_providers.")
        {
            bail!("{key} cannot override the hosted Gateway route");
        }
    }
    Ok(())
}

fn validate_exact_values(args: &[OsString], flags: &[&str], expected: &str) -> Result<()> {
    for value in flag_values(args, flags) {
        if value != expected {
            bail!("hosted mode requires {expected} for {}", flags[0]);
        }
    }
    Ok(())
}

fn validate_scoped_values(args: &[OsString], flags: &[&str], expected: &str) -> Result<()> {
    for value in flag_values(args, flags) {
        if let Some((provider, _)) = value.split_once('/')
            && provider != expected
        {
            bail!("hosted mode requires {expected} models for {}", flags[0]);
        }
    }
    Ok(())
}

fn validate_list_values(args: &[OsString], flags: &[&str], expected: &str) -> Result<()> {
    for value in flag_values(args, flags) {
        if value.split(',').any(|pattern| !pattern.starts_with(&format!("{expected}/"))) {
            bail!("hosted mode requires {expected} model patterns for {}", flags[0]);
        }
    }
    Ok(())
}

fn reject_flags(args: &[OsString], flags: &[&str]) -> Result<()> {
    if args::has_flags(args, flags) {
        bail!("{} cannot override the hosted Gateway route", flags[0]);
    }
    Ok(())
}

fn flag_values<'a>(args: &'a [OsString], flags: &[&str]) -> Vec<&'a str> {
    let mut values = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = if flags.iter().any(|flag| arg == *flag) {
            args.next().and_then(|value| value.to_str())
        } else {
            arg.to_str().and_then(|arg| {
                flags.iter().find_map(|flag| {
                    arg.strip_prefix(&format!("{flag}="))
                        .or_else(|| (flag.len() == 2).then(|| arg.strip_prefix(flag)).flatten())
                })
            })
        };
        values.extend(value);
    }
    values
}

#[cfg(test)]
mod tests;
