use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

use crate::args::{self, Harness, LaunchRequest};
use crate::catalog::{self, openai_base};
use crate::claude_catalog::{self, SeedOutcome};

mod claude;
mod permissions;

use crate::config::{AuthMode, Paths};
use crate::provider::{Provider, Setup};
#[cfg(test)]
pub(crate) use claude::{inject_claude_generated_seeded, inject_claude_openrouter};
pub(crate) use permissions::yolo_enabled;

#[derive(Debug, Clone, Default)]
pub(crate) struct EnvLookup {
    overrides: HashMap<String, String>,
    real: bool,
}

impl EnvLookup {
    pub(crate) fn real() -> Self {
        Self { overrides: HashMap::new(), real: true }
    }

    pub(crate) fn real_with(overrides: HashMap<String, String>) -> Self {
        Self { overrides, real: true }
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

#[derive(Debug)]
pub(crate) struct LaunchPlan {
    pub(crate) launch_lease: Option<std::fs::File>,
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) env_set: Vec<(String, String)>,
    pub(crate) stderr_note: Option<String>,
}

impl LaunchPlan {
    fn new(request: &LaunchRequest) -> Self {
        Self {
            launch_lease: None,
            program: PathBuf::from(request.harness.as_str()),
            args: request.passthrough.clone(),
            env_set: Vec::new(),
            stderr_note: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderTarget {
    pub provider: Provider,
    pub key: String,
    pub model: Option<String>,
}

#[derive(Debug)]
pub(crate) enum ProviderResolution {
    SkipInjection,
    Unconfigured,
    Target(Box<ProviderTarget>),
}

pub(crate) fn configured_provider(
    override_id: Option<&str>,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<ProviderResolution> {
    if override_id.is_some_and(crate::provider::is_none) {
        return Ok(ProviderResolution::SkipInjection);
    }
    let config = crate::config::load(paths)?;
    let configured_id = override_id
        .map(str::to_string)
        .or_else(|| config.as_ref().and_then(|config| config.default_provider.clone()));
    let provider_id = match configured_id {
        Some(provider_id) if crate::provider::is_none(&provider_id) => {
            return Ok(ProviderResolution::SkipInjection);
        }
        Some(provider_id) => provider_id,
        None => {
            let entry = config.as_ref().and_then(|config| config.provider.get("openrouter"));
            let openrouter = crate::provider::resolve("openrouter", entry)?;
            if crate::config::stored_key(paths, "openrouter")?.is_some()
                || env.get(&openrouter.env).is_some()
            {
                "openrouter".to_string()
            } else {
                return Ok(ProviderResolution::Unconfigured);
            }
        }
    };
    let entry = config.as_ref().and_then(|config| config.provider.get(&provider_id));
    let provider = crate::provider::resolve(&provider_id, entry)?;
    let auth = entry.map(|entry| entry.auth).unwrap_or(AuthMode::ApiKey);
    let key = resolve_key(&provider, auth, paths, env)?;
    let model = entry.and_then(|entry| entry.model.clone());
    Ok(ProviderResolution::Target(Box::new(ProviderTarget { provider, key, model })))
}

pub(crate) fn plan(request: &LaunchRequest, paths: &Paths, env: &EnvLookup) -> Result<LaunchPlan> {
    let target = match configured_provider(request.provider.as_deref(), paths, env)? {
        ProviderResolution::SkipInjection => {
            return Ok(passthrough(
                request,
                format!("[rx] provider 'none': launching {} as-is", request.harness.as_str()),
            ));
        }
        ProviderResolution::Unconfigured => {
            return Ok(passthrough(
                request,
                format!(
                    "[rx] no provider configured; launching {} as-is (configure: rx providers login)",
                    request.harness.as_str()
                ),
            ));
        }
        ProviderResolution::Target(target) => target,
    };
    plan_target(request, paths, env, &target)
}

pub(crate) fn plan_target(
    request: &LaunchRequest,
    paths: &Paths,
    env: &EnvLookup,
    target: &ProviderTarget,
) -> Result<LaunchPlan> {
    let model = target.model.as_deref().or(match request.harness {
        Harness::Claude => target.provider.claude_default_model,
        Harness::Codex => target.provider.default_model,
        Harness::OpenCode | Harness::Pi | Harness::Dsh | Harness::Kimi => None,
    });
    let mut plan = inject(request, paths, env, target, model)?;
    permissions::apply_yolo(request, &mut plan, env);
    Ok(plan)
}

fn passthrough(request: &LaunchRequest, note: String) -> LaunchPlan {
    let mut plan = LaunchPlan::new(request);
    if request.harness == Harness::Dsh {
        plan.args = crate::dsh::args(&request.passthrough, None);
    }
    plan.stderr_note = Some(note);
    plan
}

fn resolve_key(
    provider: &Provider,
    auth: AuthMode,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<String> {
    let stored = if auth == AuthMode::ApiKey {
        crate::config::stored_key(paths, &provider.id)?
    } else {
        None
    };
    let environment = env.get(&provider.env);
    match crate::provider::credential_source(
        provider,
        auth,
        stored.is_some(),
        environment.is_some(),
    ) {
        Some(crate::provider::CredentialSource::Stored) => Ok(stored.expect("stored credential")),
        Some(crate::provider::CredentialSource::Environment) => {
            Ok(environment.expect("environment credential"))
        }
        None if auth == AuthMode::Env => bail!(
            "provider '{}' is set to auth = env, but ${} is not set",
            provider.id,
            provider.env
        ),
        None => bail!(
            "no API key for provider '{}'; run: rx providers login {} (or set ${})",
            provider.id,
            provider.id,
            provider.env
        ),
    }
}

fn inject(
    request: &LaunchRequest,
    paths: &Paths,
    env: &EnvLookup,
    target: &ProviderTarget,
    model: Option<&str>,
) -> Result<LaunchPlan> {
    let provider = &target.provider;
    let provider_id = provider.id.as_str();
    let base_url = provider.endpoint.as_str();
    let key = target.key.as_str();
    let mut plan = LaunchPlan::new(request);
    match request.harness {
        Harness::Claude => {
            let seed = if env.is_real() {
                claude_catalog::try_seed_user_catalog(paths, provider_id, base_url, key, env)
            } else {
                SeedOutcome::Fallback
            };
            return Ok(claude::plan(request, provider, key, model, seed));
        }
        Harness::Codex => {
            let openai_base = openai_base(base_url);
            let mut args = vec![
                OsString::from("-c"),
                OsString::from(format!("model_provider={}", toml_edit::Value::from(provider_id))),
                OsString::from("-c"),
                OsString::from(codex_provider_override(provider_id, provider, &openai_base)),
            ];
            if env.is_real() {
                match catalog::prepare_codex_catalog(paths, provider_id, base_url, key) {
                    Ok(Some(path)) => {
                        args.push(OsString::from("-c"));
                        args.push(OsString::from(format!(
                            "model_catalog_json={}",
                            toml_edit::Value::from(path.to_str().ok_or_else(|| {
                                anyhow::anyhow!("Codex model catalog path must be UTF-8")
                            })?)
                        )));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("[rx] catalog seed skipped: {error:#}");
                    }
                }
            }
            if let Some(model) = model.filter(|_| !user_sets_model(&request.passthrough)) {
                args.push(OsString::from("-c"));
                args.push(OsString::from(format!("model={}", toml_edit::Value::from(model))));
            }
            args.extend(request.passthrough.iter().cloned());
            plan.args = args;
            plan.env_set = vec![(provider.env.clone(), key.to_string())];
        }
        Harness::OpenCode => {
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
            if let Some(model) = model.filter(|_| !user_sets_opencode_model(&request.passthrough))
                && let Some(at) = opencode_flag_index(&args)
            {
                args.splice(
                    at..at,
                    [
                        OsString::from("-m"),
                        OsString::from(crate::opencode::prefixed_model(provider_id, model)),
                    ],
                );
            }
            plan.args = args;
            plan.env_set = env_set;
            plan.stderr_note = crate::opencode::auth_conflict_note(provider, key, env);
        }
        Harness::Pi => {
            crate::pi::prepare(provider_id, provider, base_url, key, paths, env)?;
            plan.args = crate::pi::args(provider_id, model, &request.passthrough);
            plan.env_set = crate::pi::env_set(&provider.env, key);
        }
        Harness::Dsh => {
            let patch =
                crate::dsh::prepare(provider_id, provider, base_url, key, model, paths, env)?;
            plan.args = crate::dsh::args(&request.passthrough, Some(&patch));
            plan.env_set = crate::dsh::env_set(provider_id, provider, key);
            if yolo_enabled(env) {
                plan.stderr_note = Some(
                    "[rx] yolo: dsh permission preset danger-full-access (RX_NO_YOLO=1 disables)"
                        .to_string(),
                );
            }
        }
        Harness::Kimi => {
            return crate::kimi::prepare(target, model, paths, env, &request.passthrough);
        }
    }
    Ok(plan)
}

fn codex_provider_override(provider_id: &str, provider: &Provider, openai_base: &str) -> String {
    #[cfg(unix)]
    let auth = format!(
        "auth={{command=\"printenv\", args=[\"--\", {}]}}",
        toml_edit::Value::from(provider.env.as_str())
    );
    #[cfg(not(unix))]
    let auth = {
        let script = format!(
            "[Environment]::GetEnvironmentVariable('{}')",
            provider.env.replace('\'', "''")
        );
        format!(
            "auth={{command=\"powershell\", args=[\"-NoProfile\", \"-Command\", {}]}}",
            toml_edit::Value::from(script)
        )
    };
    format!(
        "model_providers.{}={{name={}, base_url={}, wire_api=\"responses\", supports_websockets=false, {auth}}}",
        provider_id,
        toml_edit::Value::from(provider.name.as_str()),
        toml_edit::Value::from(openai_base),
    )
}

fn user_sets_opencode_model(passthrough: &[OsString]) -> bool {
    args::has_flags(passthrough, &["-m", "--model"])
}

fn opencode_flag_index(passthrough: &[OsString]) -> Option<usize> {
    match args::before_double_dash(passthrough).first().and_then(|arg| arg.to_str()) {
        Some("run") => Some(1),
        Some(
            "completion" | "acp" | "mcp" | "attach" | "debug" | "providers" | "auth" | "agent"
            | "upgrade" | "uninstall" | "serve" | "web" | "models" | "stats" | "export" | "import"
            | "github" | "pr" | "session" | "plugin" | "plug" | "db",
        ) => None,
        _ => Some(0),
    }
}

fn user_sets_model(passthrough: &[OsString]) -> bool {
    let mut args = args::before_double_dash(passthrough).iter();
    while let Some(arg) = args.next() {
        if arg == "-m" || arg == "--model" || args::os_prefix(arg, "--model=") {
            return true;
        }
        if (arg == "-c" || arg == "--config")
            && args.next().is_some_and(|value| args::os_prefix(value, "model="))
        {
            return true;
        }
    }
    false
}

pub(crate) fn exec(plan: &LaunchPlan) -> Result<()> {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args);
    for key in ["RX_HOST_REQUEST", "RX_NO_INSTALL", "RX_NO_UPDATE", "RX_NO_YOLO"] {
        cmd.env_remove(key);
    }
    for (key, value) in &plan.env_set {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());

    let _lease = plan.launch_lease.as_ref().map(fs2::FileExt::duplicate).transpose()?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = cmd.exec();
        Err(anyhow::Error::from(error)
            .context(format!("failed to exec {}", plan.program.display())))
    }

    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        if !status.success() {
            anyhow::bail!("{} exited with status {status}", plan.program.display());
        }
        Ok(())
    }
}
