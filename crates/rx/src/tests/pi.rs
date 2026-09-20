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
fn pi_merges_provider_into_models_json() {
    let dir = tempfile::tempdir().unwrap();
    let models_path = dir.path().join("models.json");
    fs::write(&models_path, r#"{"providers":{"ollama":{"baseUrl":"http://127.0.0.1:11434/v1"}}}"#)
        .unwrap();
    let provider = json!({
        "baseUrl": "https://provider.test/v1",
        "apiKey": "$ACME_API_KEY",
        "api": "openai-responses",
        "models": [{ "id": "gpt-5.6-sol" }]
    });
    crate::pi::merge_provider(&models_path, "acme", provider).unwrap();
    let document: Value = read_json(&models_path);
    assert!(document["providers"]["ollama"].is_object());
    assert_eq!(document["providers"]["acme"]["baseUrl"], "https://provider.test/v1");
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
        let provider = json!({ "baseUrl": "https://provider.test/v1" });
        assert!(crate::pi::merge_provider(&models_path, "acme", provider).is_err());
        assert_eq!(fs::read_to_string(&models_path).unwrap(), body);
    }
}

#[test]
fn pi_prepares_native_models() {
    let (dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models(r#"{"data":[{"id":"gpt-5.6-sol"}]}"#);
    let provider = fixture_provider("https://provider.test/v1");
    let agent_dir = dir.path().join("pi-agent");
    let env = isolated(&[("PI_CODING_AGENT_DIR", agent_dir.to_str().unwrap())]);

    crate::pi::prepare("acme", &provider, &base_url, "sk-test", &paths, &env).unwrap();
    server.join().unwrap();

    let models: Value = read_json(agent_dir.join("models.json"));
    assert_eq!(models["providers"]["acme"]["baseUrl"], format!("{base_url}/v1"));
    assert_eq!(models["providers"]["acme"]["apiKey"], "$ACME_API_KEY");
    assert_eq!(models["providers"]["acme"]["models"][0]["id"], "gpt-5.6-sol");
}

#[test]
fn pi_purge_removes_only_the_marked_provider() {
    let dir = tempfile::tempdir().unwrap();
    let models_path = dir.path().join("models.json");
    fs::write(&models_path, r#"{"providers":{"ollama":{"baseUrl":"http://127.0.0.1:11434/v1"}}}"#)
        .unwrap();
    let provider = json!({ "baseUrl": "https://provider.test/v1", "apiKey": "$ACME_API_KEY" });
    crate::pi::merge_provider(&models_path, "acme", provider).unwrap();
    let env = isolated(&[("PI_CODING_AGENT_DIR", dir.path().to_str().unwrap())]);

    assert_eq!(crate::pi::purge("acme", &env).unwrap(), crate::residue::Residue::Removed);

    let document: Value = read_json(&models_path);
    assert!(document["providers"]["ollama"].is_object());
    assert!(document["providers"].get("acme").is_none());
    assert!(!dir.path().join("models.json.rx-catalog.json").exists());
}

#[test]
fn pi_purge_reports_unmarked_and_user_edited_residue() {
    let dir = tempfile::tempdir().unwrap();
    let models_path = dir.path().join("models.json");
    let env = isolated(&[("PI_CODING_AGENT_DIR", dir.path().to_str().unwrap())]);
    fs::write(&models_path, r#"{"providers":{"acme":{"baseUrl":"https://provider.test/v1"}}}"#)
        .unwrap();

    assert_eq!(
        crate::pi::purge("acme", &env).unwrap(),
        crate::residue::Residue::Unowned(models_path.clone())
    );
    assert!(read_json::<Value>(&models_path)["providers"]["acme"].is_object());

    let provider = json!({ "baseUrl": "https://provider.test/v1", "apiKey": "$ACME_API_KEY" });
    crate::pi::merge_provider(&models_path, "acme", provider).unwrap();
    let mut document: Value = read_json(&models_path);
    document["providers"]["acme"]["baseUrl"] = json!("https://edited.test/v1");
    write_json(&models_path, &document);

    assert_eq!(
        crate::pi::purge("acme", &env).unwrap(),
        crate::residue::Residue::Modified(models_path.clone())
    );
    assert_eq!(
        read_json::<Value>(&models_path)["providers"]["acme"]["baseUrl"],
        "https://edited.test/v1"
    );
}

#[test]
fn pi_purge_is_absent_without_models_json() {
    let dir = tempfile::tempdir().unwrap();
    let env = isolated(&[("PI_CODING_AGENT_DIR", dir.path().to_str().unwrap())]);
    assert_eq!(crate::pi::purge("acme", &env).unwrap(), crate::residue::Residue::Absent);
}
