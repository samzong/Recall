use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use crate::args::{Command, ConfigCommand, Harness, LaunchRequest, parse, rewrite_argv0};
use crate::config::{self, Paths};
use crate::launch::{self, EnvLookup};

fn args(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|arg| (*arg).to_string()).collect()
}

fn parse_line(argv: &[&str]) -> Command {
    parse(&rewrite_argv0(args(argv))).unwrap()
}

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path().to_path_buf());
    (dir, paths)
}

#[test]
fn rxc_inserts_claude() {
    let command = parse_line(&["/usr/local/bin/rxc", "fix login"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Claude,
            gateway: None,
            passthrough: vec!["fix login".to_string()],
        })
    );
}

#[test]
fn rxx_inserts_codex() {
    let command = parse_line(&["rxx", "exec", "cargo test"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Codex,
            gateway: None,
            passthrough: vec!["exec".to_string(), "cargo test".to_string()],
        })
    );
}

#[test]
fn rxc_keeps_leading_gateway_flag() {
    let command = parse_line(&["rxc", "--gateway", "tokener"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Claude,
            gateway: Some("tokener".to_string()),
            passthrough: Vec::new(),
        })
    );
}

#[test]
fn gateway_flag_before_or_after_harness() {
    let before = parse_line(&["rx", "--gateway", "openrouter", "claude", "--resume", "abc"]);
    let after = parse_line(&["rx", "claude", "--gateway=openrouter", "--resume", "abc"]);
    let expected = Command::Launch(LaunchRequest {
        harness: Harness::Claude,
        gateway: Some("openrouter".to_string()),
        passthrough: vec!["--resume".to_string(), "abc".to_string()],
    });
    assert_eq!(before, expected);
    assert_eq!(after, expected);
}

#[test]
fn gateway_after_double_dash_is_passthrough() {
    let command = parse_line(&["rx", "claude", "--", "--gateway", "openrouter"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Claude,
            gateway: None,
            passthrough: vec!["--".to_string(), "--gateway".to_string(), "openrouter".to_string()],
        })
    );
}

#[test]
fn claude_help_is_passthrough() {
    let command = parse_line(&["rx", "claude", "--help"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Claude,
            gateway: None,
            passthrough: vec!["--help".to_string()],
        })
    );
}

#[test]
fn rx_help_and_version() {
    assert_eq!(parse_line(&["rx", "--help"]), Command::Help);
    assert_eq!(parse_line(&["rx", "-V"]), Command::Version);
}

#[test]
fn update_parses_yes_flag() {
    assert_eq!(parse_line(&["rx", "update"]), Command::Update { yes: false });
    assert_eq!(parse_line(&["rx", "update", "--yes"]), Command::Update { yes: true });
}

#[test]
fn version_cmp_orders_releases() {
    use std::cmp::Ordering;

    assert_eq!(crate::update::version_cmp("0.1.0", "0.5.0"), Ordering::Less);
    assert_eq!(crate::update::version_cmp("0.5.0", "0.5.0"), Ordering::Equal);
    assert_eq!(crate::update::version_cmp("0.6.0", "0.5.0"), Ordering::Greater);
}

#[test]
fn update_pending_uses_installed_release_not_crate_version() {
    let release = crate::update::ReleaseInfo {
        version: "0.5.0".to_string(),
        asset_name: String::new(),
        download_url: String::new(),
    };
    // rx's crate version stays 0.1.0 while core releases advance; once this
    // release stream artifact is recorded as installed, it must not re-prompt.
    assert!(!crate::update::update_pending(
        "0.1.0",
        &release,
        &crate::update::state_with_installed(Some("0.5.0"))
    ));
    assert!(crate::update::update_pending(
        "0.1.0",
        &release,
        &crate::update::state_with_installed(Some("0.4.0"))
    ));
    assert!(crate::update::update_pending(
        "0.1.0",
        &release,
        &crate::update::state_with_installed(None)
    ));
}

#[test]
fn argv0_harness_resolves_aliases() {
    assert_eq!(crate::args::argv0_harness("/usr/local/bin/rxc"), Some("claude"));
    assert_eq!(crate::args::argv0_harness("rxx"), Some("codex"));
    assert_eq!(crate::args::argv0_harness("rxo.exe"), Some("opencode"));
    assert_eq!(crate::args::argv0_harness("rxp"), Some("pi"));
    assert_eq!(crate::args::argv0_harness("/home/u/.cargo/bin/rx"), None);
}

#[test]
fn claude_seed_replaces_wrong_typed_growthbook_cache() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    fs::write(&config_path, r#"{"cachedGrowthBookFeatures": null}"#).unwrap();
    let caches = crate::claude_catalog::SeedCaches::default();
    crate::claude_catalog::write_seed_for_test(&config_path, &caches).unwrap();
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(document["cachedGrowthBookFeatures"].is_object());
}

#[test]
fn release_asset_name_matches_host() {
    let name = crate::update::release_asset_name().unwrap();
    assert!(name.starts_with("recall-"));
}

#[test]
fn unknown_harness_errors() {
    let error = parse(&args(&["rx", "gemini"])).unwrap_err();
    assert!(error.to_string().contains("unknown harness: gemini"), "{error}");
}

#[test]
fn rxo_inserts_opencode() {
    let command = parse_line(&["rxo", "run", "hello"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::OpenCode,
            gateway: None,
            passthrough: vec!["run".to_string(), "hello".to_string()],
        })
    );
}

#[test]
fn rxp_inserts_pi() {
    let command = parse_line(&["rxp", "--print", "hi"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Pi,
            gateway: None,
            passthrough: vec!["--print".to_string(), "hi".to_string()],
        })
    );
}

#[test]
fn missing_gateway_value_errors() {
    let error = parse(&args(&["rx", "claude", "--gateway"])).unwrap_err();
    assert!(error.to_string().contains("--gateway requires a value"), "{error}");
}

#[test]
fn config_set_and_get_parse() {
    assert_eq!(
        parse_line(&["rx", "config", "set", "gateway", "openrouter"]),
        Command::Config(ConfigCommand::SetGateway { name: "openrouter".to_string() })
    );
    assert_eq!(
        parse_line(&["rx", "config", "set", "key", "tokener", "sk-test"]),
        Command::Config(ConfigCommand::SetKey {
            profile: "tokener".to_string(),
            key: "sk-test".to_string(),
        })
    );
    assert_eq!(
        parse_line(&["rx", "config", "get"]),
        Command::Config(ConfigCommand::Get { name: None })
    );
}

#[test]
fn unconfigured_launch_is_passthrough() {
    let (_dir, paths) = temp_paths();
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Claude,
            gateway: None,
            passthrough: vec!["--resume".to_string(), "abc".to_string()],
        },
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap();
    assert_eq!(plan.program, "claude");
    assert_eq!(plan.args, vec!["--resume", "abc"]);
    assert!(plan.env_set.is_empty());
    assert!(plan.stderr_note.as_deref().unwrap().contains("no gateway configured"));
}

#[test]
fn claude_openrouter_uses_api_key_and_discovery_fallback_without_seed() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "OPENROUTER_API_KEY".to_string(),
        "sk-or-test".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Claude,
            gateway: Some("openrouter".to_string()),
            passthrough: vec!["fix it".to_string()],
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, "claude");
    assert_eq!(plan.args, vec!["fix it"]);
    assert!(
        plan.env_set
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://openrouter.ai/api")
    );
    assert!(plan.env_set.iter().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-or-test"));
    assert!(plan.env_set.iter().any(|(k, v)| k == "ANTHROPIC_AUTH_TOKEN" && v.is_empty()));
    assert!(
        plan.env_set
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY" && v == "1")
    );
    assert!(plan.env_set.iter().any(|(k, v)| k == "OPENROUTER_API_KEY" && v == "sk-or-test"));
    assert!(
        plan.env_set.iter().any(|(k, v)| k == "ANTHROPIC_DEFAULT_SONNET_MODEL"
            && v == "~anthropic/claude-sonnet-latest")
    );
    assert!(
        plan.env_set
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_MODEL" && v == "~anthropic/claude-sonnet-latest")
    );
    assert!(plan.stderr_note.as_deref().unwrap().contains("user catalog seed failed"));
}

#[test]
fn claude_injection_tokener_still_uses_auth_token() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "TOKENER_API_KEY".to_string(),
        "sk-tokener".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Claude,
            gateway: Some("tokener".to_string()),
            passthrough: vec!["fix it".to_string()],
        },
        &paths,
        &env,
    )
    .unwrap();
    assert!(plan.env_set.iter().any(|(k, v)| k == "ANTHROPIC_AUTH_TOKEN" && v == "sk-tokener"));
    assert!(plan.env_set.iter().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_empty()));
}

#[test]
fn codex_openrouter_overrides_model_and_uses_command_auth() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "OPENROUTER_API_KEY".to_string(),
        "sk-or-test".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Codex,
            gateway: Some("openrouter".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, "codex");
    assert_eq!(plan.args[0], "-c");
    assert_eq!(plan.args[1], "model_provider=\"openrouter\"");
    assert!(plan.args[3].contains("base_url=\"https://openrouter.ai/api/v1\""));
    assert!(plan.args[3].contains("wire_api=\"responses\""));
    assert!(plan.args[3].contains("supports_websockets=false"));
    assert!(plan.args[3].contains("auth={command=\"sh\""));
    assert!(!plan.args[3].contains("env_key="));
    assert_eq!(plan.args[5], "model=\"~openai/gpt-latest\"");
    assert_eq!(plan.env_set, vec![("OPENROUTER_API_KEY".to_string(), "sk-or-test".to_string())]);
}

#[test]
fn opencode_openrouter_injects_config_and_model() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "OPENROUTER_API_KEY".to_string(),
        "sk-or-test".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::OpenCode,
            gateway: Some("openrouter".to_string()),
            passthrough: vec!["run".to_string(), "hello".to_string()],
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, "opencode");
    assert_eq!(plan.args, vec!["run".to_string(), "hello".to_string()]);
    assert!(plan.env_set.iter().any(|(k, v)| k == "OPENROUTER_API_KEY" && v == "sk-or-test"));
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
fn pi_openrouter_injects_provider_without_extension() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "OPENROUTER_API_KEY".to_string(),
        "sk-or-test".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Pi,
            gateway: Some("openrouter".to_string()),
            passthrough: vec!["--print".to_string(), "hi".to_string()],
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, "pi");
    assert_eq!(plan.args[0], "--models");
    assert_eq!(plan.args[1], "openrouter/*");
    assert_eq!(plan.args[2], "--provider");
    assert_eq!(plan.args[3], "openrouter");
    assert!(!plan.args.iter().any(|arg| arg == "--extension"));
    assert!(!plan.env_set.iter().any(|(k, _)| k == "PI_CODING_AGENT_DIR"));
    assert!(plan.env_set.iter().any(|(k, v)| k == "OPENROUTER_API_KEY" && v == "sk-or-test"));
}

#[test]
fn pi_tokener_merges_provider_into_models_json() {
    let dir = tempfile::tempdir().unwrap();
    let models_path = dir.path().join("models.json");
    fs::write(&models_path, r#"{"providers":{"ollama":{"baseUrl":"http://127.0.0.1:11434/v1"}}}"#)
        .unwrap();
    let provider = serde_json::json!({
        "baseUrl": "https://api.tokener.dev/v1",
        "apiKey": "$TOKENER_API_KEY",
        "api": "openai-responses",
        "models": [{ "id": "gpt-5.6-sol" }]
    });
    crate::pi::merge_provider(&models_path, "tokener", provider).unwrap();
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&models_path).unwrap()).unwrap();
    assert!(document["providers"]["ollama"].is_object());
    assert_eq!(document["providers"]["tokener"]["baseUrl"], "https://api.tokener.dev/v1");
}

#[test]
fn pi_merge_provider_refuses_to_reset_corrupt_models_json() {
    let dir = tempfile::tempdir().unwrap();
    let models_path = dir.path().join("models.json");
    fs::write(&models_path, "{").unwrap();
    let provider = serde_json::json!({ "baseUrl": "https://api.tokener.dev/v1" });
    let error = crate::pi::merge_provider(&models_path, "tokener", provider).unwrap_err();
    assert!(error.to_string().contains("failed to parse"));
    // The corrupt file must be left untouched, not overwritten with an empty provider map.
    assert_eq!(fs::read_to_string(&models_path).unwrap(), "{");
}

#[test]
fn pi_tokener_writes_recall_cache() {
    let (dir, paths) = temp_paths();
    let driver = launch::driver("tokener").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let size = stream.read(&mut request).unwrap();
        assert!(String::from_utf8_lossy(&request[..size]).starts_with("GET /v1/models "));
        let body = r#"{"data":[{"id":"gpt-5.6-sol"}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let agent_dir = dir.path().join("pi-agent");
    let env = EnvLookup::isolated(HashMap::from([(
        "PI_CODING_AGENT_DIR".to_string(),
        agent_dir.display().to_string(),
    )]));

    crate::pi::prepare("tokener", driver, &base_url, "sk-test", &paths, &env).unwrap();
    server.join().unwrap();

    let cache = dir.path().join("pi/tokener-provider.json");
    assert!(cache.is_file());
    let body = fs::read_to_string(cache).unwrap();
    assert!(body.contains(&format!("\"baseUrl\": \"{base_url}/v1\"")));
    let models = fs::read_to_string(agent_dir.join("models.json")).unwrap();
    assert!(models.contains("gpt-5.6-sol"));
}

#[test]
fn codex_passthrough_model_flag_wins() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "OPENROUTER_API_KEY".to_string(),
        "sk-or-test".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Codex,
            gateway: Some("openrouter".to_string()),
            passthrough: vec!["--model".to_string(), "anthropic/claude-sonnet-4.6".to_string()],
        },
        &paths,
        &env,
    )
    .unwrap();
    assert!(!plan.args.iter().any(|arg| arg.starts_with("model=")));
    assert_eq!(plan.args[plan.args.len() - 2], "--model");
    assert_eq!(plan.args[plan.args.len() - 1], "anthropic/claude-sonnet-4.6");
}

#[test]
fn codex_tokener_does_not_invent_a_model() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "TOKENER_API_KEY".to_string(),
        "sk-tokener".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Codex,
            gateway: Some("tokener".to_string()),
            passthrough: vec!["exec".to_string(), "cargo test".to_string()],
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.args[1], "model_provider=\"tokener\"");
    assert!(plan.args[3].contains("base_url=\"https://api.tokener.dev/v1\""));
    assert!(!plan.args.iter().any(|arg| arg.starts_with("model=")));
    assert_eq!(plan.args[4..], ["exec", "cargo test"]);
}

#[test]
fn missing_key_errors_before_exec() {
    let (_dir, paths) = temp_paths();
    let error = launch::plan(
        &LaunchRequest {
            harness: Harness::Claude,
            gateway: Some("openrouter".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("no API key for gateway 'openrouter'"), "{error}");
}

#[test]
fn config_set_key_is_redacted_and_secret() {
    let (_dir, paths) = temp_paths();
    config::run(ConfigCommand::SetGateway { name: "openrouter".to_string() }, &paths).unwrap();
    config::run(
        ConfigCommand::SetKey { profile: "openrouter".to_string(), key: "sk-secret".to_string() },
        &paths,
    )
    .unwrap();

    let listing = config::format_get(&paths, None).unwrap();
    assert!(listing.contains("default_gateway = openrouter"));
    assert!(listing.contains("key = set"));
    assert!(!listing.contains("sk-secret"));

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

    let env = EnvLookup::isolated(HashMap::new());
    let plan = launch::plan(
        &LaunchRequest { harness: Harness::Claude, gateway: None, passthrough: Vec::new() },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.env_set[1], ("ANTHROPIC_API_KEY".to_string(), "sk-secret".to_string()));
}

#[test]
fn config_set_key_does_not_switch_auth_when_keys_cannot_be_loaded() {
    let (_dir, paths) = temp_paths();
    fs::write(
        &paths.config,
        r#"default_gateway = "openrouter"

[gateway.openrouter]
base_url = "https://openrouter.ai/api"
auth = "env"
"#,
    )
    .unwrap();
    fs::write(&paths.keys, "{").unwrap();

    let error = config::run(
        ConfigCommand::SetKey { profile: "openrouter".to_string(), key: "sk-secret".to_string() },
        &paths,
    )
    .unwrap_err();
    assert!(error.to_string().contains("failed to parse"), "{error}");

    let config = config::load(&paths).unwrap().unwrap();
    assert_eq!(config.gateway["openrouter"].auth, config::AuthMode::Env);
}

#[test]
fn custom_gateway_profiles_keep_independent_keys_and_driver_behavior() {
    let (dir, paths) = temp_paths();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let dev_base_url = format!("http://{}", listener.local_addr().unwrap());
    fs::write(
        &paths.config,
        format!(
            r#"default_gateway = "tokener-dev"

[gateway.tokener-dev]
driver = "tokener"
base_url = "{dev_base_url}"
model = "gpt-dev"

[gateway.tokener-prod]
driver = "tokener"
base_url = "https://prod.gateway.test"
model = "gpt-prod"
"#
        ),
    )
    .unwrap();

    config::run(
        ConfigCommand::SetKey { profile: "tokener-dev".to_string(), key: "sk-dev".to_string() },
        &paths,
    )
    .unwrap();
    config::run(
        ConfigCommand::SetKey { profile: "tokener-prod".to_string(), key: "sk-prod".to_string() },
        &paths,
    )
    .unwrap();
    config::run(ConfigCommand::SetGateway { name: "tokener-prod".to_string() }, &paths).unwrap();

    let listing = config::format_get(&paths, None).unwrap();
    assert!(listing.contains("default_gateway = tokener-prod"));
    assert!(listing.contains("[tokener-dev]\ndriver = tokener"));
    assert!(listing.contains("[tokener-prod]\ndriver = tokener"));

    let agent_dir = dir.path().join("pi-agent");
    let env = EnvLookup::isolated(HashMap::from([(
        "PI_CODING_AGENT_DIR".to_string(),
        agent_dir.display().to_string(),
    )]));
    let codex = launch::plan(
        &LaunchRequest {
            harness: Harness::Codex,
            gateway: Some("tokener-dev".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(codex.args[1], "model_provider=\"tokener-dev\"");
    assert!(codex.args[3].contains("model_providers.tokener-dev="));
    assert!(codex.args[3].contains(&format!("base_url=\"{dev_base_url}/v1\"")));
    assert_eq!(codex.env_set, vec![("TOKENER_API_KEY".to_string(), "sk-dev".to_string())]);

    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let size = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..size]).starts_with("GET /v1/models "));
            let body = r#"{"data":[{"id":"gpt-dev"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });

    let opencode = launch::plan(
        &LaunchRequest {
            harness: Harness::OpenCode,
            gateway: Some("tokener-dev".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(opencode.args, ["-m", "tokener-dev/gpt-dev"]);
    let opencode_config = opencode
        .env_set
        .iter()
        .find(|(name, _)| name == "OPENCODE_CONFIG_CONTENT")
        .map(|(_, value)| serde_json::from_str::<serde_json::Value>(value).unwrap())
        .unwrap();
    assert!(opencode_config["provider"]["tokener-dev"].is_object());
    assert!(opencode_config["provider"]["tokener"].is_null());

    let pi = launch::plan(
        &LaunchRequest {
            harness: Harness::Pi,
            gateway: Some("tokener-dev".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(pi.args, ["--models", "tokener-dev/*", "--model", "tokener-dev/gpt-dev"]);
    assert!(dir.path().join("pi/tokener-dev-provider.json").is_file());
    let pi_models: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(agent_dir.join("models.json")).unwrap()).unwrap();
    assert!(pi_models["providers"]["tokener-dev"].is_object());
    assert!(pi_models["providers"]["tokener"].is_null());
    server.join().unwrap();

    let claude = launch::plan(
        &LaunchRequest {
            harness: Harness::Claude,
            gateway: Some("tokener-prod".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert!(
        claude
            .env_set
            .iter()
            .any(|(key, value)| key == "ANTHROPIC_BASE_URL" && value == "https://prod.gateway.test")
    );
    assert!(
        claude
            .env_set
            .iter()
            .any(|(key, value)| key == "ANTHROPIC_AUTH_TOKEN" && value == "sk-prod")
    );
}

#[test]
fn gateway_profile_names_cannot_escape_generated_config_boundaries() {
    let (_dir, paths) = temp_paths();
    fs::write(
        &paths.config,
        r#"default_gateway = "../prod"

[gateway."../prod"]
driver = "tokener"
"#,
    )
    .unwrap();

    let error = launch::plan(
        &LaunchRequest { harness: Harness::Pi, gateway: None, passthrough: Vec::new() },
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("invalid gateway profile name '../prod'"), "{error}");
}

#[test]
fn url_helpers_strip_or_add_v1() {
    assert_eq!(
        launch::anthropic_base("https://openrouter.ai/api/v1/"),
        "https://openrouter.ai/api"
    );
    assert_eq!(launch::openai_base("https://api.tokener.dev"), "https://api.tokener.dev/v1");
    assert_eq!(launch::openai_base("https://openrouter.ai/api/v1"), "https://openrouter.ai/api/v1");
}

#[test]
fn debug_models_parses_gateway_flag() {
    assert_eq!(
        parse_line(&["rx", "debug", "models"]),
        Command::Debug { subcommand: crate::debug::Subcommand::Models, gateway: None }
    );
    assert_eq!(
        parse_line(&["rx", "--gateway", "tokener", "debug", "models"]),
        Command::Debug {
            subcommand: crate::debug::Subcommand::Models,
            gateway: Some("tokener".to_string()),
        }
    );
    assert_eq!(
        parse_line(&["rx", "debug", "models", "--gateway=openrouter"]),
        Command::Debug {
            subcommand: crate::debug::Subcommand::Models,
            gateway: Some("openrouter".to_string()),
        }
    );
}

#[test]
fn debug_models_rejects_extra_args() {
    let error = parse(&args(&["rx", "debug", "models", "extra"])).unwrap_err();
    assert!(error.to_string().contains("usage: rx debug models"), "{error}");
}

#[test]
fn debug_models_requires_configured_gateway() {
    let (_dir, paths) = temp_paths();
    let error =
        crate::debug::models::run(None, &paths, &EnvLookup::isolated(HashMap::new())).unwrap_err();
    assert!(error.to_string().contains("no gateway configured"), "{error}");
}

#[test]
fn parse_catalog_detects_openai_anthropic_and_codex_shapes() {
    let (shape, envelope, models) = crate::catalog::parse_catalog(
        r#"{"data":[{"id":"openai/gpt-4","name":"GPT-4"}],"total_count":1}"#,
    )
    .unwrap();
    assert_eq!(shape, crate::catalog::CatalogShape::OpenAi);
    assert_eq!(envelope, vec!["data".to_string(), "total_count".to_string()]);
    assert_eq!(models[0].id, "openai/gpt-4");
    assert_eq!(models[0].label.as_deref(), Some("GPT-4"));

    let (shape, envelope, models) = crate::catalog::parse_catalog(
        r#"{"data":[{"id":"anthropic/openai/gpt-4","display_name":"GPT-4"}],"has_more":false}"#,
    )
    .unwrap();
    assert_eq!(shape, crate::catalog::CatalogShape::Anthropic);
    assert!(envelope.contains(&"has_more".to_string()));
    assert_eq!(models[0].id, "anthropic/openai/gpt-4");

    let (shape, envelope, models) = crate::catalog::parse_catalog(
        r#"{"models":[{"slug":"gpt-5.6-sol","display_name":"GPT 5.6 Sol"}]}"#,
    )
    .unwrap();
    assert_eq!(shape, crate::catalog::CatalogShape::Codex);
    assert_eq!(envelope, vec!["models".to_string()]);
    assert_eq!(models[0].id, "gpt-5.6-sol");
    assert_eq!(models[0].label.as_deref(), Some("GPT 5.6 Sol"));
}

#[test]
fn render_splits_openai_and_anthropic_ids() {
    let openai = crate::debug::models::Probe {
        url: "https://example.test/v1/models".to_string(),
        headers: vec!["Authorization: Bearer".to_string()],
        status: Some(200),
        error: None,
        envelope: vec!["data".to_string()],
        shape: crate::catalog::CatalogShape::OpenAi,
        models: vec![
            crate::catalog::ListedModel { id: "a".to_string(), label: None },
            crate::catalog::ListedModel { id: "b".to_string(), label: None },
        ],
    };
    let anthropic = crate::debug::models::Probe {
        url: "https://example.test/v1/models?limit=1000".to_string(),
        headers: vec!["Authorization: Bearer".to_string()],
        status: Some(200),
        error: None,
        envelope: vec!["data".to_string()],
        shape: crate::catalog::CatalogShape::Anthropic,
        models: vec![crate::catalog::ListedModel {
            id: "anthropic/b".to_string(),
            label: Some("B".to_string()),
        }],
    };
    let codex = crate::debug::models::Probe {
        url: "https://example.test/v1/models?client_version=0.149.0".to_string(),
        headers: vec!["Authorization: Bearer".to_string()],
        status: Some(200),
        error: None,
        envelope: vec!["models".to_string()],
        shape: crate::catalog::CatalogShape::Codex,
        models: vec![crate::catalog::ListedModel {
            id: "gpt-5.6-sol".to_string(),
            label: Some("GPT 5.6 Sol".to_string()),
        }],
    };
    let user = crate::debug::models::Probe {
        url: "https://example.test/v1/models/user?limit=1000".to_string(),
        headers: vec!["Authorization: Bearer".to_string()],
        status: Some(200),
        error: None,
        envelope: vec!["data".to_string(), "total_count".to_string()],
        shape: crate::catalog::CatalogShape::OpenAi,
        models: vec![crate::catalog::ListedModel {
            id: "anthropic/claude-sonnet-5".to_string(),
            label: Some("Sonnet 5".to_string()),
        }],
    };
    let text = crate::debug::models::render(
        "tokener",
        "https://api.tokener.dev",
        &openai,
        &codex,
        &anthropic,
        &user,
    );
    assert!(text.contains("## OpenAI (GET /v1/models)"));
    assert!(text.contains("## Codex (GET /v1/models?client_version=...)"));
    assert!(text.contains("## Claude seed (GET /v1/models/user?limit=1000)"));
    assert!(text.contains("only OpenAI: 2"));
    assert!(text.contains("only Anthropic: 1"));
    assert!(text.contains("Claude discovery filter (id contains claude|anthropic): 1/1"));
    assert!(text.contains("Claude user catalog models: 1"));
}

#[test]
fn tokener_seeded_plan_uses_auth_token_and_settings() {
    let plan = launch::inject_claude_tokener_seeded_for_test(
        &LaunchRequest {
            harness: Harness::Claude,
            gateway: Some("tokener".to_string()),
            passthrough: vec!["fix it".to_string()],
        },
        launch::driver("tokener").unwrap(),
        "http://localhost:8080",
        "sk-tokener",
        None,
    );
    assert_eq!(plan.args[0], "--settings");
    assert!(plan.args[1].contains("TOKENER_API_KEY"));
    assert_eq!(plan.args[2], "fix it");
    assert!(plan.env_set.iter().any(|(k, v)| k == "ANTHROPIC_AUTH_TOKEN" && v == "sk-tokener"));
    assert!(plan.env_set.iter().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_empty()));
    assert!(
        plan.env_set
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY" && v == "0")
    );
    assert!(plan.stderr_note.is_none());
}

#[test]
fn openrouter_seed_builds_picker_and_denylist() {
    let models = vec![
        crate::claude_catalog::UserModel {
            id: "anthropic/claude-sonnet-5".to_string(),
            name: Some("Sonnet 5".to_string()),
            context_length: Some(200_000),
            canonical_slug: Some("anthropic/claude-sonnet-5".to_string()),
            supported_efforts: vec!["high".to_string()],
            pricing: None,
        },
        crate::claude_catalog::UserModel {
            id: "openai/gpt-5.6-sol".to_string(),
            name: Some("GPT 5.6 Sol".to_string()),
            context_length: Some(400_000),
            canonical_slug: None,
            supported_efforts: vec![],
            pricing: None,
        },
    ];
    let seed = crate::claude_catalog::build_seed(&models);
    assert_eq!(seed.additional_model_options.len(), 2);
    assert!(seed.model_access.iter().any(|entry| entry.api_name == "claude-sonnet-5"));
    assert!(seed.model_access.iter().any(|entry| entry.api_name == "openai/gpt-5.6-sol"));
    assert!(seed.tool_search_denylist.iter().any(|id| id == "openai/gpt-5.6-sol"));
    assert!(!seed.tool_search_denylist.iter().any(|id| id == "claude-sonnet-5"));
}

#[test]
fn openrouter_seed_writes_claude_json() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    fs::write(
        &config_path,
        r#"{"additionalModelOptionsCache":[{"value":"keep-me","label":"Keep","description":"old"}]}"#,
    )
    .unwrap();
    let caches = crate::claude_catalog::build_seed(&[crate::claude_catalog::UserModel {
        id: "google/gemini-3.7-flash".to_string(),
        name: Some("Gemini".to_string()),
        context_length: Some(200_000),
        canonical_slug: None,
        supported_efforts: vec![],
        pricing: None,
    }]);
    crate::claude_catalog::write_seed_for_test(&config_path, &caches).unwrap();
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let options = document["additionalModelOptionsCache"].as_array().unwrap();
    assert_eq!(options.len(), 2);
    assert!(options.iter().any(|entry| entry["value"] == "keep-me"));
    assert!(options.iter().any(|entry| entry["value"] == "google/gemini-3.7-flash"));
    assert!(
        document["rxSeededToolSearchDenylist"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "google/gemini-3.7-flash")
    );
}

#[test]
fn claude_seed_errors_on_non_object_config_root_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    fs::write(&config_path, r#"["not", "an", "object"]"#).unwrap();
    let caches = crate::claude_catalog::SeedCaches::default();
    let error = crate::claude_catalog::write_seed_for_test(&config_path, &caches).unwrap_err();
    assert!(error.to_string().contains("not a JSON object"));
    // The original file is preserved.
    assert_eq!(fs::read_to_string(&config_path).unwrap(), r#"["not", "an", "object"]"#);
}

#[test]
fn openrouter_seeded_plan_injects_settings() {
    let plan = launch::inject_claude_openrouter_for_test(
        &LaunchRequest {
            harness: Harness::Claude,
            gateway: Some("openrouter".to_string()),
            passthrough: vec!["fix it".to_string()],
        },
        "https://openrouter.ai/api",
        "sk-or-test",
        Some("~anthropic/claude-sonnet-latest"),
        crate::claude_catalog::SeedOutcome::Seeded { model_count: 2 },
    );
    assert_eq!(plan.args[0], "--settings");
    assert!(plan.args[1].contains("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"));
    assert_eq!(plan.args[2], "fix it");
    assert!(plan.env_set.iter().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-or-test"));
    assert!(
        plan.env_set
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY" && v == "0")
    );
    assert!(plan.env_set.iter().any(|(k, v)| k == "ENABLE_TOOL_SEARCH" && v == "true"));
    assert!(plan.stderr_note.is_none());
}

#[test]
#[ignore = "live OpenRouter network + writes ~/.claude.json when HOME/CLAUDE_CONFIG_DIR aim at temp dir"]
fn live_openrouter_seed_populates_claude_json() {
    let dir = tempfile::tempdir().unwrap();
    let recall = dir.path().join(".recall");
    fs::create_dir_all(&recall).unwrap();
    fs::write(recall.join("rx.toml"), "default_gateway = \"openrouter\"\n\n[gateway.openrouter]\n")
        .unwrap();
    let key = std::env::var("OPENROUTER_API_KEY")
        .or_else(|_| std::env::var("ORI_OPENROUTER_API_KEY"))
        .expect("set OPENROUTER_API_KEY for live seed test");
    fs::write(recall.join("rx.keys"), format!("openrouter = \"{key}\"\n")).unwrap();
    let env = EnvLookup::isolated(HashMap::from([
        ("CLAUDE_CONFIG_DIR".to_string(), dir.path().display().to_string()),
        ("OPENROUTER_API_KEY".to_string(), key),
    ]));
    let outcome = crate::claude_catalog::try_seed_user_catalog(
        "https://openrouter.ai/api",
        &env.get("OPENROUTER_API_KEY").unwrap(),
        &env,
    )
    .unwrap();
    assert!(matches!(outcome, crate::claude_catalog::SeedOutcome::Seeded { .. }));
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(".claude.json")).unwrap())
            .unwrap();
    let count = document["additionalModelOptionsCache"].as_array().unwrap().len();
    assert!(count > 100, "expected a large seeded catalog, got {count}");
}

#[test]
fn install_specs_use_official_urls() {
    let claude = crate::install::spec(Harness::Claude);
    assert_eq!(claude.url, "https://claude.ai/install.sh");
    assert_eq!(claude.shell, "bash");
    assert_eq!(
        crate::install::command_line(&claude),
        "curl -fsSL https://claude.ai/install.sh | bash"
    );

    let codex = crate::install::spec(Harness::Codex);
    assert_eq!(codex.url, "https://chatgpt.com/codex/install.sh");
    assert_eq!(codex.shell, "sh");

    let opencode = crate::install::spec(Harness::OpenCode);
    assert_eq!(opencode.url, "https://opencode.ai/install");
    assert_eq!(opencode.shell, "bash");

    let pi = crate::install::spec(Harness::Pi);
    assert_eq!(pi.url, "https://pi.dev/install.sh");
    assert_eq!(pi.shell, "sh");
}

#[test]
fn install_lookup_finds_extra_dir_when_absent_from_path() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("pi");
    fs::write(&bin, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let found = crate::install::lookup_with("pi", "", &[dir.path().to_path_buf()], None).unwrap();
    assert_eq!(found, bin);
    assert!(crate::install::lookup_with("pi", "", &[], None).is_none());
}

#[test]
fn install_lookup_honors_windows_executable_extensions() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("codex.exe");
    fs::write(&bin, "windows executable").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let found = crate::install::lookup_with(
        "codex",
        "",
        &[dir.path().to_path_buf()],
        Some(std::ffi::OsStr::new(".COM;.EXE;.exe")),
    )
    .unwrap();
    assert!(found.is_file());
    assert_eq!(found.parent(), bin.parent());
    assert!(found.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("codex.exe"));
}

#[test]
fn isolated_env_skips_install_offer() {
    let path = crate::install::ensure(Harness::Pi, &EnvLookup::isolated(HashMap::new())).unwrap();
    assert_eq!(path, std::path::PathBuf::from("pi"));
}
