mod lease;
mod store;

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::args;
use crate::catalog::{self, ListedModel};
use crate::config::Paths;
use crate::launch::{EnvLookup, LaunchPlan, ProviderTarget};

pub(crate) fn prepare(
    target: &ProviderTarget,
    configured_model: Option<&str>,
    paths: &Paths,
    env: &EnvLookup,
    passthrough: &[OsString],
) -> Result<LaunchPlan> {
    let provider_id = target.provider.id.as_str();
    let provider_alias = format!("rx-{provider_id}");
    let model_prefix = format!("{provider_alias}/");
    let (requested_model, mut args) = take_model(passthrough)?;
    let preferred_model = requested_model
        .or_else(|| configured_model.map(str::to_string))
        .map(|model| model.strip_prefix(&model_prefix).unwrap_or(&model).to_string());
    let allow_fetch = env.is_real() || target.provider.setup == crate::provider::Setup::Generated;
    let mut notes = Vec::new();
    let mut models = match catalog::load_listed_models(
        paths,
        provider_id,
        &target.provider.endpoint,
        &target.key,
        allow_fetch,
    ) {
        Ok(models) => models,
        Err(error) if preferred_model.is_some() => {
            notes.push(format!(
                "[rx] kimi: provider catalog unavailable; seeding only the selected model: {error:#}"
            ));
            Vec::new()
        }
        Err(error) => return Err(error),
    };
    let selected_model = match preferred_model {
        Some(model) => model,
        None => {
            let model = models.first().map(|model| model.id.clone()).ok_or_else(|| {
                anyhow::anyhow!(
                    "kimi needs a model for provider '{provider_id}'; pass --model <id> or set [provider.{provider_id}] model in rx.toml"
                )
            })?;
            notes.push(format!(
                "[rx] kimi: no model selected; using first provider model '{model}' (set [provider.{provider_id}] model to choose)"
            ));
            model
        }
    };
    if !models.iter().any(|model| model.id == selected_model) {
        models.push(ListedModel {
            id: selected_model.clone(),
            name: None,
            context_length: Some(
                target.provider.default_context.unwrap_or(catalog::DEFAULT_CONTEXT_WINDOW),
            ),
        });
    }
    let config_path = kimi_config_path(paths, env)?;
    let lease = store::seed_catalog(&config_path, &provider_alias, target, &models, env.is_real())?;
    args.insert(0, OsString::from(format!("{provider_alias}/{selected_model}")));
    args.insert(0, OsString::from("--model"));
    Ok(LaunchPlan {
        launch_lease: Some(lease),
        program: PathBuf::from("kimi"),
        args,
        env_set: vec![("KIMI_MODEL_NAME".to_string(), String::new())],
        stderr_note: (!notes.is_empty()).then(|| notes.join("\n")),
    })
}

fn kimi_config_path(paths: &Paths, env: &EnvLookup) -> Result<PathBuf> {
    if let Some(home) = env.get("KIMI_CODE_HOME").filter(|value| !value.trim().is_empty()) {
        return Ok(PathBuf::from(home).join("config.toml"));
    }
    if !env.is_real() {
        return Ok(paths.dir.join("kimi-code").join("config.toml"));
    }
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".kimi-code").join("config.toml"))
}

fn take_model(passthrough: &[OsString]) -> Result<(Option<String>, Vec<OsString>)> {
    let (flags, literal) = passthrough.split_at(args::before_double_dash(passthrough).len());
    let mut flags = flags.iter();
    let mut model = None;
    let mut kept = Vec::with_capacity(passthrough.len());
    while let Some(arg) = flags.next() {
        let value = if arg == "-m" || arg == "--model" {
            let value = flags
                .next()
                .ok_or_else(|| anyhow::anyhow!("{} requires a model id", arg.to_string_lossy()))?;
            Some(value.to_str().filter(|value| !value.is_empty()).ok_or_else(|| {
                anyhow::anyhow!("{} requires a UTF-8 model id", arg.to_string_lossy())
            })?)
        } else {
            arg.to_str()
                .and_then(|text| text.strip_prefix("--model=").or_else(|| text.strip_prefix("-m=")))
        };
        if let Some(value) = value {
            if value.is_empty() {
                bail!("{} requires a model id", arg.to_string_lossy());
            }
            model = Some(value.to_string());
        } else {
            kept.push(arg.clone());
        }
    }
    kept.extend_from_slice(literal);
    Ok((model, kept))
}

#[cfg(all(test, windows))]
mod tests;
