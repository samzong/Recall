use super::*;

#[test]
fn pi_openrouter_injects_provider_without_extension() {
    let (_dir, paths) = temp_paths();
    let env = isolated(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let plan =
        launch::plan(&request(Harness::Pi, Some("openrouter"), &["--print", "hi"]), &paths, &env)
            .unwrap();
    assert_eq!(plan.program, PathBuf::from("pi"));
    assert_eq!(plan.args[0], "--models");
    assert_eq!(plan.args[1], "openrouter/*");
    assert_eq!(plan.args[2], "--provider");
    assert_eq!(plan.args[3], "openrouter");
    assert!(!plan.args.iter().any(|arg| arg == "--approve"));
    assert!(!plan.args.iter().any(|arg| arg == "--extension"));
    assert!(!plan.env_set.iter().any(|(k, _)| k == "PI_CODING_AGENT_DIR"));
    assert_env(&plan, &[("OPENROUTER_API_KEY", "sk-or-test")]);
}

#[test]
fn pi_selected_credential_survives_route_cleanup() {
    let (_dir, paths) = temp_paths();
    for env_key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "CUSTOM_API_KEY"] {
        fs::write(
            &paths.config,
            format!("[provider.openrouter]\nenv = \"{env_key}\"\nauth = \"env\"\n"),
        )
        .unwrap();
        let env = isolated(&[(env_key, "sk-selected")]);
        let plan =
            launch::plan(&request(Harness::Pi, Some("openrouter"), &[]), &paths, &env).unwrap();
        let child_env: HashMap<_, _> = plan.env_set.into_iter().collect();
        assert_eq!(child_env.get(env_key).map(String::as_str), Some("sk-selected"));
        for other in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"] {
            if other != env_key {
                assert_eq!(child_env.get(other).map(String::as_str), Some(""));
            }
        }
    }
}

#[test]
fn pi_tokener_merges_provider_into_models_json() {
    let dir = tempfile::tempdir().unwrap();
    let models_path = dir.path().join("models.json");
    fs::write(&models_path, r#"{"providers":{"ollama":{"baseUrl":"http://127.0.0.1:11434/v1"}}}"#)
        .unwrap();
    let provider = json!({
        "baseUrl": "https://api.tokener.dev/v1",
        "apiKey": "$TOKENER_API_KEY",
        "api": "openai-responses",
        "models": [{ "id": "gpt-5.6-sol" }]
    });
    crate::pi::merge_provider(&models_path, "tokener", provider).unwrap();
    let document: Value = read_json(&models_path);
    assert!(document["providers"]["ollama"].is_object());
    assert_eq!(document["providers"]["tokener"]["baseUrl"], "https://api.tokener.dev/v1");
}

#[test]
fn pi_merge_provider_refuses_to_reset_corrupt_models_json() {
    let dir = tempfile::tempdir().unwrap();
    let models_path = dir.path().join("models.json");
    for body in [
        "{",
        "[]",
        r#"{"providers":[{"name":"user-entry"}],"other":"keep"}"#,
        r#"{"providers":null}"#,
        r#"{"providers":"user-entry"}"#,
        r#"{"providers":42}"#,
        r#"{"providers":false}"#,
    ] {
        fs::write(&models_path, body).unwrap();
        let provider = json!({ "baseUrl": "https://api.tokener.dev/v1" });
        assert!(crate::pi::merge_provider(&models_path, "tokener", provider).is_err());
        assert_eq!(fs::read_to_string(&models_path).unwrap(), body);
    }
}

#[test]
fn pi_tokener_prepares_native_models() {
    let (dir, paths) = temp_paths();
    let provider = provider::find("tokener").unwrap();
    let (base_url, server) = serve_openai_models(r#"{"data":[{"id":"gpt-5.6-sol"}]}"#);
    let agent_dir = dir.path().join("pi-agent");
    let env = isolated(&[("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())]);

    crate::pi::prepare("tokener", provider, &base_url, "sk-test", &paths, &env).unwrap();
    server.join().unwrap();

    let models: Value = read_json(agent_dir.join("models.json"));
    assert_eq!(models["providers"]["tokener"]["baseUrl"], format!("{base_url}/v1"));
    assert_eq!(models["providers"]["tokener"]["apiKey"], "$TOKENER_API_KEY");
    assert_eq!(models["providers"]["tokener"]["models"][0]["id"], "gpt-5.6-sol");
}
