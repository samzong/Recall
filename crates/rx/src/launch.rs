use std::collections::HashMap;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

use crate::args::{Harness, LaunchRequest};
use crate::claude_catalog::{self, SeedOutcome};
use crate::config::{AuthMode, GatewayConfig, Paths};

#[derive(Debug, Clone, Copy)]
pub(crate) struct DriverSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub default_base_url: &'static str,
    pub env_key: &'static str,
    pub default_model: Option<&'static str>,
    pub claude_default_model: Option<&'static str>,
}

pub(crate) const DRIVERS: &[DriverSpec] = &[
    DriverSpec {
        id: "openrouter",
        name: "OpenRouter",
        default_base_url: "https://openrouter.ai/api",
        env_key: "OPENROUTER_API_KEY",
        default_model: Some("~openai/gpt-latest"),
        claude_default_model: Some("~anthropic/claude-sonnet-latest"),
    },
    DriverSpec {
        id: "tokener",
        name: "Tokener",
        default_base_url: "https://api.tokener.dev",
        env_key: "TOKENER_API_KEY",
        default_model: None,
        claude_default_model: None,
    },
];

pub(crate) fn driver(id: &str) -> Result<&'static DriverSpec> {
    DRIVERS.iter().find(|driver| driver.id == id).ok_or_else(|| {
        anyhow::anyhow!("unknown gateway driver: {id} (supported: openrouter, tokener)")
    })
}

pub(crate) fn profile_driver(
    profile_id: &str,
    entry: Option<&GatewayConfig>,
) -> Result<&'static DriverSpec> {
    validate_profile_id(profile_id)?;
    if let Some(driver_id) = entry.and_then(|entry| entry.driver.as_deref()) {
        return driver(driver_id).map_err(|_| {
            anyhow::anyhow!(
                "gateway profile '{profile_id}' has unknown driver '{driver_id}' (supported: openrouter, tokener)"
            )
        });
    }
    if let Ok(driver) = driver(profile_id) {
        return Ok(driver);
    }
    if entry.is_some() {
        bail!(
            "gateway profile '{profile_id}' must set driver = \"openrouter\" or driver = \"tokener\""
        );
    }
    bail!("unknown gateway profile: {profile_id} (define it in rx.toml or use openrouter/tokener)")
}

fn validate_profile_id(id: &str) -> Result<()> {
    if !id.is_empty()
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(());
    }
    bail!("invalid gateway profile name '{id}'; use only letters, numbers, '-' and '_'")
}

#[derive(Debug, Clone, Default)]
pub struct EnvLookup {
    overrides: HashMap<String, String>,
    real: bool,
}

impl EnvLookup {
    pub(crate) fn real() -> Self {
        Self { overrides: HashMap::new(), real: true }
    }

    #[cfg(test)]
    pub(crate) fn isolated(overrides: HashMap<String, String>) -> Self {
        Self { overrides, real: false }
    }

    pub(crate) fn is_real(&self) -> bool {
        self.real
    }

    pub(crate) fn get(&self, key: &str) -> Option<String> {
        if let Some(value) = self.overrides.get(key) {
            return Some(value.clone());
        }
        if self.real { std::env::var(key).ok() } else { None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub stderr_note: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct GatewayTarget {
    pub profile_id: String,
    pub driver: &'static DriverSpec,
    pub base_url: String,
    pub key: String,
    pub model: Option<String>,
}

pub(crate) fn configured_gateway(
    override_id: Option<&str>,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<Option<GatewayTarget>> {
    let config = crate::config::load(paths)?;
    let gateway_id = override_id
        .map(str::to_string)
        .or_else(|| config.as_ref().and_then(|config| config.default_gateway.clone()));
    let Some(gateway_id) = gateway_id else {
        return Ok(None);
    };
    let entry = config.as_ref().and_then(|config| config.gateway.get(&gateway_id));
    let driver = profile_driver(&gateway_id, entry)?;
    let base_url =
        entry.and_then(|entry| entry.base_url.as_deref()).unwrap_or(driver.default_base_url);
    let auth = entry.map(|entry| entry.auth).unwrap_or(AuthMode::ApiKey);
    let key = resolve_key(&gateway_id, driver, auth, paths, env)?;
    let model = entry.and_then(|entry| entry.model.clone());
    Ok(Some(GatewayTarget {
        profile_id: gateway_id,
        driver,
        base_url: base_url.to_string(),
        key,
        model,
    }))
}

pub(crate) fn plan(request: &LaunchRequest, paths: &Paths, env: &EnvLookup) -> Result<LaunchPlan> {
    let Some(target) = configured_gateway(request.gateway.as_deref(), paths, env)? else {
        return Ok(passthrough(request));
    };
    let model = target.model.as_deref().or(match request.harness {
        Harness::Claude => target.driver.claude_default_model,
        Harness::Codex => target.driver.default_model,
        Harness::OpenCode | Harness::Pi => None,
    });
    if matches!(request.harness, Harness::Claude) {
        let seed = if env.is_real() {
            claude_catalog::try_seed_user_catalog(&target.base_url, &target.key, env)?
        } else {
            SeedOutcome::Fallback
        };
        if target.driver.id == "openrouter" {
            return Ok(inject_claude_openrouter(
                request,
                &target.base_url,
                &target.key,
                model,
                seed,
            ));
        }
        if matches!(seed, SeedOutcome::Seeded { .. }) {
            return Ok(inject_claude_tokener_seeded(
                request,
                target.driver,
                &target.base_url,
                &target.key,
                model,
            ));
        }
    }
    inject(request, paths, env, &target, model)
}

fn passthrough(request: &LaunchRequest) -> LaunchPlan {
    LaunchPlan {
        program: request.harness.as_str().to_string(),
        args: request.passthrough.clone(),
        env_set: Vec::new(),
        stderr_note: Some(format!(
            "[rx] no gateway configured; launching {} as-is (configure: rx config set gateway <profile>)",
            request.harness.as_str()
        )),
    }
}

fn resolve_key(
    profile_id: &str,
    driver: &DriverSpec,
    auth: AuthMode,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<String> {
    match auth {
        AuthMode::Env => env.get(driver.env_key).ok_or_else(|| {
            anyhow::anyhow!(
                "gateway '{}' is set to auth = env, but ${} is not set",
                profile_id,
                driver.env_key
            )
        }),
        AuthMode::ApiKey => {
            if let Some(key) = crate::config::stored_key(paths, profile_id)? {
                return Ok(key);
            }
            if let Some(key) = env.get(driver.env_key) {
                return Ok(key);
            }
            bail!(
                "no API key for gateway '{}'; run: rx config set key {} <key>  (or set ${} and auth = \"env\")",
                profile_id,
                profile_id,
                driver.env_key
            )
        }
    }
}

fn inject_claude_openrouter(
    request: &LaunchRequest,
    base_url: &str,
    key: &str,
    model: Option<&str>,
    seed: SeedOutcome,
) -> LaunchPlan {
    inject_claude_openrouter_impl(request, base_url, key, model, seed)
}

#[cfg(test)]
pub(crate) fn inject_claude_openrouter_for_test(
    request: &LaunchRequest,
    base_url: &str,
    key: &str,
    model: Option<&str>,
    seed: SeedOutcome,
) -> LaunchPlan {
    inject_claude_openrouter_impl(request, base_url, key, model, seed)
}

fn inject_claude_openrouter_impl(
    request: &LaunchRequest,
    base_url: &str,
    key: &str,
    model: Option<&str>,
    seed: SeedOutcome,
) -> LaunchPlan {
    let seeded = matches!(seed, SeedOutcome::Seeded { .. });
    let mut args = request.passthrough.clone();
    if seeded && !claude_catalog::user_passes_settings(&request.passthrough) {
        args.insert(0, claude_catalog::claude_settings_json(base_url, true, "OPENROUTER_API_KEY"));
        args.insert(0, "--settings".to_string());
    }
    let stderr_note = match seed {
        SeedOutcome::Fallback => Some(
            "[rx] gateway user catalog seed failed; falling back to gateway model discovery"
                .to_string(),
        ),
        SeedOutcome::Seeded { .. } => None,
    };
    LaunchPlan {
        program: "claude".to_string(),
        args,
        env_set: claude_openrouter_env(base_url, key, model, seeded),
        stderr_note,
    }
}

fn claude_openrouter_env(
    base_url: &str,
    key: &str,
    model: Option<&str>,
    seeded: bool,
) -> Vec<(String, String)> {
    let mut env_set = vec![
        ("ANTHROPIC_BASE_URL".to_string(), anthropic_base(base_url)),
        ("ANTHROPIC_API_KEY".to_string(), key.to_string()),
        ("ANTHROPIC_AUTH_TOKEN".to_string(), String::new()),
        ("OPENROUTER_API_KEY".to_string(), key.to_string()),
        ("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK".to_string(), "1".to_string()),
        ("CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT".to_string(), "1".to_string()),
    ];
    if seeded {
        env_set.extend([
            ("ENABLE_TOOL_SEARCH".to_string(), "true".to_string()),
            ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(), "0".to_string()),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(), "1".to_string()),
            ("CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(), "1000000".to_string()),
            ("DISABLE_TELEMETRY".to_string(), "1".to_string()),
            ("DISABLE_GROWTHBOOK".to_string(), "0".to_string()),
            ("CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF".to_string(), "1".to_string()),
        ]);
    } else {
        env_set.push(("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(), "1".to_string()));
        let sonnet = model.unwrap_or("~anthropic/claude-sonnet-latest");
        env_set.extend([
            (
                "ANTHROPIC_DEFAULT_FABLE_MODEL".to_string(),
                "~anthropic/claude-fable-latest".to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                "~anthropic/claude-opus-latest".to_string(),
            ),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(), sonnet.to_string()),
            (
                "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                "~anthropic/claude-haiku-latest".to_string(),
            ),
        ]);
    }
    if let Some(model) = model {
        env_set.push(("ANTHROPIC_MODEL".to_string(), model.to_string()));
    }
    env_set
}

fn inject_claude_tokener_seeded(
    request: &LaunchRequest,
    driver: &DriverSpec,
    base_url: &str,
    key: &str,
    model: Option<&str>,
) -> LaunchPlan {
    let mut args = request.passthrough.clone();
    if !claude_catalog::user_passes_settings(&request.passthrough) {
        args.insert(0, claude_catalog::claude_settings_json(base_url, true, driver.env_key));
        args.insert(0, "--settings".to_string());
    }
    LaunchPlan {
        program: "claude".to_string(),
        args,
        env_set: claude_tokener_seeded_env(driver, base_url, key, model),
        stderr_note: None,
    }
}

#[cfg(test)]
pub(crate) fn inject_claude_tokener_seeded_for_test(
    request: &LaunchRequest,
    driver: &DriverSpec,
    base_url: &str,
    key: &str,
    model: Option<&str>,
) -> LaunchPlan {
    inject_claude_tokener_seeded(request, driver, base_url, key, model)
}

fn claude_tokener_seeded_env(
    driver: &DriverSpec,
    base_url: &str,
    key: &str,
    model: Option<&str>,
) -> Vec<(String, String)> {
    let mut env_set = vec![
        ("ANTHROPIC_BASE_URL".to_string(), anthropic_base(base_url)),
        ("ANTHROPIC_AUTH_TOKEN".to_string(), key.to_string()),
        ("ANTHROPIC_API_KEY".to_string(), String::new()),
        (driver.env_key.to_string(), key.to_string()),
        ("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK".to_string(), "1".to_string()),
        ("CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT".to_string(), "1".to_string()),
        ("ENABLE_TOOL_SEARCH".to_string(), "true".to_string()),
        ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(), "0".to_string()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(), "1".to_string()),
        ("CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(), "1000000".to_string()),
        ("DISABLE_TELEMETRY".to_string(), "1".to_string()),
        ("DISABLE_GROWTHBOOK".to_string(), "0".to_string()),
        ("CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF".to_string(), "1".to_string()),
    ];
    if let Some(model) = model {
        env_set.push(("ANTHROPIC_MODEL".to_string(), model.to_string()));
    }
    env_set
}

fn claude_env(
    driver: &DriverSpec,
    base_url: &str,
    key: &str,
    model: Option<&str>,
) -> Vec<(String, String)> {
    let mut env_set = vec![
        ("ANTHROPIC_BASE_URL".to_string(), anthropic_base(base_url)),
        ("ANTHROPIC_AUTH_TOKEN".to_string(), key.to_string()),
        ("ANTHROPIC_API_KEY".to_string(), String::new()),
        ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(), "1".to_string()),
        (driver.env_key.to_string(), key.to_string()),
    ];
    if let Some(model) = model {
        env_set.push(("ANTHROPIC_MODEL".to_string(), model.to_string()));
    }
    env_set
}

fn inject(
    request: &LaunchRequest,
    paths: &Paths,
    env: &EnvLookup,
    target: &GatewayTarget,
    model: Option<&str>,
) -> Result<LaunchPlan> {
    let profile_id = target.profile_id.as_str();
    let driver = target.driver;
    let base_url = target.base_url.as_str();
    let key = target.key.as_str();
    match request.harness {
        Harness::Claude => Ok(LaunchPlan {
            program: "claude".to_string(),
            args: request.passthrough.clone(),
            env_set: claude_env(driver, base_url, key, model),
            stderr_note: None,
        }),
        Harness::Codex => {
            let openai_base = openai_base(base_url);
            let mut args = vec![
                "-c".to_string(),
                format!("model_provider=\"{profile_id}\""),
                "-c".to_string(),
                codex_provider_override(profile_id, driver, &openai_base),
            ];
            if let Some(model) = model.filter(|_| !user_sets_model(&request.passthrough)) {
                args.push("-c".to_string());
                args.push(format!("model=\"{model}\""));
            }
            args.extend(request.passthrough.iter().cloned());
            Ok(LaunchPlan {
                program: "codex".to_string(),
                args,
                env_set: vec![(driver.env_key.to_string(), key.to_string())],
                stderr_note: None,
            })
        }
        Harness::OpenCode => {
            let provider_id = if driver.id == "tokener" { profile_id } else { driver.id };
            let env_set = vec![
                (driver.env_key.to_string(), key.to_string()),
                (
                    "OPENCODE_CONFIG_CONTENT".to_string(),
                    crate::opencode::config_content(provider_id, driver, base_url, key)?,
                ),
            ];
            let mut args = request.passthrough.clone();
            if let Some(model) = model.filter(|_| !user_sets_opencode_model(&request.passthrough)) {
                args.insert(0, crate::opencode::prefixed_model(provider_id, model));
                args.insert(0, "-m".to_string());
            }
            Ok(LaunchPlan {
                program: "opencode".to_string(),
                args,
                env_set,
                stderr_note: crate::opencode::auth_conflict_note(driver, key, env),
            })
        }
        Harness::Pi => {
            let provider_id = if driver.id == "tokener" { profile_id } else { driver.id };
            crate::pi::prepare(provider_id, driver, base_url, key, paths, env)?;
            Ok(LaunchPlan {
                program: "pi".to_string(),
                args: crate::pi::args(provider_id, model, &request.passthrough),
                env_set: crate::pi::env_set(driver, key),
                stderr_note: None,
            })
        }
    }
}

fn codex_provider_override(profile_id: &str, driver: &DriverSpec, openai_base: &str) -> String {
    format!(
        "model_providers.{}={{name=\"{}\", base_url=\"{}\", wire_api=\"responses\", supports_websockets=false, {}}}",
        profile_id,
        driver.name,
        openai_base,
        auth_override(driver.env_key)
    )
}

fn auth_override(env_key: &str) -> String {
    #[cfg(unix)]
    {
        format!("auth={{command=\"sh\", args=[\"-c\", \"printf %s \\\"${env_key}\\\"\"]}}")
    }
    #[cfg(not(unix))]
    {
        format!(
            "auth={{command=\"powershell\", args=[\"-NoProfile\", \"-Command\", \"Write-Output $env:{env_key}\"]}}"
        )
    }
}

fn user_sets_opencode_model(passthrough: &[String]) -> bool {
    passthrough.iter().any(|arg| arg == "-m" || arg.starts_with("-m="))
}

fn user_sets_model(passthrough: &[String]) -> bool {
    let mut i = 0;
    while i < passthrough.len() {
        let arg = &passthrough[i];
        if arg == "-m" || arg == "--model" {
            return true;
        }
        if arg.starts_with("--model=") {
            return true;
        }
        if arg == "-c" || arg == "--config" {
            if passthrough.get(i + 1).is_some_and(|value| value.starts_with("model=")) {
                return true;
            }
            i += 1;
        }
        i += 1;
    }
    false
}

pub(crate) fn anthropic_base(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

pub(crate) fn openai_base(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.ends_with("/v1") { trimmed.to_string() } else { format!("{trimmed}/v1") }
}

pub(crate) fn exec(plan: &LaunchPlan) -> Result<()> {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args);
    for (key, value) in &plan.env_set {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = cmd.exec();
        Err(anyhow::Error::from(error).context(format!("failed to exec {}", plan.program)))
    }

    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        if !status.success() {
            anyhow::bail!("{} exited with status {status}", plan.program);
        }
        Ok(())
    }
}
