use super::*;

#[test]
fn url_helpers_strip_or_add_v1() {
    assert_eq!(
        catalog::anthropic_base("https://openrouter.ai/api/v1/"),
        "https://openrouter.ai/api"
    );
    assert_eq!(catalog::openai_base("https://provider.test"), "https://provider.test/v1");
    assert_eq!(
        catalog::openai_base("https://openrouter.ai/api/v1"),
        "https://openrouter.ai/api/v1"
    );
    assert_eq!(
        catalog::openai_base("https://api.z.ai/api/paas/v4"),
        "https://api.z.ai/api/paas/v4"
    );
    assert_eq!(
        catalog::openai_base("https://api.z.ai/api/coding/paas/v4/"),
        "https://api.z.ai/api/coding/paas/v4"
    );
}

#[test]
fn claude_base_uses_explicit_anthropic_origin() {
    let openrouter = provider::find("openrouter").unwrap();
    assert_eq!(provider::claude_base(openrouter), "https://openrouter.ai/api");

    let override_entry = crate::config::ProviderConfig {
        anthropic_base: Some("https://api.deepseek.com/anthropic/v1".to_string()),
        ..crate::config::ProviderConfig::default()
    };
    let overridden = provider::resolve("openrouter", Some(&override_entry)).unwrap();
    assert_eq!(provider::claude_base(&overridden), "https://api.deepseek.com/anthropic");
    assert_eq!(overridden.endpoint, openrouter.endpoint);

    let (_dir, paths) = temp_paths();
    fs::write(
        &paths.config,
        r#"default_provider = "moonshot"

[provider.moonshot]
base_url = "https://api.moonshot.ai/v1"
anthropic_base = "https://api.moonshot.ai/anthropic"
"#,
    )
    .unwrap();
    config::login(&paths, "moonshot", "sk-moon".to_string()).unwrap();
    let plan =
        launch::plan(&request(Harness::Claude, Some("moonshot"), &[]), &paths, &isolated(&[]))
            .unwrap();
    assert_env(&plan, &[("ANTHROPIC_BASE_URL", "https://api.moonshot.ai/anthropic")]);
    let codex =
        launch::plan(&request(Harness::Codex, Some("moonshot"), &[]), &paths, &isolated(&[]))
            .unwrap();
    assert!(arg_str(&codex.args[3]).contains("base_url=\"https://api.moonshot.ai/v1\""));
}

#[test]
fn base_url_override_clears_bundled_anthropic_base() {
    let deepseek = provider::find("deepseek").unwrap();
    assert_eq!(provider::claude_base(deepseek), "https://api.deepseek.com/anthropic");

    let override_entry = crate::config::ProviderConfig {
        base_url: Some("https://proxy.example.com/v1".to_string()),
        ..crate::config::ProviderConfig::default()
    };
    let overridden = provider::resolve("deepseek", Some(&override_entry)).unwrap();
    assert_eq!(overridden.endpoint, "https://proxy.example.com/v1");
    assert_eq!(provider::claude_base(&overridden), "https://proxy.example.com");
    assert_eq!(overridden.env, deepseek.env);
}

#[test]
fn openai_data_becomes_codex_model_catalog_json() {
    let models = catalog::parse_openai_models(
        r#"{"data":[{"id":"gpt-5.6-sol","name":"GPT 5.6 Sol"},{"id":"claude-sonnet-5"}]}"#,
    )
    .unwrap();
    let catalog = catalog::synthesize_codex_catalog(&models);
    assert_eq!(catalog["models"][0]["slug"], "gpt-5.6-sol");
    assert_eq!(catalog["models"][0]["display_name"], "GPT 5.6 Sol");
    assert_eq!(catalog["models"][0]["visibility"], "list");
    assert_eq!(catalog["models"][0]["supported_in_api"], true);
    assert_eq!(catalog["models"][0]["context_window"], 200_000);
    assert_eq!(catalog["models"][0]["supported_reasoning_levels"], json!([]));
    assert_eq!(catalog["models"][0]["shell_type"], "shell_command");
    assert_eq!(catalog["models"][0]["priority"], 1);
    assert_eq!(catalog["models"][0]["truncation_policy"]["mode"], "bytes");
    assert_eq!(catalog["models"][0]["base_instructions"], "");
    assert_eq!(catalog["models"][1]["slug"], "claude-sonnet-5");
    assert_eq!(catalog["models"][1]["display_name"], "claude-sonnet-5");
}

#[test]
fn openai_data_without_context_still_seeds_claude_picker() {
    let models = catalog::parse_openai_models(r#"{"data":[{"id":"openai/gpt-5.6-sol"}]}"#).unwrap();
    let seed = claude::seed_from_listed("lab", &models);
    assert_eq!(seed.provider_id, "lab");
    assert_eq!(seed.additional_model_options.len(), 1);
    assert_eq!(seed.additional_model_options[0].value, "openai/gpt-5.6-sol");
}

#[test]
fn openai_body_without_context_falls_back_to_listed_claude_seed() {
    let body = r#"{"data":[{"id":"deepseek-v4-flash"},{"id":"deepseek-v4-pro"}]}"#;
    let models = catalog::parse_openai_models(body).unwrap();
    let seed = claude::seed_from_openai_body("deepseek", body, &models);
    assert_eq!(seed.provider_id, "deepseek");
    assert_eq!(seed.additional_model_options.len(), 2);
    assert_eq!(seed.additional_model_options[0].value, "deepseek-v4-flash");
    assert_eq!(seed.additional_model_options[0].description, "1M context");
    assert_eq!(seed.additional_model_options[1].value, "deepseek-v4-pro");
    assert_eq!(seed.auto_compact_windows["deepseek-v4-flash"], 1_000_000);
}

#[test]
fn openai_body_fills_omitted_context_among_models_with_windows() {
    let body = r#"{"data":[{"id":"with-ctx","context_length":200000},{"id":"no-ctx"}]}"#;
    let models = catalog::parse_openai_models(body).unwrap();
    let seed = claude::seed_from_openai_body("lab", body, &models);
    let values = seed
        .additional_model_options
        .iter()
        .map(|option| option.value.as_str())
        .collect::<Vec<_>>();
    assert_eq!(values, ["with-ctx", "no-ctx"]);
    assert_eq!(seed.auto_compact_windows["no-ctx"], 200_000);
    assert_eq!(
        seed.additional_model_options
            .iter()
            .find(|option| option.value == "no-ctx")
            .unwrap()
            .description,
        "200K context"
    );
}

#[test]
fn prepare_codex_catalog_writes_processed_provider_file() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models(
        r#"{"data":[{"id":"gpt-5.6-sol","name":"Sol","context_length":200000}]}"#,
    );
    let path =
        catalog::prepare_codex_catalog(&paths, "lab", &base_url, "sk-test").unwrap().unwrap();
    server.join().unwrap();
    assert_eq!(path, paths.dir.join("catalogs/lab.json"));
    let document: Value = read_json(&path);
    assert_eq!(document["models"][0]["slug"], "gpt-5.6-sol");
    assert_eq!(document["models"][0]["display_name"], "Sol");
    assert!(document.get("fetched_at").is_none());
    assert!(paths.dir.join("catalogs/lab.claude.json").is_file());
    assert!(paths.dir.join("catalogs/lab.opencode.json").is_file());
    assert!(paths.dir.join("catalogs/lab.pi.json").is_file());
    assert!(paths.dir.join("catalogs/lab.meta.json").is_file());
}

#[test]
fn missing_context_uses_models_dev_provider_default() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models(r#"{"data":[{"id":"deepseek-v4-flash"}]}"#);
    let path =
        catalog::prepare_codex_catalog(&paths, "deepseek", &base_url, "sk-test").unwrap().unwrap();
    server.join().unwrap();
    let document: Value = read_json(&path);
    assert_eq!(document["models"][0]["context_window"], 1_000_000);
    let seed: Value = read_json(paths.dir.join("catalogs/deepseek.claude.json"));
    assert_eq!(seed["additional_model_options"][0]["description"], "1M context");
}

#[test]
fn providers_models_update_bypasses_fresh_cache() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) =
        serve_openai_models_times(r#"{"data":[{"id":"first"},{"id":"second"}]}"#, 2);
    catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test").unwrap().unwrap();
    let count = catalog::update_models(&paths, "openrouter", &base_url, "sk-test").unwrap();
    server.join().unwrap();
    assert_eq!(count, 2);
}

#[test]
fn providers_models_update_command_fetches_configured_provider() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models(r#"{"data":[{"id":"sol"}]}"#);
    fs::write(&paths.config, format!("[provider.lab]\nbase_url = \"{base_url}\"\n")).unwrap();
    config::login(&paths, "lab", "sk-test".to_string()).unwrap();
    crate::providers::run(
        ProvidersCommand::ModelsUpdate { provider: Some("lab".to_string()) },
        &paths,
        &isolated(&[]),
    )
    .unwrap();
    server.join().unwrap();
    assert!(paths.dir.join("catalogs/lab.json").is_file());
    assert!(paths.dir.join("catalogs/lab.claude.json").is_file());
}

#[test]
fn provider_catalog_cache_is_reused_until_expiry() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models(r#"{"data":[{"id":"gpt-5.6-sol","name":"Sol"}]}"#);
    let first = catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    server.join().unwrap();
    let second = catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
    let models =
        catalog::load_opencode_models(&paths, "openrouter", &base_url, "sk-test", false).unwrap();
    assert!(models.contains_key("gpt-5.6-sol"));
    let pi = catalog::load_pi_models(&paths, "openrouter", &base_url, "sk-test", false).unwrap();
    assert_eq!(pi[0]["id"], "gpt-5.6-sol");
}

#[test]
fn catalogs_are_written_per_provider() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models_times(r#"{"data":[{"id":"shared-model"}]}"#, 2);
    catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-or").unwrap().unwrap();
    catalog::prepare_codex_catalog(&paths, "lab", &base_url, "sk-lab").unwrap().unwrap();
    server.join().unwrap();
    assert!(paths.dir.join("catalogs/openrouter.json").is_file());
    assert!(paths.dir.join("catalogs/lab.json").is_file());
    assert_ne!(paths.dir.join("catalogs/openrouter.json"), paths.dir.join("catalogs/lab.json"));
}

#[test]
fn expired_catalog_is_refetched() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models_times(r#"{"data":[{"id":"gpt-5.6-sol"}]}"#, 2);
    catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test").unwrap().unwrap();
    let meta_path = paths.dir.join("catalogs/openrouter.meta.json");
    let mut meta: Value = read_json(&meta_path);
    meta["fetched_at"] = json!(0);
    write_json(&meta_path, &meta);
    catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test").unwrap().unwrap();
    server.join().unwrap();
}

#[test]
fn catalog_endpoint_change_misses_cache() {
    let (_dir, paths) = temp_paths();
    let (first_url, first_server) = serve_openai_models(r#"{"data":[{"id":"first"}]}"#);
    catalog::prepare_codex_catalog(&paths, "lab", &first_url, "sk-test").unwrap().unwrap();
    first_server.join().unwrap();
    let (second_url, second_server) = serve_openai_models(r#"{"data":[{"id":"second"}]}"#);
    let path =
        catalog::prepare_codex_catalog(&paths, "lab", &second_url, "sk-test").unwrap().unwrap();
    second_server.join().unwrap();
    let document: Value = read_json(path);
    assert_eq!(document["models"][0]["slug"], "second");
}

#[test]
fn catalog_refresh_failure_does_not_reuse_other_endpoint() {
    let (_dir, paths) = temp_paths();
    let (first_url, first_server) = serve_openai_models(r#"{"data":[{"id":"first"}]}"#);
    catalog::prepare_codex_catalog(&paths, "lab", &first_url, "sk-test").unwrap().unwrap();
    first_server.join().unwrap();
    let (error_url, error_server) = serve_openai_error(500);
    let error = catalog::prepare_codex_catalog(&paths, "lab", &error_url, "sk-test").unwrap_err();
    error_server.join().unwrap();
    assert!(error.to_string().contains("HTTP 500"), "{error:#}");
    let document: Value = read_json(paths.dir.join("catalogs/lab.json"));
    assert_eq!(document["models"][0]["slug"], "first");
}

#[test]
fn catalog_refresh_failure_reuses_same_endpoint() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models_then_error(r#"{"data":[{"id":"first"}]}"#, 500);
    catalog::prepare_codex_catalog(&paths, "lab", &base_url, "sk-test").unwrap().unwrap();
    let meta_path = paths.dir.join("catalogs/lab.meta.json");
    let mut meta: Value = read_json(&meta_path);
    meta["fetched_at"] = json!(0);
    write_json(&meta_path, &meta);
    let path =
        catalog::prepare_codex_catalog(&paths, "lab", &base_url, "sk-test").unwrap().unwrap();
    server.join().unwrap();
    let document: Value = read_json(path);
    assert_eq!(document["models"][0]["slug"], "first");
}
