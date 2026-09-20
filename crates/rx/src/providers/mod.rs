mod picker;

use crate::args::ProvidersCommand;
use crate::catalog;
use crate::config::{AuthMode, Paths};
use crate::launch::{self, EnvLookup};
use crate::provider::Provider;
use anyhow::{Result, bail};
use picker::{Outcome, run_ui};

#[derive(Debug, Clone)]
struct ProviderState {
    provider: Provider,
    configured: bool,
    stored_key: bool,
    environment_active: bool,
    default: bool,
}

impl ProviderState {
    fn selectable(&self, action: Action) -> bool {
        match action {
            Action::Login => true,
            Action::Logout => self.stored_key || self.configured,
            Action::Use => self.configured,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Login,
    Logout,
    Use,
}

pub(crate) fn run(command: ProvidersCommand, paths: &Paths, env: &EnvLookup) -> Result<()> {
    match command {
        ProvidersCommand::Help => {
            print!("{}", help());
            Ok(())
        }
        ProvidersCommand::List => list(paths, env),
        ProvidersCommand::Login { provider } => login(paths, env, provider.as_deref()),
        ProvidersCommand::Logout { provider } => logout(paths, env, provider.as_deref()),
        ProvidersCommand::Use { provider } => use_provider(paths, env, provider.as_deref()),
        ProvidersCommand::ModelsHelp => {
            print!("{}", models_help());
            Ok(())
        }
        ProvidersCommand::ModelsUpdate { provider } => {
            update_models(paths, env, provider.as_deref())
        }
    }
}

pub(crate) fn help() -> &'static str {
    concat!(
        "rx providers — manage AI providers\n\n",
        "Usage:\n",
        "  rx providers list\n",
        "  rx providers login [provider]\n",
        "  rx providers logout [provider]\n",
        "  rx providers use [provider|none]\n",
        "  rx providers models update [provider]\n\n",
    )
}

pub(crate) fn models_help() -> &'static str {
    concat!(
        "rx providers models — update provider model catalogs\n\n",
        "Usage:\n",
        "  rx providers models update [provider]\n\n",
    )
}

fn update_models(paths: &Paths, env: &EnvLookup, requested: Option<&str>) -> Result<()> {
    let mut ids = Vec::new();
    if let Some(id) = requested {
        ids.push(id.to_string());
    } else {
        for state in provider_states(paths, env)? {
            if state.configured {
                ids.push(state.provider.id.clone());
            }
        }
        if ids.is_empty() {
            println!("No providers configured. Run: rx providers login");
            return Ok(());
        }
    }
    for id in ids {
        let target = match launch::configured_provider(Some(&id), paths, env)? {
            launch::ProviderResolution::Target(target) => target,
            launch::ProviderResolution::SkipInjection => {
                bail!("'{id}' skips provider injection; there is no catalog to update")
            }
            launch::ProviderResolution::Unconfigured => {
                bail!("no API key for provider '{id}'; run: rx providers login {id}")
            }
        };
        let count = catalog::update_models(
            paths,
            &target.provider.id,
            &target.provider.endpoint,
            &target.key,
        )?;
        println!("{}: {count} models", target.provider.id);
    }
    Ok(())
}

fn list(paths: &Paths, env: &EnvLookup) -> Result<()> {
    let states = provider_states(paths, env)?;
    let configured = states.iter().filter(|provider| provider.configured).collect::<Vec<_>>();
    if configured.is_empty() {
        println!("No providers configured. Run: rx providers login");
        return Ok(());
    }
    print!("{}", render_list(&configured));
    Ok(())
}

fn login(paths: &Paths, env: &EnvLookup, requested: Option<&str>) -> Result<()> {
    let states = provider_states(paths, env)?;
    let selected = requested.map(|id| provider_index(&states, id, Action::Login)).transpose()?;
    let outcome = run_ui(Action::Login, &states, env, selected)?;
    let Some(Outcome::Login { id, key }) = outcome else {
        return Ok(());
    };
    crate::config::login(paths, &id, key)?;
    let provider =
        crate::provider::resolve(&id, crate::config::load_or_default(paths)?.provider.get(&id))?;
    println!("* {} configured and set as default\n  {}", provider.name, provider.endpoint);
    Ok(())
}

fn logout(paths: &Paths, env: &EnvLookup, requested: Option<&str>) -> Result<()> {
    if let Some(state) = select_provider(Action::Logout, paths, env, requested)? {
        logout_provider(paths, env, &state)?;
    }
    Ok(())
}

fn logout_provider(paths: &Paths, env: &EnvLookup, state: &ProviderState) -> Result<()> {
    let report = crate::residue::purge(&state.provider.id, paths, env);
    if report.removed() {
        println!("Cleared rx-owned harness configuration for {}.", state.provider.name);
    }
    for note in report.notes() {
        println!("  {note}");
    }
    if report.credential_retained() {
        println!(
            "{} is NOT logged out: a copy of the API key is still on disk, so the stored key was kept. Clear the blocker above, then run this logout again.",
            state.provider.name
        );
        return Ok(());
    }
    let removed = crate::config::logout(paths, &state.provider.id)?;
    if removed {
        println!("Removed stored API key for {}.", state.provider.name);
    }
    if state.environment_active {
        println!(
            "{} is still available through ${}. Run this in your shell to remove it:\n  unset {}",
            state.provider.name, state.provider.env, state.provider.env
        );
    } else if removed {
        println!("{} logged out.", state.provider.name);
    }
    Ok(())
}

fn use_provider(paths: &Paths, env: &EnvLookup, requested: Option<&str>) -> Result<()> {
    if requested.is_some_and(crate::provider::is_none) {
        crate::config::set_none(paths)?;
        println!("* none\n  launch harnesses as-is");
        return Ok(());
    }
    if let Some(state) = select_provider(Action::Use, paths, env, requested)? {
        set_default_provider(paths, &state)?;
    }
    Ok(())
}

fn select_provider(
    action: Action,
    paths: &Paths,
    env: &EnvLookup,
    requested: Option<&str>,
) -> Result<Option<ProviderState>> {
    let states = provider_states(paths, env)?;
    let index = if let Some(id) = requested {
        provider_index(&states, id, action)?
    } else {
        if !states.iter().any(|state| state.selectable(action)) {
            println!(
                "{}",
                if action == Action::Logout {
                    "No providers configured."
                } else {
                    "No providers configured. Run: rx providers login"
                }
            );
            return Ok(None);
        }
        match run_ui(action, &states, env, None)? {
            Some(Outcome::Logout { index } | Outcome::Use { index }) => index,
            _ => return Ok(None),
        }
    };
    Ok(states.into_iter().nth(index))
}

fn set_default_provider(paths: &Paths, state: &ProviderState) -> Result<()> {
    crate::config::set_default(paths, &state.provider.id)?;
    println!("* {} set as default\n  {}", state.provider.name, state.provider.endpoint);
    Ok(())
}

pub(crate) fn completion_ids(
    paths: &Paths,
    env: &EnvLookup,
    filter: crate::args::ProviderIdFilter,
) -> Result<Vec<String>> {
    let mut ids = provider_states(paths, env)?
        .into_iter()
        .filter(|state| match filter {
            crate::args::ProviderIdFilter::All => true,
            crate::args::ProviderIdFilter::Configured | crate::args::ProviderIdFilter::Targets => {
                state.configured
            }
        })
        .map(|state| state.provider.id)
        .collect::<Vec<_>>();
    if matches!(filter, crate::args::ProviderIdFilter::Targets) {
        ids.push(crate::provider::NONE.to_string());
    }
    Ok(ids)
}

fn provider_index(states: &[ProviderState], id: &str, action: Action) -> Result<usize> {
    let index = states
        .iter()
        .position(|state| state.provider.id == id)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {id}"))?;
    if !states[index].selectable(action) {
        bail!("provider '{id}' is not configured; run: rx providers login {id}");
    }
    Ok(index)
}

fn provider_states(paths: &Paths, env: &EnvLookup) -> Result<Vec<ProviderState>> {
    let config = crate::config::load_or_default(paths)?;
    let stored = crate::config::stored_providers(paths)?;
    let mut states = crate::provider::available(&config)?
        .into_iter()
        .map(|provider| {
            let entry = config.provider.get(&provider.id);
            let auth = entry.map(|entry| entry.auth).unwrap_or(AuthMode::ApiKey);
            let environment = env.get(&provider.env).is_some();
            let environment_active =
                crate::provider::credential_source(&provider, auth, false, environment).is_some();
            let configured = crate::provider::credential_source(
                &provider,
                auth,
                stored.contains(&provider.id),
                environment,
            )
            .is_some();
            let default =
                config.default_provider.as_deref().unwrap_or("openrouter") == provider.id.as_str();
            ProviderState {
                stored_key: stored.contains(&provider.id),
                provider,
                configured,
                environment_active,
                default,
            }
        })
        .collect::<Vec<_>>();
    sort_provider_states(&mut states);
    Ok(states)
}

fn sort_provider_states(states: &mut [ProviderState]) {
    states.sort_by_cached_key(|state| {
        (
            state.provider.id != "openrouter",
            !state.configured,
            state.provider.name.to_ascii_lowercase(),
            state.provider.id.clone(),
        )
    });
}

fn render_list(providers: &[&ProviderState]) -> String {
    let name_width = providers
        .iter()
        .map(|state| state.provider.name.chars().count())
        .max()
        .unwrap_or(7)
        .max("PROVIDER".len());
    let mut output = format!("  {:<name_width$}  API ENDPOINT\n", "PROVIDER");
    for state in providers {
        let marker = if state.default { '*' } else { '•' };
        output.push_str(&format!(
            "{marker} {:<name_width$}  {}\n",
            state.provider.name, state.provider.endpoint
        ));
    }
    output
}
