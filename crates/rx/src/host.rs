use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

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
    permission_policy: PermissionPolicy,
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
enum PermissionPolicy {
    Standard,
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
    let mut overrides = state_env(&request.state_dir)?;
    prepare_state(&overrides)?;
    overrides.insert("RX_NO_YOLO".to_string(), "1".to_string());
    overrides.insert(
        "RX_NO_INSTALL".to_string(),
        match request.install_policy {
            InstallPolicy::Prompt => "0",
            InstallPolicy::Deny => "1",
        }
        .to_string(),
    );
    let hosted_env = EnvLookup::real_with(overrides.clone());
    let harness = match request.harness.as_deref() {
        Some(name) => {
            Harness::parse(name).ok_or_else(|| anyhow::anyhow!("unknown harness: {name}"))?
        }
        None => match crate::pick::harness(&hosted_env)? {
            Some(harness) => harness,
            None => return Ok(()),
        },
    };
    validate_route_args(harness, &passthrough)?;
    let key = env
        .get(&request.gateway.credential_env)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "credential environment variable ${} is not set",
                request.gateway.credential_env
            )
        })?;
    let target = target(&request.gateway, key)?;
    let paths = Paths::in_dir(request.state_dir);
    let launch_request = LaunchRequest { harness, provider: None, passthrough };
    let program = crate::install::ensure(harness, &hosted_env)?;
    let mut plan = crate::launch::plan_target(&launch_request, &paths, &hosted_env, &target)?;
    plan.program = program;
    plan.env_set.extend(overrides);
    if let Some(note) = &plan.stderr_note {
        eprintln!("{note}");
    }
    crate::launch::exec(&plan)
}

fn capabilities_json() -> Result<String> {
    serde_json::to_string(&Capabilities {
        protocol: Protocol { major: 1, minor: 0 },
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
    let _ = request.permission_policy;
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

fn state_env(state_dir: &Path) -> Result<HashMap<String, String>> {
    let path = |suffix: &str| {
        state_dir
            .join(suffix)
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("host state_dir must be UTF-8"))
    };
    Ok(HashMap::from([
        ("CLAUDE_CONFIG_DIR".to_string(), path("claude")?),
        ("CODEX_HOME".to_string(), path("codex")?),
        ("DSH_HOME".to_string(), path("dsh-home")?),
        ("KIMI_CODE_HOME".to_string(), path("kimi-code")?),
        ("PI_CODING_AGENT_DIR".to_string(), path("pi-agent")?),
        ("XDG_DATA_HOME".to_string(), path("data")?),
    ]))
}

fn prepare_state(environment: &HashMap<String, String>) -> Result<()> {
    for path in environment.values() {
        fs::create_dir_all(path)
            .with_context(|| format!("failed to create hosted state {path}"))?;
    }
    Ok(())
}

fn target(profile: &GatewayProfile, key: String) -> Result<ProviderTarget> {
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
    Ok(ProviderTarget {
        provider_id: profile.provider_id.clone(),
        base_url: profile.endpoint.clone(),
        claude_url: crate::provider::claude_base(&provider),
        provider,
        key,
        model: None,
    })
}

fn validate_route_args(harness: Harness, passthrough: &[OsString]) -> Result<()> {
    let args = args::before_double_dash(passthrough);
    match harness {
        Harness::Claude => reject_flags(args, &["--settings", "--setting-sources"]),
        Harness::Codex => validate_codex(args),
        Harness::OpenCode => validate_scoped_values(args, &["-m", "--model"], "tokener"),
        Harness::Pi => {
            reject_flags(args, &["--api-key"])?;
            validate_exact_values(args, &["--provider"], "tokener")?;
            validate_scoped_values(args, &["-m", "--model"], "tokener")?;
            validate_list_values(args, &["--models"], "tokener")
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
            || key.starts_with("model_providers.")
        {
            bail!("{} cannot override the hosted Gateway route", value);
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
    if args.iter().any(|arg| {
        flags.iter().any(|flag| arg == *flag || args::os_prefix(arg, &format!("{flag}=")))
    }) {
        bail!("{} cannot override the hosted Gateway route", flags[0]);
    }
    Ok(())
}

fn flag_values<'a>(args: &'a [OsString], flags: &[&str]) -> Vec<&'a str> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if flags.iter().any(|flag| arg == *flag) {
            if let Some(value) = args.get(index + 1).and_then(|value| value.to_str()) {
                values.push(value);
            }
            index += 2;
            continue;
        }
        if let Some(value) = arg
            .to_str()
            .and_then(|arg| flags.iter().find_map(|flag| arg.strip_prefix(&format!("{flag}="))))
        {
            values.push(value);
        }
        index += 1;
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn request(harness: Option<&str>) -> String {
        serde_json::json!({
            "harness": harness,
            "gateway": {
                "provider_id": "tokener",
                "name": "Tokener",
                "endpoint": "https://api.tokener.dev/v1",
                "credential_env": "TOKENER_API_KEY"
            },
            "state_dir": "/tmp/tokener-agent",
            "permission_policy": "standard",
            "install_policy": "prompt"
        })
        .to_string()
    }

    #[test]
    fn capabilities_are_stable() {
        let value: serde_json::Value = serde_json::from_str(&capabilities_json().unwrap()).unwrap();
        assert_eq!(value["protocol"], serde_json::json!({"major": 1, "minor": 0}));
        assert_eq!(
            value["harnesses"],
            serde_json::json!(["claude", "codex", "opencode", "pi", "dsh", "kimi"])
        );
        assert_eq!(value["version"], crate::RELEASE_VERSION);
    }

    #[test]
    fn hosted_request_allows_missing_harness() {
        assert!(parse_request(&request(None)).unwrap().harness.is_none());
        assert_eq!(
            parse_request(&request(Some("codex"))).unwrap().harness.as_deref(),
            Some("codex")
        );
    }

    #[test]
    fn request_rejects_unknown_fields_and_unsafe_profile_values() {
        let mut value: serde_json::Value = serde_json::from_str(&request(None)).unwrap();
        value["tokener_key"] = serde_json::json!("secret");
        assert!(parse_request(&value.to_string()).is_err());
        let mut value: serde_json::Value = serde_json::from_str(&request(None)).unwrap();
        value["gateway"]["credential_env"] = serde_json::json!("KEY;bad");
        assert!(parse_request(&value.to_string()).is_err());
    }

    #[test]
    fn route_guards_are_harness_specific() {
        validate_route_args(Harness::Claude, &os(&["--resume", "session", "--tools", "Read"]))
            .unwrap();
        assert!(validate_route_args(Harness::Claude, &os(&["--settings", "route.json"])).is_err());
        validate_route_args(
            Harness::Codex,
            &os(&["resume", "--last", "-c", "sandbox_mode=read-only"]),
        )
        .unwrap();
        assert!(
            validate_route_args(Harness::Codex, &os(&["-c", "model_provider=ollama"])).is_err()
        );
        validate_route_args(Harness::OpenCode, &os(&["--model", "tokener/model-a", "--fork"]))
            .unwrap();
        assert!(
            validate_route_args(Harness::OpenCode, &os(&["--model", "openai/model-a"])).is_err()
        );
        validate_route_args(
            Harness::Pi,
            &os(&["--provider", "tokener", "--model", "model-a", "--resume"]),
        )
        .unwrap();
        assert!(validate_route_args(Harness::Pi, &os(&["--api-key", "secret"])).is_err());
        assert!(validate_route_args(Harness::Dsh, &os(&["--patch", "other.yml"])).is_err());
        validate_route_args(Harness::Kimi, &os(&["--session", "session-id", "--plan"])).unwrap();
    }

    #[test]
    fn native_arguments_after_double_dash_are_literal() {
        validate_route_args(
            Harness::Claude,
            &os(&["--resume", "session", "--", "--settings", "literal"]),
        )
        .unwrap();
    }

    #[test]
    fn hosted_state_isolated_from_standalone_paths() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("tokener-agent");
        let env = state_env(&state).unwrap();
        assert_eq!(env["CLAUDE_CONFIG_DIR"], state.join("claude").to_str().unwrap());
        assert_eq!(env["CODEX_HOME"], state.join("codex").to_str().unwrap());
        assert_eq!(env["PI_CODING_AGENT_DIR"], state.join("pi-agent").to_str().unwrap());
        assert_eq!(env["DSH_HOME"], state.join("dsh-home").to_str().unwrap());
        assert_eq!(env["KIMI_CODE_HOME"], state.join("kimi-code").to_str().unwrap());
        prepare_state(&env).unwrap();
        assert!(env.values().all(|path| Path::new(path).is_dir()));
    }
}
