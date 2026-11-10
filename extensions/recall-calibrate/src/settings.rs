use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

pub const CLAUDE_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
pub const CODEX_KEY: &str = "model_auto_compact_token_limit";

#[derive(Clone, Debug, Serialize)]
pub struct Setting {
    pub file: String,
    pub key: String,
    pub value: Option<i64>,
    pub source: &'static str,
    pub locked_by: Option<String>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn config_dir(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(default))
}

fn number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn claude_model_key(model: &str) -> Option<String> {
    let rest = model.strip_prefix("claude-")?;
    let (family, rest) = rest.split_once('-')?;
    if !matches!(family, "opus" | "sonnet" | "haiku" | "fable") {
        return None;
    }
    let major: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if major.is_empty() {
        return None;
    }
    let rest = &rest[major.len()..];
    let mut key = format!("claude-{family}-{major}");
    if let Some(tail) = rest.strip_prefix('-') {
        let minor: String = tail.chars().take_while(char::is_ascii_digit).collect();
        if (1..=2).contains(&minor.len()) {
            key = format!("{key}-{minor}");
        }
    }
    Some(key)
}

pub fn claude(model_key: &str) -> Setting {
    let path = config_dir("CLAUDE_CONFIG_DIR", ".claude").join("settings.json");
    let json: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    let model_path = format!("modelSettings.{model_key}.autoCompactWindow");
    let mut setting = Setting {
        file: path.display().to_string(),
        key: model_path,
        value: None,
        source: "none",
        locked_by: None,
    };
    let env = json
        .pointer(&format!("/env/{CLAUDE_ENV}"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| std::env::var(CLAUDE_ENV).ok())
        .filter(|v| v != "0" && v != "false" && !v.is_empty());
    if let Some(v) = env {
        setting.value = v.trim().parse().ok();
        setting.source = "env";
        setting.locked_by = Some(CLAUDE_ENV.to_string());
        return setting;
    }
    let model_value = json
        .get("modelSettings")
        .and_then(|m| m.get(model_key))
        .and_then(|m| m.get("autoCompactWindow"));
    if let Some(v) = model_value.and_then(number) {
        setting.value = Some(v);
        setting.source = "model";
        return setting;
    }
    if let Some(v) = json.get("autoCompactWindow").and_then(number) {
        setting.value = Some(v);
        setting.source = "settings";
    }
    setting
}

pub fn codex() -> Setting {
    let path = config_dir("CODEX_HOME", ".codex").join("config.toml");
    let value = std::fs::read_to_string(&path).ok().and_then(|text| {
        text.lines()
            .map(|line| line.split('#').next().unwrap_or("").trim())
            .take_while(|line| !line.starts_with('['))
            .find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == CODEX_KEY)
                    .then(|| value.trim().trim_matches(['"', '\'']).replace('_', "").parse().ok())?
            })
    });
    Setting {
        file: path.display().to_string(),
        key: CODEX_KEY.to_string(),
        value,
        source: if value.is_some() { "settings" } else { "none" },
        locked_by: None,
    }
}
