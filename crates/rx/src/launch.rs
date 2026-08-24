use std::collections::HashMap;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

use crate::args::{Harness, LaunchRequest};
use crate::catalog;
use crate::claude_catalog::{self, SeedOutcome};
use crate::config::{AuthMode, Paths};
use crate::provider::{Provider, Setup};

pub(crate) use crate::catalog::{anthropic_base, openai_base};

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
pub(crate) struct ProviderTarget {
    pub provider_id: String,
    pub provider: Provider,
    pub base_url: String,
    pub claude_url: String,
    pub key: String,
    pub model: Option<String>,
}

pub(crate) fn configured_provider(
    override_id: Option<&str>,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<Option<ProviderTarget>> {
    let config = crate::config::load(paths)?;
    let configured_id = override_id
        .map(str::to_string)
        .or_else(|| config.as_ref().and_then(|config| config.default_provider.clone()));
    let provider_id = match configured_id {
        Some(provider_id) => provider_id,
        None => {
            let entry = config.as_ref().and_then(|config| config.provider.get("openrouter"));
            let openrouter = crate::provider::resolve("openrouter", entry)?;
            if crate::config::stored_key(paths, "openrouter")?.is_some()
                || env.get(&openrouter.env).is_some()
            {
                "openrouter".to_string()
            } else {
                return Ok(None);
            }
        }
    };
    let entry = config.as_ref().and_then(|config| config.provider.get(&provider_id));
    let provider = crate::provider::resolve(&provider_id, entry)?;
    let auth = entry.map(|entry| entry.auth).unwrap_or(AuthMode::ApiKey);
    let key = resolve_key(&provider, auth, paths, env)?;
    let model = entry.and_then(|entry| entry.model.clone());
    Ok(Some(ProviderTarget {
        provider_id,
        base_url: provider.endpoint.clone(),
        claude_url: crate::provider::claude_base(&provider),
        provider,
        key,
        model,
    }))
}

pub(crate) fn plan(request: &LaunchRequest, paths: &Paths, env: &EnvLookup) -> Result<LaunchPlan> {
    let Some(target) = configured_provider(request.provider.as_deref(), paths, env)? else {
        return Ok(passthrough(request));
    };
    let model = target.model.as_deref().or(match request.harness {
        Harness::Claude => target.provider.claude_default_model,
        Harness::Codex => target.provider.default_model,
        Harness::OpenCode | Harness::Pi | Harness::Dsh => None,
    });
    if matches!(request.harness, Harness::Claude) {
        let seed = if env.is_real() {
            claude_catalog::try_seed_user_catalog(
                paths,
                &target.provider_id,
                &target.base_url,
                &target.key,
                env,
            )?
        } else {
            SeedOutcome::Fallback
        };
        if target.provider.setup == Setup::OpenRouter {
            return Ok(inject_claude_openrouter(
                request,
                &target.claude_url,
                &target.key,
                model,
                seed,
            ));
        }
        if matches!(seed, SeedOutcome::Seeded { .. }) {
            return Ok(inject_claude_generated_seeded(
                request,
                &target.provider.env,
                &target.claude_url,
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
        args: match request.harness {
            Harness::Dsh => crate::dsh::args(&request.passthrough, None),
            _ => request.passthrough.clone(),
        },
        env_set: Vec::new(),
        stderr_note: Some(format!(
            "[rx] no provider configured; launching {} as-is (configure: rx providers login)",
            request.harness.as_str()
        )),
    }
}

fn resolve_key(
    provider: &Provider,
    auth: AuthMode,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<String> {
    match auth {
        AuthMode::Env => env.get(&provider.env).ok_or_else(|| {
            anyhow::anyhow!(
                "provider '{}' is set to auth = env, but ${} is not set",
                provider.id,
                provider.env
            )
        }),
        AuthMode::ApiKey => {
            if let Some(key) = crate::config::stored_key(paths, &provider.id)? {
                return Ok(key);
            }
            if crate::provider::find(&provider.id).is_some()
                && let Some(key) = env.get(&provider.env)
            {
                return Ok(key);
            }
            bail!(
                "no API key for provider '{}'; run: rx providers login {} (or set ${})",
                provider.id,
                provider.id,
                provider.env
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
            "[rx] provider catalog seed failed; falling back to provider model discovery"
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

fn inject_claude_generated_seeded(
    request: &LaunchRequest,
    env_key: &str,
    base_url: &str,
    key: &str,
    model: Option<&str>,
) -> LaunchPlan {
    let mut args = request.passthrough.clone();
    if !claude_catalog::user_passes_settings(&request.passthrough) {
        args.insert(0, claude_catalog::claude_settings_json(base_url, true, env_key));
        args.insert(0, "--settings".to_string());
    }
    LaunchPlan {
        program: "claude".to_string(),
        args,
        env_set: claude_generated_seeded_env(env_key, base_url, key, model),
        stderr_note: None,
    }
}

#[cfg(test)]
pub(crate) fn inject_claude_generated_seeded_for_test(
    request: &LaunchRequest,
    env_key: &str,
    base_url: &str,
    key: &str,
    model: Option<&str>,
) -> LaunchPlan {
    inject_claude_generated_seeded(request, env_key, base_url, key, model)
}

fn claude_generated_seeded_env(
    env_key: &str,
    base_url: &str,
    key: &str,
    model: Option<&str>,
) -> Vec<(String, String)> {
    let mut env_set = vec![
        ("ANTHROPIC_BASE_URL".to_string(), anthropic_base(base_url)),
        ("ANTHROPIC_AUTH_TOKEN".to_string(), key.to_string()),
        ("ANTHROPIC_API_KEY".to_string(), String::new()),
        (env_key.to_string(), key.to_string()),
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
    env_key: &str,
    base_url: &str,
    key: &str,
    model: Option<&str>,
) -> Vec<(String, String)> {
    let mut env_set = vec![
        ("ANTHROPIC_BASE_URL".to_string(), anthropic_base(base_url)),
        ("ANTHROPIC_AUTH_TOKEN".to_string(), key.to_string()),
        ("ANTHROPIC_API_KEY".to_string(), String::new()),
        ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(), "1".to_string()),
        (env_key.to_string(), key.to_string()),
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
    target: &ProviderTarget,
    model: Option<&str>,
) -> Result<LaunchPlan> {
    let provider_id = target.provider_id.as_str();
    let provider = &target.provider;
    let base_url = target.base_url.as_str();
    let key = target.key.as_str();
    match request.harness {
        Harness::Claude => Ok(LaunchPlan {
            program: "claude".to_string(),
            args: request.passthrough.clone(),
            env_set: claude_env(&provider.env, &target.claude_url, key, model),
            stderr_note: None,
        }),
        Harness::Codex => {
            let openai_base = openai_base(base_url);
            let mut args = vec![
                "-c".to_string(),
                format!("model_provider=\"{provider_id}\""),
                "-c".to_string(),
                codex_provider_override(provider_id, provider, &openai_base),
            ];
            if env.is_real() {
                match catalog::prepare_codex_catalog(paths, provider_id, base_url, key) {
                    Ok(Some(path)) => {
                        args.push("-c".to_string());
                        args.push(format!("model_catalog_json=\"{}\"", path.display()));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("[rx] catalog seed skipped: {error:#}");
                    }
                }
            }
            if let Some(model) = model.filter(|_| !user_sets_model(&request.passthrough)) {
                args.push("-c".to_string());
                args.push(format!("model=\"{model}\""));
            }
            args.extend(request.passthrough.iter().cloned());
            Ok(LaunchPlan {
                program: "codex".to_string(),
                args,
                env_set: vec![(provider.env.clone(), key.to_string())],
                stderr_note: None,
            })
        }
        Harness::OpenCode => {
            let provider_id =
                if provider.setup == Setup::Generated { provider_id } else { provider.id.as_str() };
            let mut env_set = vec![(provider.env.clone(), key.to_string())];
            env_set.push((
                "OPENCODE_CONFIG_CONTENT".to_string(),
                crate::opencode::config_content(
                    provider_id,
                    provider,
                    base_url,
                    key,
                    paths,
                    env.is_real() || provider.setup == Setup::Generated,
                )?,
            ));
            let mut args = request.passthrough.clone();
            if let Some(model) = model.filter(|_| !user_sets_opencode_model(&request.passthrough)) {
                args.insert(0, crate::opencode::prefixed_model(provider_id, model));
                args.insert(0, "-m".to_string());
            }
            Ok(LaunchPlan {
                program: "opencode".to_string(),
                args,
                env_set,
                stderr_note: crate::opencode::auth_conflict_note(provider, key, env),
            })
        }
        Harness::Pi => {
            let provider_id =
                if provider.setup == Setup::Generated { provider_id } else { provider.id.as_str() };
            crate::pi::prepare(provider_id, provider, base_url, key, paths, env)?;
            Ok(LaunchPlan {
                program: "pi".to_string(),
                args: crate::pi::args(provider_id, model, &request.passthrough),
                env_set: crate::pi::env_set(&provider.env, key),
                stderr_note: None,
            })
        }
        Harness::Dsh => {
            let patch =
                crate::dsh::prepare(provider_id, provider, base_url, key, model, paths, env)?;
            Ok(LaunchPlan {
                program: "dsh".to_string(),
                args: crate::dsh::args(&request.passthrough, Some(&patch)),
                env_set: crate::dsh::env_set(provider_id, provider, key),
                stderr_note: None,
            })
        }
    }
}

fn codex_provider_override(provider_id: &str, provider: &Provider, openai_base: &str) -> String {
    format!(
        "model_providers.{}={{name=\"{}\", base_url=\"{}\", wire_api=\"responses\", supports_websockets=false, {}}}",
        provider_id,
        provider.name,
        openai_base,
        auth_override(&provider.env)
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
