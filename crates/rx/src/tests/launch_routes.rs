use super::*;

#[test]
fn unconfigured_launch_is_passthrough() {
    let (_dir, paths) = temp_paths();
    let plan =
        launch::plan(&request(Harness::Claude, None, &["--resume", "abc"]), &paths, &isolated(&[]))
            .unwrap();
    assert_eq!(plan.program, PathBuf::from("claude"));
    assert_eq!(plan.args, os(&["--resume", "abc"]));
    assert!(plan.env_set.is_empty());
    assert!(plan.stderr_note.as_deref().unwrap().contains("no provider configured"));
}

#[test]
fn unconfigured_dsh_still_boots_tui_profile() {
    let (_dir, paths) = temp_paths();
    let plan =
        launch::plan(&request(Harness::Dsh, None, &["--resume"]), &paths, &isolated(&[])).unwrap();
    assert_eq!(plan.program, PathBuf::from("dsh"));
    assert_eq!(plan.args, os(&["--profile", "dsh-tui", "--resume"]));
    assert!(plan.env_set.is_empty());
}

#[test]
fn unconfigured_kimi_launches_as_is() {
    let (_dir, paths) = temp_paths();
    let plan =
        launch::plan(&request(Harness::Kimi, None, &["web", "--no-open"]), &paths, &isolated(&[]))
            .unwrap();
    assert_eq!(plan.program, PathBuf::from("kimi"));
    assert_eq!(plan.args, os(&["web", "--no-open"]));
    assert!(plan.env_set.is_empty());
}

#[test]
fn kimi_explicit_model_seeds_selected_alias_when_catalog_is_unavailable() {
    let (_dir, paths) = temp_paths();
    fs::write(&paths.config, "[provider.tokener]\nbase_url = \"http://127.0.0.1:9\"\n").unwrap();
    let env = isolated(&[("TOKENER_API_KEY", "sk-tokener")]);
    let plan = launch::plan(
        &request(Harness::Kimi, Some("tokener"), &["--model", "rx-tokener/kimi-k3", "--plan"]),
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, PathBuf::from("kimi"));
    assert_eq!(plan.args, os(&["--auto", "--model", "rx-tokener/kimi-k3", "--plan"]));
    assert_eq!(plan.env_set, vec![("KIMI_MODEL_NAME".to_string(), String::new())]);
    let config: toml::Value = toml::from_str(
        &fs::read_to_string(paths.dir.join("kimi-code").join("config.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(config["providers"]["rx-tokener"]["type"].as_str(), Some("openai"));
    assert_eq!(
        config["providers"]["rx-tokener"]["base_url"].as_str(),
        Some("http://127.0.0.1:9/v1")
    );
    assert_eq!(config["models"]["rx-tokener/kimi-k3"]["model"].as_str(), Some("kimi-k3"));
    assert!(plan.stderr_note.as_deref().unwrap().contains("seeding only the selected model"));
    assert!(plan.stderr_note.as_deref().unwrap().contains("yolo"));
}

#[test]
fn kimi_fallback_uses_first_provider_model_and_reports_it() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models(r#"{"data":[{"id":"glm-5"},{"id":"kimi-k3"}]}"#);
    fs::write(&paths.config, format!("[provider.tokener]\nbase_url = \"{base_url}\"\n")).unwrap();
    let env = isolated(&[("TOKENER_API_KEY", "sk-tokener")]);
    let plan =
        launch::plan(&request(Harness::Kimi, Some("tokener"), &["--auto"]), &paths, &env).unwrap();
    server.join().unwrap();
    assert_eq!(plan.args, os(&["--model", "rx-tokener/glm-5", "--auto"]));
    assert_eq!(plan.env_set, vec![("KIMI_MODEL_NAME".to_string(), String::new())]);
    let config: toml::Value = toml::from_str(
        &fs::read_to_string(paths.dir.join("kimi-code").join("config.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(config["models"].as_table().unwrap().len(), 2);
    assert_eq!(config["models"]["rx-tokener/glm-5"]["model"].as_str(), Some("glm-5"));
    assert_eq!(config["models"]["rx-tokener/kimi-k3"]["model"].as_str(), Some("kimi-k3"));
    assert!(plan.stderr_note.as_deref().unwrap().contains("using first provider model 'glm-5'"));
    assert!(!plan.stderr_note.as_deref().unwrap().contains("yolo"));
}

#[test]
fn kimi_skips_yolo_when_prompt_or_hidden_yolo_alias_is_set() {
    let (_dir, paths) = temp_paths();
    fs::write(&paths.config, "[provider.tokener]\nbase_url = \"http://127.0.0.1:9\"\n").unwrap();
    let env = isolated(&[("TOKENER_API_KEY", "sk-tokener")]);
    for passthrough in [
        &["--model", "kimi-k3", "-p", "hello"].as_slice(),
        &["--model", "kimi-k3", "--prompt", "hello"].as_slice(),
        &["--model", "kimi-k3", "--yes"].as_slice(),
        &["--model", "kimi-k3", "--auto-approve"].as_slice(),
    ] {
        let plan =
            launch::plan(&request(Harness::Kimi, Some("tokener"), passthrough), &paths, &env)
                .unwrap();
        assert!(!plan.args.iter().any(|arg| arg == "--auto"), "{:?}", plan.args);
        assert!(!plan.stderr_note.as_deref().is_some_and(|note| note.contains("yolo")));
    }
}

#[test]
fn claude_openrouter_uses_api_key_and_discovery_fallback_without_seed() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan =
        launch::plan(&request(Harness::Claude, Some("openrouter"), &["fix it"]), &paths, &env)
            .unwrap();
    assert_eq!(plan.program, PathBuf::from("claude"));
    assert_eq!(plan.args, os(&["--dangerously-skip-permissions", "fix it"]));
    assert_env(
        &plan,
        &[
            ("ANTHROPIC_BASE_URL", "https://openrouter.ai/api"),
            ("ANTHROPIC_API_KEY", "sk-or-test"),
            ("ANTHROPIC_AUTH_TOKEN", ""),
            ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1"),
            ("OPENROUTER_API_KEY", "sk-or-test"),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL", "~anthropic/claude-sonnet-latest"),
            ("ANTHROPIC_MODEL", "~anthropic/claude-sonnet-latest"),
        ],
    );
    assert!(plan.stderr_note.as_deref().unwrap().contains("catalog seed failed"));
}

#[test]
fn configured_openrouter_credential_is_the_implicit_default() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan = launch::plan(&request(Harness::Codex, None, &[]), &paths, &env).unwrap();
    assert_eq!(plan.args[1], "model_provider=\"openrouter\"");
}

#[test]
fn provider_none_skips_injection() {
    let (_dir, paths) = temp_paths();
    config::login(&paths, "openrouter", "sk-secret".to_string()).unwrap();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan =
        launch::plan(&request(Harness::Claude, Some("none"), &["--resume", "abc"]), &paths, &env)
            .unwrap();
    assert_eq!(plan.args, os(&["--resume", "abc"]));
    assert!(plan.env_set.is_empty());
    let note = plan.stderr_note.unwrap();
    assert!(note.contains("'none'"), "{note}");
    assert!(!note.contains("no provider configured"), "{note}");
}

#[test]
fn use_none_skips_implicit_openrouter() {
    let (_dir, paths) = temp_paths();
    config::login(&paths, "openrouter", "sk-secret".to_string()).unwrap();
    crate::providers::run(
        ProvidersCommand::Use { provider: Some("none".to_string()) },
        &paths,
        &isolated(&[]),
    )
    .unwrap();
    assert_eq!(config::load(&paths).unwrap().unwrap().default_provider.as_deref(), Some("none"));
    assert!(config::stored_providers(&paths).unwrap().contains("openrouter"));

    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan = launch::plan(&request(Harness::Codex, None, &[]), &paths, &env).unwrap();
    assert!(plan.env_set.is_empty());
    assert!(plan.args.iter().all(|arg| arg != "model_provider=\"openrouter\""));
    let note = plan.stderr_note.unwrap();
    assert!(note.contains("'none'"), "{note}");
    assert!(!note.contains("no provider configured"), "{note}");
}

#[test]
fn models_update_rejects_none_provider() {
    let (_dir, paths) = temp_paths();
    let error = crate::providers::run(
        ProvidersCommand::ModelsUpdate { provider: Some("none".to_string()) },
        &paths,
        &isolated(&[]),
    )
    .unwrap_err();
    assert!(error.to_string().contains("skips provider injection"), "{error}");
}

#[test]
fn provider_override_wins_over_use_none() {
    let (_dir, paths) = temp_paths();
    config::set_none(&paths).unwrap();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan =
        launch::plan(&request(Harness::Codex, Some("openrouter"), &[]), &paths, &env).unwrap();
    assert_eq!(plan.args[1], "model_provider=\"openrouter\"");
}

#[test]
fn none_is_reserved_provider_id() {
    let error = provider::validate_id("none").unwrap_err();
    assert!(error.to_string().contains("reserved"), "{error}");
    let error = provider::resolve("none", None).unwrap_err();
    assert!(error.to_string().contains("reserved"), "{error}");
}

#[test]
fn claude_injection_tokener_still_uses_auth_token() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("TOKENER_API_KEY", "sk-tokener")]);
    let plan = launch::plan(&request(Harness::Claude, Some("tokener"), &["fix it"]), &paths, &env)
        .unwrap();
    assert_env(&plan, &[("ANTHROPIC_AUTH_TOKEN", "sk-tokener"), ("ANTHROPIC_API_KEY", "")]);
}

#[test]
fn codex_openrouter_overrides_model_and_uses_command_auth() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan =
        launch::plan(&request(Harness::Codex, Some("openrouter"), &[]), &paths, &env).unwrap();
    assert_eq!(plan.program, PathBuf::from("codex"));
    assert_eq!(plan.args[0], "-c");
    assert_eq!(plan.args[1], "model_provider=\"openrouter\"");
    assert!(arg_str(&plan.args[3]).contains("base_url=\"https://openrouter.ai/api/v1\""));
    assert!(arg_str(&plan.args[3]).contains("wire_api=\"responses\""));
    assert!(arg_str(&plan.args[3]).contains("supports_websockets=false"));
    assert!(!arg_str(&plan.args[3]).contains("env_key="));
    assert_eq!(plan.args[5], "model=\"~openai/gpt-latest\"");
    assert_eq!(plan.env_set, vec![("OPENROUTER_API_KEY".to_string(), "sk-or-test".to_string())]);
}

#[test]
fn codex_injected_values_round_trip_through_toml() {
    let (_dir, paths) = temp_paths();
    let mut provider = provider::find("tokener").unwrap().clone();
    provider.name = "Gateway \"Beta\"\nC:\\gateway".to_string();
    provider.endpoint = "http://127.0.0.1:1/\"quoted\\path".to_string();
    provider.env = "KEY\"'$(exit 9)\\VALUE".to_string();
    let model = "vendor/\"model\\name\nnext";
    let target = launch::ProviderTarget {
        provider: provider.clone(),
        key: "synthetic-value".to_string(),
        model: Some(model.to_string()),
    };
    let plan = launch::plan_target(
        &request(Harness::Codex, None, &[]),
        &paths,
        &isolated(&[("RX_NO_YOLO", "1")]),
        &target,
    )
    .unwrap();
    let config: toml::Value = arg_str(&plan.args[3]).parse().unwrap();
    let injected = &config["model_providers"]["tokener"];
    assert_eq!(injected["name"].as_str(), Some(provider.name.as_str()));
    assert_eq!(
        injected["base_url"].as_str(),
        Some(catalog::openai_base(&provider.endpoint).as_str())
    );
    let selection: toml::Value = arg_str(&plan.args[5]).parse().unwrap();
    assert_eq!(selection["model"].as_str(), Some(model));
    #[cfg(unix)]
    {
        let auth = &injected["auth"];
        let output = std::process::Command::new(auth["command"].as_str().unwrap())
            .args(auth["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()))
            .env_clear()
            .env(&provider.env, &target.key)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim_end(), target.key);
    }
}

#[test]
fn codex_catalog_override_preserves_the_generated_path() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = if cfg!(unix) { "quoted\"\\state" } else { "catalog state" };
    let paths = Paths::in_dir(dir.path().join(state_dir));
    let (endpoint, server) = serve_openai_models(r#"{"data":[{"id":"test-model"}]}"#);
    let mut provider = provider::find("tokener").unwrap().clone();
    provider.endpoint = endpoint;
    let plan = launch::plan_target(
        &request(Harness::Codex, None, &[]),
        &paths,
        &EnvLookup::real_with(HashMap::from([("RX_NO_YOLO".to_string(), "1".to_string())])),
        &launch::ProviderTarget { provider, key: "synthetic-key".to_string(), model: None },
    )
    .unwrap();
    server.join().unwrap();
    let override_arg =
        plan.args.iter().find(|arg| arg_str(arg).starts_with("model_catalog_json=")).unwrap();
    let config: toml::Value = arg_str(override_arg).parse().unwrap();
    let catalog = PathBuf::from(config["model_catalog_json"].as_str().unwrap());
    assert_eq!(catalog, paths.dir.join("catalogs/tokener.json"));
    assert!(catalog.is_file());
}

#[cfg(unix)]
#[test]
fn exec_scopes_inherited_controls_and_explicit_credentials() {
    const CONTROLS: [&str; 4] = ["RX_HOST_REQUEST", "RX_NO_INSTALL", "RX_NO_UPDATE", "RX_NO_YOLO"];
    if let Ok(credential) = std::env::var("RX_TEST_EXEC_CREDENTIAL") {
        let plan = launch::LaunchPlan {
            launch_lease: None,
            program: PathBuf::from("/usr/bin/env"),
            args: Vec::new(),
            env_set: if credential.is_empty() {
                Vec::new()
            } else {
                vec![(credential, "selected-credential".to_string())]
            },
            stderr_note: None,
        };
        launch::exec(&plan).unwrap();
        unreachable!();
    }
    for credential in std::iter::once("").chain(CONTROLS) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "tests::launch_routes::exec_scopes_inherited_controls_and_explicit_credentials",
                "--nocapture",
            ])
            .env_clear()
            .env("RX_TEST_EXEC_CREDENTIAL", credential)
            .env("NATIVE_SENTINEL", "preserved");
        for control in CONTROLS {
            child.env(control, "parent-control");
        }
        let output = child.output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.lines().any(|line| line == "NATIVE_SENTINEL=preserved"));
        for control in CONTROLS {
            let entry = stdout.lines().find(|line| line.starts_with(&format!("{control}=")));
            let expected = format!("{control}=selected-credential");
            assert_eq!(entry, (control == credential).then_some(expected.as_str()));
        }
    }
}

#[test]
fn opencode_openrouter_injects_config_and_model() {
    let (_dir, paths) = temp_paths();
    fs::write(&paths.config, "[provider.openrouter]\nmodel = \"openai/gpt-5\"\n").unwrap();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan = launch::plan(
        &request(Harness::OpenCode, Some("openrouter"), &["run", "hello"]),
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, PathBuf::from("opencode"));
    assert_eq!(plan.args, os(&["run", "--auto", "-m", "openrouter/openai/gpt-5", "hello"]));
    assert_env(&plan, &[("OPENROUTER_API_KEY", "sk-or-test")]);
    let config = plan
        .env_set
        .iter()
        .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
        .map(|(_, v)| v.as_str())
        .unwrap();
    assert!(config.contains("openrouter"));
    assert!(config.contains("OPENROUTER_API_KEY"));
}

#[test]
fn opencode_server_commands_preserve_native_arguments() {
    let (_dir, paths) = temp_paths();
    fs::write(&paths.config, "[provider.openrouter]\nmodel = \"openai/gpt-5\"\n").unwrap();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    for passthrough in
        [&["web", "--port", "4096"].as_slice(), &["serve", "--hostname", "127.0.0.1"].as_slice()]
    {
        let plan = launch::plan(
            &request(Harness::OpenCode, Some("openrouter"), passthrough),
            &paths,
            &env,
        )
        .unwrap();
        assert_eq!(plan.args, os(passthrough));
        assert!(plan.env_set.iter().any(|(key, _)| key == "OPENCODE_CONFIG_CONTENT"));
        assert!(!plan.stderr_note.as_deref().is_some_and(|note| note.contains("yolo")));
    }
}

#[test]
fn opencode_non_generated_catalog_failure_degrades_to_base_config() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_error(500);
    let provider = provider::find("openrouter").unwrap();
    let config =
        crate::opencode::config_content("openrouter", provider, &base_url, "sk-test", &paths, true)
            .unwrap();
    server.join().unwrap();
    let document: Value = serde_json::from_str(&config).unwrap();
    assert_eq!(document["provider"]["openrouter"]["options"]["baseURL"], format!("{base_url}/v1"));
    assert!(document["provider"]["openrouter"].get("models").is_none());
}

#[test]
fn yolo_respects_user_permission_flag() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan = launch::plan(
        &request(Harness::Claude, Some("openrouter"), &["--permission-mode=acceptEdits", "fix it"]),
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.args, os(&["--permission-mode=acceptEdits", "fix it"]));
    assert!(!plan.stderr_note.as_deref().unwrap_or_default().contains("yolo"));

    let plan = launch::plan(
        &request(
            Harness::Codex,
            Some("openrouter"),
            &["--sandbox=read-only", "--ask-for-approval=on-request", "exec", "cargo test"],
        ),
        &paths,
        &env,
    )
    .unwrap();
    assert!(!plan.args.iter().any(|arg| arg == "--sandbox"));
    assert!(!plan.args.iter().any(|arg| arg == "--ask-for-approval"));
    assert!(!plan.stderr_note.as_deref().unwrap_or_default().contains("yolo"));
}

#[test]
fn rx_no_yolo_disables_injection() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test"), ("RX_NO_YOLO", "1")]);
    let plan =
        launch::plan(&request(Harness::Claude, Some("openrouter"), &["fix it"]), &paths, &env)
            .unwrap();
    assert_eq!(plan.args, os(&["fix it"]));
    assert!(!plan.stderr_note.as_deref().unwrap_or_default().contains("yolo"));
}

#[test]
fn codex_passthrough_model_flag_wins() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan = launch::plan(
        &request(Harness::Codex, Some("openrouter"), &["--model", "anthropic/claude-sonnet-4.6"]),
        &paths,
        &env,
    )
    .unwrap();
    assert!(!plan.args.iter().any(|arg| arg_str(arg).starts_with("model=")));
    assert_eq!(plan.args[plan.args.len() - 2], "--model");
    assert_eq!(plan.args[plan.args.len() - 1], "anthropic/claude-sonnet-4.6");
}

#[test]
fn codex_tokener_does_not_invent_a_model() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("TOKENER_API_KEY", "sk-tokener")]);
    let plan = launch::plan(
        &request(Harness::Codex, Some("tokener"), &["exec", "cargo test"]),
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.args[1], "model_provider=\"tokener\"");
    assert!(arg_str(&plan.args[3]).contains("base_url=\"https://api.tokener.dev/v1\""));
    assert!(!plan.args.iter().any(|arg| arg_str(arg).starts_with("model=")));
    assert_eq!(
        &plan.args[4..8],
        os(&["--sandbox", "danger-full-access", "--ask-for-approval", "never"])
    );
    assert_eq!(&plan.args[8..], os(&["exec", "cargo test"]));
}

#[test]
fn isolated_codex_plan_does_not_write_model_catalog_json() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("TOKENER_API_KEY", "sk-tokener")]);
    let plan =
        launch::plan(&request(Harness::Codex, Some("tokener"), &["exec"]), &paths, &env).unwrap();
    assert!(!plan.args.iter().any(|arg| arg_str(arg).contains("model_catalog_json")));
    assert!(!paths.dir.join("catalogs").exists());
}
