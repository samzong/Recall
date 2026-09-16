use super::*;

#[test]
fn dsh_deepseek_uses_official_adapter_and_clears_pi_ai_routes() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("DEEPSEEK_API_KEY", "sk-ds-test")]);
    let plan = launch::plan(&request(Harness::Dsh, Some("deepseek"), &[]), &paths, &env).unwrap();
    assert_eq!(plan.program, PathBuf::from("dsh"));
    assert_eq!(plan.args[0], "--profile");
    assert_eq!(plan.args[1], "dsh-tui");
    assert_eq!(plan.args[2], "--patch");
    assert_eq!(
        Path::new(&plan.args[3]),
        paths.dir.join("dsh").join("deepseek").join("launch.cordis.yml")
    );
    assert_eq!(plan.env_set, vec![("DEEPSEEK_API_KEY".to_string(), "sk-ds-test".to_string())]);
    let patch =
        fs::read_to_string(paths.dir.join("dsh").join("deepseek").join("launch.cordis.yml"))
            .unwrap();
    assert!(patch.contains("id: settings"));
    assert!(!patch.contains("disabled: true"));
    let settings: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(paths.dir.join("dsh").join("deepseek").join("settings.yaml")).unwrap(),
    )
    .unwrap();
    assert!(settings["llm-pi-ai"]["providers"].as_mapping().unwrap().is_empty());
    assert_eq!(settings["agent-default-model"]["provider"], "deepseek-official");
}

#[test]
fn dsh_tokener_injects_provider_catalog() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) =
        serve_openai_models(r#"{"data":[{"id":"kimi-k3"},{"id":"gpt-5.6-sol"}]}"#);
    fs::write(&paths.config, format!("[provider.tokener]\nbase_url = \"{base_url}\"\n")).unwrap();
    let env = isolated(&[("TOKENER_API_KEY", "sk-tokener")]);
    let plan = launch::plan(&request(Harness::Dsh, Some("tokener"), &[]), &paths, &env).unwrap();
    server.join().unwrap();
    assert_eq!(plan.env_set, vec![("TOKENER_API_KEY".to_string(), "sk-tokener".to_string())]);
    let settings: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(paths.dir.join("dsh").join("tokener").join("settings.yaml")).unwrap(),
    )
    .unwrap();
    let models = settings["llm-pi-ai"]["providers"]["tokener"]["models"].as_sequence().unwrap();
    assert_eq!(settings["llm-pi-ai"]["providers"]["tokener"]["api"], "openai-completions");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["id"], "kimi-k3");
    assert_eq!(models[1]["id"], "gpt-5.6-sol");
    assert!(models[0].get("reasoningEfforts").is_none());
    assert!(models[1].get("reasoningEfforts").is_none());
    assert_eq!(settings["agent-default-model"]["provider"], "tokener");
    assert!(settings["agent-default-model"].get("model").is_none());
}

#[test]
fn dsh_overlays_are_isolated_per_provider() {
    let (_dir, paths) = temp_paths();
    let (tokener_url, tokener_server) = serve_openai_models(r#"{"data":[{"id":"kimi-k3"}]}"#);
    fs::write(
        &paths.config,
        format!(
            "[provider.tokener]\nbase_url = \"{tokener_url}\"\n\n[provider.openrouter]\nmodel = \"openai/gpt-5\"\n"
        ),
    )
    .unwrap();
    let env = isolated(&[("TOKENER_API_KEY", "sk-tokener"), ("OPENROUTER_API_KEY", "sk-or")]);
    let launch = |provider: &str| {
        launch::plan(&request(Harness::Dsh, Some(provider), &[]), &paths, &env).unwrap()
    };
    let tokener_plan = launch("tokener");
    tokener_server.join().unwrap();
    let openrouter_plan = launch("openrouter");
    assert_ne!(tokener_plan.args[3], openrouter_plan.args[3]);
    let read = |provider: &str| -> serde_yaml::Value {
        let overlay = paths.dir.join("dsh").join(provider).join("settings.yaml");
        serde_yaml::from_str(&fs::read_to_string(overlay).unwrap()).unwrap()
    };
    let tokener = read("tokener");
    let openrouter = read("openrouter");
    assert_eq!(tokener["agent-default-model"]["provider"], "tokener");
    assert_eq!(tokener["llm-pi-ai"]["providers"]["tokener"]["models"][0]["id"], "kimi-k3");
    assert!(tokener["llm-pi-ai"]["providers"].get("openrouter").is_none());
    assert_eq!(openrouter["agent-default-model"]["provider"], "openrouter");
    assert_eq!(
        openrouter["llm-pi-ai"]["providers"]["openrouter"]["models"][0]["id"],
        "openai/gpt-5"
    );
    assert!(openrouter["llm-pi-ai"]["providers"].get("tokener").is_none());
}

#[test]
fn dsh_tokener_launch_owns_settings_and_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let dsh_home = dir.path().join("dsh-home");
    fs::create_dir_all(&dsh_home).unwrap();
    fs::write(
        dsh_home.join("settings.yaml"),
        r#"
dsh-tui:
  lang: zh
permission:
  defaultPreset: danger-full-access
llm-pi-ai:
  providers:
    openrouter: { apiKeyEnv: OPENROUTER_API_KEY }
    kimi-coding: { baseURL: https://api.tokener.dev, apiKeyEnv: KIMI_CODING_API_KEY }
agent-default-model:
  provider: openrouter
  model: openrouter/auto
"#,
    )
    .unwrap();
    let (_recall, paths) = temp_paths();
    let (base_url, server) =
        serve_openai_models(r#"{"data":[{"id":"kimi-k3"},{"id":"gpt-5.6-sol"}]}"#);
    fs::write(
        &paths.config,
        format!("[provider.tokener]\nbase_url = \"{base_url}\"\nmodel = \"kimi-k3\"\n"),
    )
    .unwrap();
    let env =
        isolated(&[("TOKENER_API_KEY", "sk-tokener"), ("DSH_HOME", dsh_home.to_str().unwrap())]);
    let plan = launch::plan(&request(Harness::Dsh, Some("tokener"), &[]), &paths, &env).unwrap();
    server.join().unwrap();
    assert_eq!(plan.env_set, vec![("TOKENER_API_KEY".to_string(), "sk-tokener".to_string())]);
    let patch = fs::read_to_string(Path::new(&plan.args[3])).unwrap();
    assert!(patch.contains("id: settings"));
    assert!(patch.contains("id: llm-deepseek"));
    assert!(patch.contains("disabled: true"));
    let overlay = paths.dir.join("dsh").join("tokener").join("settings.yaml");
    let patch: serde_yaml::Value = serde_yaml::from_str(&patch).unwrap();
    assert_eq!(patch[0]["config"]["path"].as_str(), overlay.to_str());
    let settings: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&overlay).unwrap()).unwrap();
    assert_eq!(settings["dsh-tui"]["lang"], "zh");
    assert_eq!(settings["permission"]["defaultPreset"], "danger-full-access");
    let providers = settings["llm-pi-ai"]["providers"].as_mapping().unwrap();
    assert_eq!(providers.len(), 1);
    assert!(providers.get(serde_yaml::Value::from("kimi-coding")).is_none());
    assert_eq!(settings["llm-pi-ai"]["providers"]["tokener"]["apiKeyEnv"], "TOKENER_API_KEY");
    assert_eq!(settings["llm-pi-ai"]["providers"]["tokener"]["baseURL"], format!("{base_url}/v1"));
    let models = settings["llm-pi-ai"]["providers"]["tokener"]["models"].as_sequence().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["id"], "kimi-k3");
    assert_eq!(models[1]["id"], "gpt-5.6-sol");
    assert_eq!(settings["agent-default-model"]["provider"], "tokener");
    assert_eq!(settings["agent-default-model"]["model"], "kimi-k3");
    let original = fs::read_to_string(dsh_home.join("settings.yaml")).unwrap();
    assert!(original.contains("kimi-coding"));
}

#[test]
fn yolo_forces_dsh_preset_over_user_settings() {
    let dir = tempfile::tempdir().unwrap();
    let dsh_home = dir.path().join("dsh-home");
    fs::create_dir_all(&dsh_home).unwrap();
    fs::write(dsh_home.join("settings.yaml"), "permission:\n  defaultPreset: read-only\n").unwrap();
    let (_recall, paths) = temp_paths();
    let (base_url, server) = serve_openai_models(r#"{"data":[{"id":"kimi-k3"}]}"#);
    fs::write(&paths.config, format!("[provider.tokener]\nbase_url = \"{base_url}\"\n")).unwrap();
    let env =
        isolated(&[("TOKENER_API_KEY", "sk-tokener"), ("DSH_HOME", dsh_home.to_str().unwrap())]);
    let plan = launch::plan(&request(Harness::Dsh, Some("tokener"), &[]), &paths, &env).unwrap();
    server.join().unwrap();
    let overlay = paths.dir.join("dsh").join("tokener").join("settings.yaml");
    let settings: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&overlay).unwrap()).unwrap();
    assert_eq!(settings["permission"]["defaultPreset"], "danger-full-access");
    assert!(plan.stderr_note.as_deref().unwrap().contains("yolo"));
}
