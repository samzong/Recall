use super::*;

#[test]
fn completion_ids_list_configured_and_known() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[]);
    let known = crate::providers::completion_ids(&paths, &env, ProviderIdFilter::All).unwrap();
    assert!(known.contains(&"openrouter".to_string()), "{known:?}");
    assert!(known.contains(&"tokener".to_string()), "{known:?}");
    assert!(
        crate::providers::completion_ids(&paths, &env, ProviderIdFilter::Configured)
            .unwrap()
            .is_empty()
    );

    config::login(&paths, "openrouter", "sk-secret".to_string()).unwrap();
    assert_eq!(
        crate::providers::completion_ids(&paths, &env, ProviderIdFilter::Configured).unwrap(),
        vec!["openrouter".to_string()]
    );
    let mut targets =
        crate::providers::completion_ids(&paths, &env, ProviderIdFilter::Targets).unwrap();
    assert_eq!(targets.pop(), Some("none".to_string()));
    assert_eq!(targets, vec!["openrouter".to_string()]);
    for id in known {
        assert_ne!(id, "none");
    }
}

#[test]
fn available_providers_include_only_complete_custom_entries_once() {
    let config: crate::config::RxConfig = toml::from_str(
        r#"
[provider.openrouter]
base_url = "https://proxy.example.com/v1"

[provider.complete]
base_url = "https://complete.example.com/v1"

[provider.incomplete]
env = "INCOMPLETE_API_KEY"
"#,
    )
    .unwrap();
    let available = provider::available(&config).unwrap();
    assert_eq!(available.iter().filter(|provider| provider.id == "openrouter").count(), 1);
    assert!(available.iter().any(|provider| provider.id == "complete"));
    assert!(!available.iter().any(|provider| provider.id == "incomplete"));
}

#[test]
fn bundled_providers_match_the_admission_list() {
    let admission: Value =
        serde_json::from_str(include_str!("../../data/provider-admission.json")).unwrap();
    let models_dev = entries(&admission, "models_dev_ids")
        .iter()
        .map(|entry| entry.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let managed = entries(&admission, "managed")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let admitted = models_dev
        .iter()
        .take(1)
        .chain(managed.iter().take(1))
        .chain(models_dev.iter().skip(1))
        .chain(managed.iter().skip(1))
        .cloned()
        .collect::<Vec<_>>();
    let bundled =
        provider::catalog().iter().map(|provider| provider.id.clone()).collect::<Vec<_>>();
    assert_eq!(bundled, admitted);
    assert_eq!(bundled.first().map(String::as_str), Some("openrouter"));
    assert_eq!(bundled.get(1).map(String::as_str), Some("tokener"));
    let deepseek = provider::find("deepseek").unwrap();
    assert_eq!(deepseek.endpoint, "https://api.deepseek.com");
    assert_eq!(deepseek.env, "DEEPSEEK_API_KEY");
    assert_eq!(deepseek.anthropic_base.as_deref(), Some("https://api.deepseek.com/anthropic"));
    assert_eq!(deepseek.default_context, Some(1_000_000));
    assert_eq!(provider::claude_base(deepseek), "https://api.deepseek.com/anthropic");
    let moonshot = provider::find("moonshotai").unwrap();
    assert_eq!(moonshot.endpoint, "https://api.moonshot.ai/v1");
    assert_eq!(moonshot.anthropic_base.as_deref(), Some("https://api.moonshot.ai/anthropic"));
    let minimax = provider::find("minimax").unwrap();
    assert_eq!(minimax.endpoint, "https://api.minimax.io/v1");
    assert_eq!(minimax.anthropic_base.as_deref(), Some("https://api.minimax.io/anthropic"));
    assert_eq!(provider::claude_base(minimax), "https://api.minimax.io/anthropic");
    let siliconflow = provider::find("siliconflow").unwrap();
    assert_eq!(siliconflow.endpoint, "https://api.siliconflow.com/v1");
    assert_eq!(siliconflow.env, "SILICONFLOW_API_KEY");
    assert_eq!(siliconflow.anthropic_base, None);
    assert_eq!(provider::claude_base(siliconflow), "https://api.siliconflow.com");
    let zai = provider::find("zai").unwrap();
    assert_eq!(zai.endpoint, "https://api.z.ai/api/paas/v4");
    assert_eq!(zai.env, "ZHIPU_API_KEY");
    assert_eq!(zai.anthropic_base.as_deref(), Some("https://api.z.ai/api/anthropic"));
    assert_eq!(provider::claude_base(zai), "https://api.z.ai/api/anthropic");
    assert_eq!(catalog::openai_base(&zai.endpoint), "https://api.z.ai/api/paas/v4");
    let zhipu = provider::find("zhipuai").unwrap();
    assert_eq!(zhipu.endpoint, "https://open.bigmodel.cn/api/paas/v4");
    assert_eq!(zhipu.anthropic_base.as_deref(), Some("https://open.bigmodel.cn/api/anthropic"));
    for provider in provider::catalog() {
        provider::validate_id(&provider.id).unwrap();
        assert!(provider.endpoint.starts_with("https://"), "{}", provider.id);
        assert!(!provider.endpoint.contains("${"), "{}", provider.id);
        assert!(!provider.env.is_empty(), "{}", provider.id);
        if let Some(claude) = &provider.anthropic_base {
            assert!(claude.starts_with("https://"), "{}", provider.id);
            assert!(!claude.contains("${"), "{}", provider.id);
        }
        if let Some(context) = provider.default_context {
            assert!(context > 0, "{}", provider.id);
        }
    }
}

#[test]
fn provider_login_and_logout_store_no_driver_or_catalog_endpoint() {
    let (_dir, paths) = temp_paths();
    config::login(&paths, "openrouter", "sk-secret".to_string()).unwrap();

    let config = config::load(&paths).unwrap().unwrap();
    assert_eq!(config.default_provider.as_deref(), Some("openrouter"));
    assert!(config::stored_providers(&paths).unwrap().contains("openrouter"));
    let serialized = fs::read_to_string(&paths.config).unwrap();
    assert!(!serialized.contains("driver"));
    assert!(!serialized.contains("base_url"));

    assert!(config::logout(&paths, "openrouter").unwrap());
    let config = config::load(&paths).unwrap().unwrap();
    assert_eq!(config.default_provider, None);
    assert!(!config::stored_providers(&paths).unwrap().contains("openrouter"));
}

#[test]
fn missing_key_errors_before_exec() {
    let (_dir, paths) = temp_paths();
    let error =
        launch::plan(&request(Harness::Claude, Some("openrouter"), &[]), &paths, &isolated(&[]))
            .unwrap_err();
    assert_eq!(
        error.to_string(),
        "no API key for provider 'openrouter'; run: rx providers login openrouter (or set $OPENROUTER_API_KEY)"
    );
}

#[test]
fn custom_provider_does_not_inherit_catalog_env_key() {
    let (_dir, paths) = temp_paths();
    fs::write(
        &paths.config,
        r#"default_provider = "tokener-dev"

[provider.tokener-dev]
base_url = "https://dev.provider.test"
"#,
    )
    .unwrap();
    let env = isolated(&[("TOKENER_API_KEY", "sk-prod")]);

    let error = launch::plan(&request(Harness::Codex, None, &[]), &paths, &env).unwrap_err();

    assert!(error.to_string().contains("no API key for provider 'tokener-dev'"), "{error}");
}

#[test]
fn provider_login_stores_secret_permissions_and_launches() {
    let (_dir, paths) = temp_paths();
    config::login(&paths, "openrouter", "sk-secret".to_string()).unwrap();

    let loaded = config::load(&paths).unwrap().unwrap();
    assert_eq!(loaded.default_provider.as_deref(), Some("openrouter"));
    assert_eq!(loaded.provider["openrouter"].auth, config::AuthMode::ApiKey);

    let stored = fs::read_to_string(&paths.keys).unwrap();
    assert!(stored.contains("sk-secret"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&paths.keys).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(&paths.dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    let env = isolated(&[]);
    let plan = launch::plan(&request(Harness::Claude, None, &[]), &paths, &env).unwrap();
    assert_eq!(plan.env_set[1], ("ANTHROPIC_API_KEY".to_string(), "sk-secret".to_string()));
}

#[test]
fn provider_login_does_not_switch_auth_when_keys_cannot_be_loaded() {
    let (_dir, paths) = temp_paths();
    fs::write(
        &paths.config,
        r#"default_provider = "openrouter"

[provider.openrouter]
base_url = "https://openrouter.ai/api"
auth = "env"
"#,
    )
    .unwrap();
    fs::write(&paths.keys, "{").unwrap();

    let error = config::login(&paths, "openrouter", "sk-secret".to_string()).unwrap_err();
    assert!(error.to_string().contains("failed to parse"), "{error}");

    let config = config::load(&paths).unwrap().unwrap();
    assert_eq!(config.provider["openrouter"].auth, config::AuthMode::Env);
}

#[test]
fn overlapping_provider_mutations_preserve_distinct_updates() {
    let (_dir, paths) = temp_paths();
    let ids = (0..24).map(|index| format!("custom-{index}")).collect::<Vec<_>>();
    fs::write(
        &paths.config,
        ids.iter()
            .map(|id| {
                format!(
                    "[provider.{id}]\nbase_url = \"https://provider.test/v1\"\nauth = \"env\"\n"
                )
            })
            .collect::<String>(),
    )
    .unwrap();
    fs::write(
        &paths.keys,
        ids[12..].iter().map(|id| format!("{id} = \"old-key\"\n")).collect::<String>(),
    )
    .unwrap();
    let barrier = std::sync::Barrier::new(ids.len() + 2);
    thread::scope(|scope| {
        for (index, id) in ids.iter().enumerate() {
            let paths = &paths;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                if index < 12 {
                    config::login(paths, id, format!("key-{id}")).unwrap();
                } else {
                    assert!(config::logout(paths, id).unwrap());
                }
            });
        }
        scope.spawn(|| {
            barrier.wait();
            config::set_default(&paths, &ids[0]).unwrap();
        });
        scope.spawn(|| {
            barrier.wait();
            config::set_none(&paths).unwrap();
        });
    });
    let loaded = config::load_or_default(&paths).unwrap();
    assert_eq!(loaded.provider.len(), ids.len());
    for (index, id) in ids.iter().enumerate() {
        assert_eq!(
            config::stored_key(&paths, id).unwrap(),
            (index < 12).then(|| format!("key-{id}"))
        );
        assert_eq!(
            loaded.provider[id].auth,
            if index < 12 { config::AuthMode::ApiKey } else { config::AuthMode::Env }
        );
        assert_eq!(loaded.provider[id].base_url.as_deref(), Some("https://provider.test/v1"));
    }
}

#[test]
fn provider_default_selection_preserves_env_auth() {
    let (_dir, paths) = temp_paths();
    fs::write(
        &paths.config,
        r#"[provider.custom]
base_url = "https://provider.test/v1"
env = "CUSTOM_API_KEY"
auth = "env"
"#,
    )
    .unwrap();

    config::set_default(&paths, "custom").unwrap();

    let loaded = config::load(&paths).unwrap().unwrap();
    assert_eq!(loaded.default_provider.as_deref(), Some("custom"));
    assert_eq!(loaded.provider["custom"].auth, config::AuthMode::Env);
    assert!(!paths.keys.exists());
}

#[test]
fn provider_default_selection_does_not_create_auth_config() {
    let (_dir, paths) = temp_paths();

    config::set_default(&paths, "openrouter").unwrap();

    let loaded = config::load(&paths).unwrap().unwrap();
    assert_eq!(loaded.default_provider.as_deref(), Some("openrouter"));
    assert!(!loaded.provider.contains_key("openrouter"));
}

#[test]
fn provider_use_argument_sets_default_without_a_terminal() {
    let (_dir, paths) = temp_paths();
    config::login(&paths, "openrouter", "sk-openrouter".to_string()).unwrap();
    config::login(&paths, "tokener-dev", "sk-dev".to_string()).unwrap();

    crate::run_with(os(&["rx", "providers", "use", "openrouter"]), &paths, &isolated(&[])).unwrap();

    let loaded = config::load(&paths).unwrap().unwrap();
    assert_eq!(loaded.default_provider.as_deref(), Some("openrouter"));
}

#[test]
fn provider_logout_argument_removes_key_without_a_terminal() {
    let (_dir, paths) = temp_paths();
    config::login(&paths, "tokener-dev", "sk-dev".to_string()).unwrap();

    crate::run_with(os(&["rx", "providers", "logout", "tokener-dev"]), &paths, &isolated(&[]))
        .unwrap();

    assert!(config::stored_key(&paths, "tokener-dev").unwrap().is_none());
}

#[test]
fn providers_keep_independent_keys_and_behavior() {
    let (dir, paths) = temp_paths();
    let (dev_base_url, server) = serve_openai_models(r#"{"data":[{"id":"gpt-dev"}]}"#);
    fs::write(
        &paths.config,
        format!(
            r#"default_provider = "tokener-dev"

[provider.tokener-dev]
base_url = "{dev_base_url}"
model = "gpt-dev"

[provider.tokener-prod]
base_url = "https://prod.provider.test"
model = "gpt-prod"
"#
        ),
    )
    .unwrap();

    config::login(&paths, "tokener-prod", "sk-prod".to_string()).unwrap();
    config::login(&paths, "tokener-dev", "sk-dev".to_string()).unwrap();
    config::set_default(&paths, "tokener-prod").unwrap();

    let loaded = config::load(&paths).unwrap().unwrap();
    assert_eq!(loaded.default_provider.as_deref(), Some("tokener-prod"));
    assert_eq!(loaded.provider["tokener-dev"].base_url.as_deref(), Some(dev_base_url.as_str()));
    assert_eq!(
        loaded.provider["tokener-prod"].base_url.as_deref(),
        Some("https://prod.provider.test")
    );

    let agent_dir = dir.path().join("pi-agent");
    let env = isolated(&[("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())]);
    let codex =
        launch::plan(&request(Harness::Codex, Some("tokener-dev"), &[]), &paths, &env).unwrap();
    assert_eq!(codex.args[1], "model_provider=\"tokener-dev\"");
    assert!(arg_str(&codex.args[3]).contains("model_providers.tokener-dev="));
    assert!(arg_str(&codex.args[3]).contains(&format!("base_url=\"{dev_base_url}/v1\"")));
    assert_eq!(codex.env_set, vec![("TOKENER_DEV_API_KEY".to_string(), "sk-dev".to_string())]);

    let opencode =
        launch::plan(&request(Harness::OpenCode, Some("tokener-dev"), &[]), &paths, &env).unwrap();
    assert_eq!(opencode.args, ["--auto", "-m", "tokener-dev/gpt-dev"]);
    let opencode_config = opencode
        .env_set
        .iter()
        .find(|(name, _)| name == "OPENCODE_CONFIG_CONTENT")
        .map(|(_, value)| serde_json::from_str::<Value>(value).unwrap())
        .unwrap();
    assert!(opencode_config["provider"]["tokener-dev"].is_object());
    assert!(opencode_config["provider"]["tokener"].is_null());

    let pi = launch::plan(&request(Harness::Pi, Some("tokener-dev"), &[]), &paths, &env).unwrap();
    assert_eq!(pi.args, ["--models", "tokener-dev/*", "--model", "tokener-dev/gpt-dev"]);
    let pi_models: Value = read_json(agent_dir.join("models.json"));
    assert!(pi_models["providers"]["tokener-dev"].is_object());
    assert!(pi_models["providers"]["tokener"].is_null());
    server.join().unwrap();

    let claude =
        launch::plan(&request(Harness::Claude, Some("tokener-prod"), &[]), &paths, &env).unwrap();
    assert_env(
        &claude,
        &[
            ("ANTHROPIC_BASE_URL", "https://prod.provider.test"),
            ("ANTHROPIC_AUTH_TOKEN", "sk-prod"),
        ],
    );
}

#[test]
fn provider_names_cannot_escape_generated_config_boundaries() {
    let (_dir, paths) = temp_paths();
    fs::write(
        &paths.config,
        r#"default_provider = "../prod"

[provider."../prod"]
base_url = "https://provider.test/v1"
"#,
    )
    .unwrap();

    let error = launch::plan(&request(Harness::Pi, None, &[]), &paths, &isolated(&[])).unwrap_err();
    assert!(error.to_string().contains("invalid provider name '../prod'"), "{error}");
}
