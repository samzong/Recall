use super::*;

#[test]
fn config_migrates_legacy_enabled_sources() {
    let legacy_json = r#"{
        "enabled_sources": ["claude-code", "codex", "opencode"],
        "sync_window": "week"
    }"#;
    let mut config: AppConfig = serde_json::from_str(legacy_json).unwrap();

    let known = vec![
        ("claude-code".to_string(), "CC".to_string()),
        ("opencode".to_string(), "OC".to_string()),
        ("codex".to_string(), "CDX".to_string()),
        ("gemini-cli".to_string(), "GEM".to_string()),
        ("kiro-cli".to_string(), "KIRO".to_string()),
    ];
    config.normalize_sources(&known);

    assert!(config.is_source_enabled("claude-code"));
    assert!(config.is_source_enabled("opencode"));
    assert!(config.is_source_enabled("codex"));
    assert!(
        config.is_source_enabled("gemini-cli"),
        "newly-added adapter should be enabled after migration"
    );
    assert!(
        config.is_source_enabled("kiro-cli"),
        "newly-added adapter should be enabled after migration"
    );

    let round_tripped = serde_json::to_string(&config).unwrap();
    assert!(
        !round_tripped.contains("enabled_sources"),
        "legacy field must not be re-serialized: {round_tripped}"
    );
}

#[test]
fn config_disables_persist_across_reloads() {
    let mut known = vec![
        ("claude-code".to_string(), "CC".to_string()),
        ("gemini-cli".to_string(), "GEM".to_string()),
    ];

    let mut config = AppConfig::default();
    config.normalize_sources(&known);
    config.disabled_sources.push("gemini-cli".to_string());

    let json = serde_json::to_string(&config).unwrap();
    let mut reloaded: AppConfig = serde_json::from_str(&json).unwrap();
    reloaded.normalize_sources(&known);

    assert!(reloaded.is_source_enabled("claude-code"));
    assert!(
        !reloaded.is_source_enabled("gemini-cli"),
        "explicit disable must survive a save/load cycle"
    );

    known.push(("kiro-cli".to_string(), "KIRO".to_string()));
    reloaded.normalize_sources(&known);
    assert!(
        reloaded.is_source_enabled("kiro-cli"),
        "a brand new adapter should default to enabled"
    );
    assert!(
        !reloaded.is_source_enabled("gemini-cli"),
        "previously disabled adapter must stay disabled"
    );
}

#[test]
fn config_drops_obsolete_disabled_entries_without_reenabling_known_sources() {
    let mut config = AppConfig::default();
    config.disabled_sources = vec!["ghost-adapter".to_string(), "claude-code".to_string()];
    let known = vec![("claude-code".to_string(), "CC".to_string())];
    config.normalize_sources(&known);

    assert!(!config.disabled_sources.iter().any(|id| id == "ghost-adapter"));
    assert!(!config.is_source_enabled("claude-code"), "explicit all-disabled state must survive");
}
