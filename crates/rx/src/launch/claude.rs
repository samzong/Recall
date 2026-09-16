use std::ffi::OsString;

use crate::args::LaunchRequest;
use crate::catalog::anthropic_base;
use crate::claude_catalog::{self, SeedOutcome};
use crate::provider::{Provider, Setup};

use super::LaunchPlan;

pub(super) fn plan(
    request: &LaunchRequest,
    provider: &Provider,
    key: &str,
    model: Option<&str>,
    seed: SeedOutcome,
) -> LaunchPlan {
    let openrouter = provider.setup == Setup::OpenRouter;
    inject(
        request,
        if openrouter { "OPENROUTER_API_KEY" } else { &provider.env },
        &crate::provider::claude_base(provider),
        key,
        model,
        openrouter,
        seed,
    )
}

#[cfg(test)]
pub(crate) fn inject_claude_openrouter(
    request: &LaunchRequest,
    base_url: &str,
    key: &str,
    model: Option<&str>,
    seed: SeedOutcome,
) -> LaunchPlan {
    inject(request, "OPENROUTER_API_KEY", base_url, key, model, true, seed)
}

#[cfg(test)]
pub(crate) fn inject_claude_generated_seeded(
    request: &LaunchRequest,
    env_key: &str,
    base_url: &str,
    key: &str,
    model: Option<&str>,
) -> LaunchPlan {
    inject(request, env_key, base_url, key, model, false, SeedOutcome::Seeded)
}

fn inject(
    request: &LaunchRequest,
    env_key: &str,
    base_url: &str,
    key: &str,
    model: Option<&str>,
    openrouter: bool,
    seed: SeedOutcome,
) -> LaunchPlan {
    let seeded = seed == SeedOutcome::Seeded;
    let mut plan = LaunchPlan::new(request);
    if seeded && !claude_catalog::user_passes_settings(&request.passthrough) {
        plan.args.splice(
            0..0,
            [
                OsString::from("--settings"),
                OsString::from(claude_catalog::claude_settings_json(base_url, true, env_key)),
            ],
        );
    }
    let env = &mut plan.env_set;
    env.push(("ANTHROPIC_BASE_URL".to_string(), anthropic_base(base_url)));
    if openrouter {
        env.extend(pairs([("ANTHROPIC_API_KEY", key), ("ANTHROPIC_AUTH_TOKEN", "")]));
    } else {
        env.extend(pairs([("ANTHROPIC_AUTH_TOKEN", key), ("ANTHROPIC_API_KEY", "")]));
    }
    if seeded || openrouter {
        env.extend(pairs([
            (env_key, key),
            ("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK", "1"),
            ("CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT", "1"),
        ]));
    }
    if seeded {
        env.extend(pairs([
            ("ENABLE_TOOL_SEARCH", "true"),
            ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "0"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "1000000"),
            ("DISABLE_TELEMETRY", "1"),
            ("DISABLE_GROWTHBOOK", "0"),
            ("CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF", "1"),
        ]));
    } else {
        env.extend(pairs([("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1")]));
        if openrouter {
            env.extend(pairs([
                ("ANTHROPIC_DEFAULT_FABLE_MODEL", "~anthropic/claude-fable-latest"),
                ("ANTHROPIC_DEFAULT_OPUS_MODEL", "~anthropic/claude-opus-latest"),
                (
                    "ANTHROPIC_DEFAULT_SONNET_MODEL",
                    model.unwrap_or("~anthropic/claude-sonnet-latest"),
                ),
                ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "~anthropic/claude-haiku-latest"),
            ]));
            plan.stderr_note = Some(
                "[rx] provider catalog seed failed; falling back to provider model discovery"
                    .to_string(),
            );
        } else {
            env.extend(pairs([(env_key, key)]));
        }
    }
    env.extend(model.map(|model| ("ANTHROPIC_MODEL".to_string(), model.to_string())));
    plan
}

fn pairs<const N: usize>(values: [(&str, &str); N]) -> [(String, String); N] {
    values.map(|(key, value)| (key.to_string(), value.to_string()))
}
