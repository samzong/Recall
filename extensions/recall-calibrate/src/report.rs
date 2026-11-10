use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde::Serialize;
use serde_json::Value;

use crate::compact::{self, Call, Compaction, Options, Weights};
use crate::recall::UsageEvent;
use crate::settings::{self, Setting};

const CLAUDE_WINDOW: i64 = 200_000;
const CLAUDE_LONG_WINDOW: i64 = 1_000_000;
const CODEX_WINDOW: i64 = 272_000;

#[derive(Serialize)]
pub struct Report {
    pub time: String,
    pub advice: Vec<Advice>,
}

#[derive(Serialize)]
pub struct Advice {
    pub agent: &'static str,
    pub model: String,
    pub calls: usize,
    pub setting: Setting,
    pub apply: Option<i64>,
    pub saving: Option<f64>,
    pub skipped: Option<String>,
    pub compaction: Option<Compaction>,
}

fn to_call(event: &UsageEvent) -> Option<Call> {
    let prompt = event.input_tokens + event.cache_read_tokens + event.cache_write_tokens;
    if prompt <= 0 {
        return None;
    }
    let stream = event
        .source_path
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| event.session_id.clone());
    let wrote1 = raw(event)
        .and_then(|v| {
            v.pointer("/cache_creation/ephemeral_1h_input_tokens").and_then(Value::as_i64)
        })
        .unwrap_or(0);
    Some(Call {
        time: event.timestamp,
        stream,
        prompt,
        out: event.output_tokens + event.reasoning_tokens,
        wrote: event.cache_write_tokens,
        wrote1,
    })
}

fn raw(event: &UsageEvent) -> Option<Value> {
    serde_json::from_str(event.raw_usage_json.as_deref()?).ok()
}

fn advice(
    agent: &'static str,
    model: String,
    calls: &[Call],
    setting: Setting,
    weights: Weights,
    options: Options,
) -> Advice {
    let compaction = compact::advise(calls, weights, options);
    let skipped = compaction.is_none().then(|| {
        if (calls.len() as i64) < compact::MIN_CALLS {
            format!("needs at least {} calls with a usable prompt", compact::MIN_CALLS)
        } else {
            "no candidate window fits between the post-compaction floor and the model window"
                .to_string()
        }
    });
    let apply = compaction
        .as_ref()
        .filter(|c| c.best != c.current || setting.value.is_some_and(|v| v != c.best))
        .map(|c| c.best);
    let saving =
        compaction.as_ref().map(|c| if c.cost > 0.0 { (c.cost - c.low) / c.cost } else { 0.0 });
    Advice { agent, model, calls: calls.len(), setting, apply, saving, skipped, compaction }
}

pub fn claude(events: &[UsageEvent]) -> Vec<Advice> {
    let mut by_model: BTreeMap<String, Vec<Call>> = BTreeMap::new();
    for event in events {
        let model = event.model.as_deref().unwrap_or("").to_lowercase();
        let Some(key) = settings::claude_model_key(&model) else { continue };
        if let Some(call) = to_call(event) {
            by_model.entry(key).or_default().push(call);
        }
    }
    let mut out: Vec<Advice> = by_model
        .into_iter()
        .map(|(model, calls)| {
            let setting = settings::claude(&model);
            let window = if calls.iter().any(|c| c.prompt > CLAUDE_WINDOW) {
                CLAUDE_LONG_WINDOW
            } else {
                CLAUDE_WINDOW
            };
            let options = Options {
                min: 100_000,
                max: 1_000_000,
                reserve: 33_000,
                current: setting.value.unwrap_or(0),
                window,
            };
            advice(
                "claude-code",
                model.clone(),
                &calls,
                setting,
                Weights::for_model(&model),
                options,
            )
        })
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.calls));
    out
}

pub fn codex(events: &[UsageEvent]) -> Vec<Advice> {
    let mut models = BTreeSet::new();
    let mut calls = vec![];
    let mut window = 0;
    for event in events {
        let Some(call) = to_call(event) else { continue };
        let context = raw(event)
            .and_then(|v| v.get("model_context_window").and_then(Value::as_i64))
            .filter(|w| *w > 0)
            .unwrap_or(CODEX_WINDOW);
        window = window.max(context).max(call.prompt);
        if let Some(model) = event.model.as_deref().filter(|m| !m.is_empty()) {
            models.insert(model.to_lowercase());
        }
        calls.push(call);
    }
    if calls.is_empty() {
        return vec![];
    }
    let setting = settings::codex();
    let current = setting.value.unwrap_or(0);
    let options = Options {
        min: 50_000,
        max: 1_000_000.max(window).max(current),
        reserve: 0,
        current,
        window,
    };
    let model = models.into_iter().collect::<Vec<_>>().join(",");
    let weights = Weights::for_model(&model);
    vec![advice("codex", model, &calls, setting, weights, options)]
}

fn k(tokens: i64) -> String {
    format!("{}K", (tokens + 500) / 1000)
}

pub fn render_text(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Compaction window advice over {}", report.time);
    if report.advice.is_empty() {
        let _ = writeln!(out, "\nNo Claude Code or Codex usage found.");
    }
    for a in &report.advice {
        let _ = writeln!(out, "\n{} {} ({} calls)", a.agent, a.model, a.calls);
        let now = match a.setting.value {
            Some(v) => format!("{} from {}", k(v), a.setting.source),
            None => "not set".to_string(),
        };
        let _ = writeln!(out, "  setting  {} in {} ({})", a.setting.key, a.setting.file, now);
        if let Some(lock) = &a.setting.locked_by {
            let _ = writeln!(out, "  locked   environment variable {lock} overrides the file");
        }
        let Some(c) = &a.compaction else {
            let _ = writeln!(out, "  skipped  {}", a.skipped.as_deref().unwrap_or(""));
            continue;
        };
        let saving = a.saving.unwrap_or(0.0) * 100.0;
        if c.best == c.current {
            let _ = writeln!(out, "  advice   keep {} (no window saves 3% or more)", k(c.current));
        } else {
            let _ = writeln!(
                out,
                "  advice   {} -> {}, {:.1}% fewer weighted tokens, compactions {} -> {}",
                k(c.current),
                k(c.best),
                saving,
                c.compacts_current,
                c.compacts_best
            );
        }
        let f = &c.facts;
        let _ = writeln!(
            out,
            "  observed {} conversations, {} compactions (median {} -> {}), median growth {}/call",
            f.streams,
            f.compacts,
            k(f.peak),
            k(f.after),
            k(f.growth)
        );
    }
    out
}
