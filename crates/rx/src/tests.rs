use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use crate::args::{
    Command, Harness, LaunchRequest, ModelsCommand, ProvidersCommand, UpdateCommand, parse,
    rewrite_argv0,
};
use crate::config::{self, Paths};
use crate::launch::{self, EnvLookup};

fn args(argv: &[&str]) -> Vec<OsString> {
    argv.iter().map(|arg| OsString::from(*arg)).collect()
}

fn os(argv: &[&str]) -> Vec<OsString> {
    args(argv)
}

fn arg_str(arg: &OsString) -> &str {
    arg.to_str().expect("test args are utf-8")
}

fn parse_line(argv: &[&str]) -> Command {
    parse(&rewrite_argv0(args(argv))).unwrap()
}

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path().to_path_buf());
    (dir, paths)
}

fn serve_openai_models(body: &str) -> (String, thread::JoinHandle<()>) {
    serve_openai_models_times(body, 1)
}

fn serve_openai_models_times(body: &str, times: usize) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let body = body.to_string();
    let server = thread::spawn(move || {
        for _ in 0..times {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let size = stream.read(&mut request).unwrap();
            let req = String::from_utf8_lossy(&request[..size]);
            assert!(req.contains("GET /v1/models "), "{req}");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (base_url, server)
}

fn serve_openai_error(status: u16) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 2048];
        let _ = stream.read(&mut request);
        write!(
            stream,
            "HTTP/1.1 {status} ERR\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfail"
        )
        .unwrap();
    });
    (base_url, server)
}

fn serve_openai_models_then_error(body: &str, status: u16) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let body = body.to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 2048];
        let size = stream.read(&mut request).unwrap();
        let req = String::from_utf8_lossy(&request[..size]);
        assert!(req.contains("GET /v1/models "), "{req}");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 2048];
        let _ = stream.read(&mut request);
        write!(
            stream,
            "HTTP/1.1 {status} ERR\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfail"
        )
        .unwrap();
    });
    (base_url, server)
}

#[test]
fn rxc_inserts_claude() {
    let command = parse_line(&["/usr/local/bin/rxc", "fix login"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Claude,
            provider: None,
            passthrough: os(&["fix login"]),
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
            provider: None,
            passthrough: os(&["exec", "cargo test"]),
        })
    );
}

#[test]
fn rxc_keeps_leading_provider_flag() {
    let command = parse_line(&["rxc", "--provider", "tokener"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Claude,
            provider: Some("tokener".to_string()),
            passthrough: Vec::new(),
        })
    );
}

#[test]
fn provider_flag_before_or_after_harness() {
    let before = parse_line(&["rx", "--provider", "openrouter", "claude", "--resume", "abc"]);
    let after = parse_line(&["rx", "claude", "--provider=openrouter", "--resume", "abc"]);
    let expected = Command::Launch(LaunchRequest {
        harness: Harness::Claude,
        provider: Some("openrouter".to_string()),
        passthrough: os(&["--resume", "abc"]),
    });
    assert_eq!(before, expected);
    assert_eq!(after, expected);
}

#[test]
fn provider_after_double_dash_is_passthrough() {
    let command = parse_line(&["rx", "claude", "--", "--provider", "openrouter"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Claude,
            provider: None,
            passthrough: os(&["--", "--provider", "openrouter"]),
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
            provider: None,
            passthrough: os(&["--help"]),
        })
    );
}

#[test]
fn bare_rx_is_harness_picker() {
    assert_eq!(parse_line(&["rx"]), Command::PickHarness { provider: None });
}

#[test]
fn bare_rx_with_provider_is_harness_picker() {
    assert_eq!(
        parse_line(&["rx", "--provider", "deepseek"]),
        Command::PickHarness { provider: Some("deepseek".to_string()) }
    );
    assert_eq!(
        parse_line(&["rx", "--provider=deepseek"]),
        Command::PickHarness { provider: Some("deepseek".to_string()) }
    );
}

#[test]
fn rx_help_and_version() {
    assert_eq!(parse_line(&["rx", "--help"]), Command::Help);
    assert_eq!(parse_line(&["rx", "-V"]), Command::Version);
    assert!(!crate::help_text().contains("rx config"));
    assert!(crate::help_text().contains("rx --provider <provider> <harness>"));
    assert!(crate::help_text().contains("rx providers <list|login|logout|use>"));
    assert!(crate::help_text().contains("rx providers models update [provider]"));
    assert!(!crate::help_text().contains("rx providers list\n"));
    assert!(!crate::help_text().contains("rx debug"));
}

#[test]
fn update_parses_yes_flag() {
    assert_eq!(parse_line(&["rx", "update"]), Command::Update(UpdateCommand::Run { yes: false }));
    assert_eq!(
        parse_line(&["rx", "update", "--yes"]),
        Command::Update(UpdateCommand::Run { yes: true })
    );
}

#[test]
fn version_cmp_orders_releases() {
    use std::cmp::Ordering;

    assert_eq!(crate::update::version_cmp("0.1.0", "0.5.0"), Ordering::Less);
    assert_eq!(crate::update::version_cmp("0.5.0", "0.5.0"), Ordering::Equal);
    assert_eq!(crate::update::version_cmp("0.6.0", "0.5.0"), Ordering::Greater);
}

#[test]
fn release_version_matches_core_package() {
    let manifest: toml::Value = toml::from_str(include_str!("../../../Cargo.toml")).unwrap();
    assert_eq!(
        crate::RELEASE_VERSION,
        manifest["workspace"]["package"]["version"].as_str().unwrap()
    );
}

#[test]
fn update_pending_uses_release_version() {
    let release = crate::update::ReleaseInfo {
        version: "0.5.0".to_string(),
        asset_name: String::new(),
        download_url: String::new(),
    };
    assert!(!crate::update::update_pending("0.5.0", &release));
    assert!(crate::update::update_pending("0.4.0", &release));
}

#[test]
fn homebrew_managed_rx_requires_brew_upgrade() {
    for path in [
        "/opt/homebrew/Cellar/recall/0.5.1/bin/rx",
        "/usr/local/Cellar/recall/0.5.1/bin/rx",
        "/home/linuxbrew/.linuxbrew/Cellar/recall/0.5.1/bin/rx",
        "/srv/custom-brew/Cellar/recall/0.5.1/bin/rx",
    ] {
        assert_eq!(
            crate::update::self_update_blocker_for_test(Path::new(path)),
            Some(crate::update::HOMEBREW_UPDATE_HINT)
        );
    }
    assert_eq!(
        crate::update::self_update_blocker_for_test(Path::new("/Users/x/.cargo/bin/rx")),
        None
    );
    assert_eq!(
        crate::update::self_update_blocker_for_test(Path::new(
            "/opt/homebrew/Cellar/other/0.5.1/bin/rx"
        )),
        None
    );
}

#[cfg(unix)]
#[test]
fn homebrew_managed_rx_is_detected_through_bin_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let cellar_rx = dir.path().join("Cellar/recall/0.5.1/bin/rx");
    fs::create_dir_all(cellar_rx.parent().unwrap()).unwrap();
    fs::write(&cellar_rx, b"rx").unwrap();
    let linked_rx = dir.path().join("bin/rx");
    fs::create_dir_all(linked_rx.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&cellar_rx, &linked_rx).unwrap();

    assert_eq!(
        crate::update::self_update_blocker_for_test(&linked_rx),
        Some(crate::update::HOMEBREW_UPDATE_HINT)
    );
    assert_eq!(
        crate::update::homebrew_launch_update_notice_for_test(&linked_rx, "0.6.0"),
        Some("rx 0.6.0 is available — run `brew upgrade recall`".to_string())
    );
}

#[test]
fn argv0_harness_resolves_aliases() {
    assert_eq!(crate::args::argv0_harness("/usr/local/bin/rxc"), Some("claude"));
    assert_eq!(crate::args::argv0_harness("rxx"), Some("codex"));
    assert_eq!(crate::args::argv0_harness("rxo.exe"), Some("opencode"));
    assert_eq!(crate::args::argv0_harness("rxp"), Some("pi"));
    assert_eq!(crate::args::argv0_harness("rxd"), Some("dsh"));
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
            provider: None,
            passthrough: os(&["run", "hello"]),
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
            provider: None,
            passthrough: os(&["--print", "hi"]),
        })
    );
}

#[test]
fn rxd_inserts_dsh() {
    let command = parse_line(&["rxd", "--resume"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Dsh,
            provider: None,
            passthrough: os(&["--resume"]),
        })
    );
}

#[test]
fn rx_dsh_is_a_harness() {
    let command = parse_line(&["rx", "dsh", "--resume"]);
    assert_eq!(
        command,
        Command::Launch(LaunchRequest {
            harness: Harness::Dsh,
            provider: None,
            passthrough: os(&["--resume"]),
        })
    );
}

#[test]
fn missing_provider_value_errors() {
    let error = parse(&args(&["rx", "claude", "--provider"])).unwrap_err();
    assert!(error.to_string().contains("--provider requires a value"), "{error}");
}

#[test]
fn providers_commands_parse() {
    assert_eq!(parse_line(&["rx", "providers"]), Command::Providers(ProvidersCommand::Help));
    assert_eq!(
        parse_line(&["rx", "providers", "list"]),
        Command::Providers(ProvidersCommand::List)
    );
    assert_eq!(
        parse_line(&["rx", "providers", "login"]),
        Command::Providers(ProvidersCommand::Login { provider: None })
    );
    assert_eq!(
        parse_line(&["rx", "providers", "logout"]),
        Command::Providers(ProvidersCommand::Logout { provider: None })
    );
    assert_eq!(
        parse_line(&["rx", "providers", "use"]),
        Command::Providers(ProvidersCommand::Use { provider: None })
    );
    assert_eq!(
        parse_line(&["rx", "providers", "login", "tokener-dev"]),
        Command::Providers(ProvidersCommand::Login { provider: Some("tokener-dev".to_string()) })
    );
    assert_eq!(
        parse_line(&["rx", "providers", "logout", "tokener-dev"]),
        Command::Providers(ProvidersCommand::Logout { provider: Some("tokener-dev".to_string()) })
    );
    assert_eq!(
        parse_line(&["rx", "providers", "use", "tokener-dev"]),
        Command::Providers(ProvidersCommand::Use { provider: Some("tokener-dev".to_string()) })
    );
    assert_eq!(
        parse_line(&["rx", "providers", "models"]),
        Command::Providers(ProvidersCommand::Models(ModelsCommand::Help))
    );
    assert_eq!(
        parse_line(&["rx", "providers", "models", "update"]),
        Command::Providers(ProvidersCommand::Models(ModelsCommand::Update { provider: None }))
    );
    assert_eq!(
        parse_line(&["rx", "providers", "models", "update", "openrouter"]),
        Command::Providers(ProvidersCommand::Models(ModelsCommand::Update {
            provider: Some("openrouter".to_string()),
        }))
    );
}

#[test]
fn provider_command_rejects_multiple_provider_arguments() {
    let error = parse(&args(&["rx", "providers", "use", "openrouter", "tokener"])).unwrap_err();
    assert!(error.to_string().contains("usage: rx providers use [provider]"), "{error}");
}

#[test]
fn providers_models_unknown_command_errors() {
    let error = parse(&args(&["rx", "providers", "models", "list"])).unwrap_err();
    assert!(error.to_string().contains("unknown providers models command: list"), "{error}");
}

#[test]
fn providers_models_update_rejects_multiple_provider_arguments() {
    let error = parse(&args(&["rx", "providers", "models", "update", "openrouter", "tokener"]))
        .unwrap_err();
    assert!(error.to_string().contains("usage: rx providers models update [provider]"), "{error}");
}

#[test]
fn bundled_providers_match_the_admission_list() {
    let admission: serde_json::Value =
        serde_json::from_str(include_str!("../data/provider-admission.json")).unwrap();
    let models_dev = admission["models_dev_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let managed = admission["managed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let mut admitted = Vec::new();
    if let Some(first) = models_dev.first() {
        admitted.push(first.clone());
    }
    if let Some(second) = managed.first() {
        admitted.push(second.clone());
    }
    if models_dev.len() > 1 {
        admitted.extend(models_dev.into_iter().skip(1));
    }
    if managed.len() > 1 {
        admitted.extend(managed.into_iter().skip(1));
    }
    let bundled =
        crate::provider::catalog().iter().map(|provider| provider.id.clone()).collect::<Vec<_>>();
    assert_eq!(bundled, admitted);
    assert_eq!(bundled.first().map(String::as_str), Some("openrouter"));
    assert_eq!(bundled.get(1).map(String::as_str), Some("tokener"));
    let deepseek = crate::provider::find("deepseek").unwrap();
    assert_eq!(deepseek.endpoint, "https://api.deepseek.com");
    assert_eq!(deepseek.env, "DEEPSEEK_API_KEY");
    assert_eq!(deepseek.anthropic_base.as_deref(), Some("https://api.deepseek.com/anthropic"));
    assert_eq!(deepseek.default_context, Some(1_000_000));
    assert_eq!(crate::provider::claude_base(deepseek), "https://api.deepseek.com/anthropic");
    let moonshot = crate::provider::find("moonshotai").unwrap();
    assert_eq!(moonshot.endpoint, "https://api.moonshot.ai/v1");
    assert_eq!(moonshot.anthropic_base.as_deref(), Some("https://api.moonshot.ai/anthropic"));
    let minimax = crate::provider::find("minimax").unwrap();
    assert_eq!(minimax.endpoint, "https://api.minimax.io/v1");
    assert_eq!(minimax.anthropic_base.as_deref(), Some("https://api.minimax.io/anthropic"));
    assert_eq!(crate::provider::claude_base(minimax), "https://api.minimax.io/anthropic");
    let siliconflow = crate::provider::find("siliconflow").unwrap();
    assert_eq!(siliconflow.endpoint, "https://api.siliconflow.com/v1");
    assert_eq!(siliconflow.env, "SILICONFLOW_API_KEY");
    assert_eq!(siliconflow.anthropic_base, None);
    assert_eq!(crate::provider::claude_base(siliconflow), "https://api.siliconflow.com");
    let zai = crate::provider::find("zai").unwrap();
    assert_eq!(zai.endpoint, "https://api.z.ai/api/paas/v4");
    assert_eq!(zai.env, "ZHIPU_API_KEY");
    assert_eq!(zai.anthropic_base.as_deref(), Some("https://api.z.ai/api/anthropic"));
    assert_eq!(crate::provider::claude_base(zai), "https://api.z.ai/api/anthropic");
    assert_eq!(launch::openai_base(&zai.endpoint), "https://api.z.ai/api/paas/v4");
    let zhipu = crate::provider::find("zhipuai").unwrap();
    assert_eq!(zhipu.endpoint, "https://open.bigmodel.cn/api/paas/v4");
    assert_eq!(zhipu.anthropic_base.as_deref(), Some("https://open.bigmodel.cn/api/anthropic"));
    for provider in crate::provider::catalog() {
        crate::provider::validate_id(&provider.id).unwrap();
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
fn unconfigured_launch_is_passthrough() {
    let (_dir, paths) = temp_paths();
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Claude,
            provider: None,
            passthrough: os(&["--resume", "abc"]),
        },
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap();
    assert_eq!(plan.program, PathBuf::from("claude"));
    assert_eq!(plan.args, os(&["--resume", "abc"]));
    assert!(plan.env_set.is_empty());
    assert!(plan.stderr_note.as_deref().unwrap().contains("no provider configured"));
}

#[test]
fn unconfigured_dsh_still_boots_tui_profile() {
    let (_dir, paths) = temp_paths();
    let plan = launch::plan(
        &LaunchRequest { harness: Harness::Dsh, provider: None, passthrough: os(&["--resume"]) },
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap();
    assert_eq!(plan.program, PathBuf::from("dsh"));
    assert_eq!(plan.args, os(&["--profile", "dsh-tui", "--resume"]));
    assert!(plan.env_set.is_empty());
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
            provider: Some("openrouter".to_string()),
            passthrough: os(&["fix it"]),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, PathBuf::from("claude"));
    assert_eq!(plan.args, os(&["fix it"]));
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
    assert!(plan.stderr_note.as_deref().unwrap().contains("catalog seed failed"));
}

#[test]
fn configured_openrouter_credential_is_the_implicit_default() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "OPENROUTER_API_KEY".to_string(),
        "sk-or-test".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest { harness: Harness::Codex, provider: None, passthrough: Vec::new() },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.args[1], "model_provider=\"openrouter\"");
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
            provider: Some("tokener".to_string()),
            passthrough: os(&["fix it"]),
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
            provider: Some("openrouter".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, PathBuf::from("codex"));
    assert_eq!(plan.args[0], "-c");
    assert_eq!(plan.args[1], "model_provider=\"openrouter\"");
    assert!(arg_str(&plan.args[3]).contains("base_url=\"https://openrouter.ai/api/v1\""));
    assert!(arg_str(&plan.args[3]).contains("wire_api=\"responses\""));
    assert!(arg_str(&plan.args[3]).contains("supports_websockets=false"));
    assert!(arg_str(&plan.args[3]).contains("auth={command=\"sh\""));
    assert!(!arg_str(&plan.args[3]).contains("env_key="));
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
            provider: Some("openrouter".to_string()),
            passthrough: os(&["run", "hello"]),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, PathBuf::from("opencode"));
    assert_eq!(plan.args, os(&["run", "hello"]));
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
fn opencode_non_generated_catalog_failure_degrades_to_base_config() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_error(500);
    let provider = crate::provider::find("openrouter").unwrap();
    let config =
        crate::opencode::config_content("openrouter", provider, &base_url, "sk-test", &paths, true)
            .unwrap();
    server.join().unwrap();
    let document: serde_json::Value = serde_json::from_str(&config).unwrap();
    assert_eq!(document["provider"]["openrouter"]["options"]["baseURL"], format!("{base_url}/v1"));
    assert!(document["provider"]["openrouter"].get("models").is_none());
}

#[test]
fn dsh_deepseek_uses_official_adapter_and_clears_pi_ai_routes() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "DEEPSEEK_API_KEY".to_string(),
        "sk-ds-test".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Dsh,
            provider: Some("deepseek".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, PathBuf::from("dsh"));
    assert_eq!(plan.args[0], "--profile");
    assert_eq!(plan.args[1], "dsh-tui");
    assert_eq!(plan.args[2], "--patch");
    assert_eq!(Path::new(&plan.args[3]), paths.dir.join("dsh").join("launch.cordis.yml"));
    assert_eq!(plan.env_set, vec![("DEEPSEEK_API_KEY".to_string(), "sk-ds-test".to_string())]);
    let patch = fs::read_to_string(paths.dir.join("dsh").join("launch.cordis.yml")).unwrap();
    assert!(patch.contains("id: settings"));
    assert!(!patch.contains("disabled: true"));
    let settings: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(paths.dir.join("dsh").join("settings.yaml")).unwrap(),
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
    let env = EnvLookup::isolated(HashMap::from([(
        "TOKENER_API_KEY".to_string(),
        "sk-tokener".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Dsh,
            provider: Some("tokener".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    server.join().unwrap();
    assert_eq!(plan.env_set, vec![("TOKENER_API_KEY".to_string(), "sk-tokener".to_string())]);
    let settings: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(paths.dir.join("dsh").join("settings.yaml")).unwrap(),
    )
    .unwrap();
    let models = settings["llm-pi-ai"]["providers"]["tokener"]["models"].as_sequence().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["id"], "kimi-k3");
    assert_eq!(models[1]["id"], "gpt-5.6-sol");
    assert_eq!(settings["agent-default-model"]["provider"], "tokener");
    assert!(settings["agent-default-model"].get("model").is_none());
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
    let env = EnvLookup::isolated(HashMap::from([
        ("TOKENER_API_KEY".to_string(), "sk-tokener".to_string()),
        ("DSH_HOME".to_string(), dsh_home.display().to_string()),
    ]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Dsh,
            provider: Some("tokener".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    server.join().unwrap();
    assert_eq!(plan.env_set, vec![("TOKENER_API_KEY".to_string(), "sk-tokener".to_string())]);
    let patch = fs::read_to_string(Path::new(&plan.args[3])).unwrap();
    assert!(patch.contains("id: settings"));
    assert!(patch.contains("id: llm-deepseek"));
    assert!(patch.contains("disabled: true"));
    let overlay = paths.dir.join("dsh").join("settings.yaml");
    assert!(patch.contains(&overlay.display().to_string()));
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
fn pi_openrouter_injects_provider_without_extension() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "OPENROUTER_API_KEY".to_string(),
        "sk-or-test".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Pi,
            provider: Some("openrouter".to_string()),
            passthrough: os(&["--print", "hi"]),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.program, PathBuf::from("pi"));
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
    let provider = crate::provider::find("tokener").unwrap();
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

    crate::pi::prepare("tokener", provider, &base_url, "sk-test", &paths, &env).unwrap();
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
            provider: Some("openrouter".to_string()),
            passthrough: os(&["--model", "anthropic/claude-sonnet-4.6"]),
        },
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
    let env = EnvLookup::isolated(HashMap::from([(
        "TOKENER_API_KEY".to_string(),
        "sk-tokener".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Codex,
            provider: Some("tokener".to_string()),
            passthrough: os(&["exec", "cargo test"]),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(plan.args[1], "model_provider=\"tokener\"");
    assert!(arg_str(&plan.args[3]).contains("base_url=\"https://api.tokener.dev/v1\""));
    assert!(!plan.args.iter().any(|arg| arg_str(arg).starts_with("model=")));
    assert_eq!(&plan.args[4..], os(&["exec", "cargo test"]));
}

#[test]
fn missing_key_errors_before_exec() {
    let (_dir, paths) = temp_paths();
    let error = launch::plan(
        &LaunchRequest {
            harness: Harness::Claude,
            provider: Some("openrouter".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
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
    let env = EnvLookup::isolated(HashMap::from([(
        "TOKENER_API_KEY".to_string(),
        "sk-prod".to_string(),
    )]));

    let error = launch::plan(
        &LaunchRequest { harness: Harness::Codex, provider: None, passthrough: Vec::new() },
        &paths,
        &env,
    )
    .unwrap_err();

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

    let env = EnvLookup::isolated(HashMap::new());
    let plan = launch::plan(
        &LaunchRequest { harness: Harness::Claude, provider: None, passthrough: Vec::new() },
        &paths,
        &env,
    )
    .unwrap();
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

    crate::run_with(
        args(&["rx", "providers", "use", "openrouter"]),
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap();

    let loaded = config::load(&paths).unwrap().unwrap();
    assert_eq!(loaded.default_provider.as_deref(), Some("openrouter"));
}

#[test]
fn provider_logout_argument_removes_key_without_a_terminal() {
    let (_dir, paths) = temp_paths();
    config::login(&paths, "tokener-dev", "sk-dev".to_string()).unwrap();

    crate::run_with(
        args(&["rx", "providers", "logout", "tokener-dev"]),
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap();

    assert!(config::stored_key(&paths, "tokener-dev").unwrap().is_none());
}

#[test]
fn providers_keep_independent_keys_and_behavior() {
    let (dir, paths) = temp_paths();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let dev_base_url = format!("http://{}", listener.local_addr().unwrap());
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
    let env = EnvLookup::isolated(HashMap::from([(
        "PI_CODING_AGENT_DIR".to_string(),
        agent_dir.display().to_string(),
    )]));
    let codex = launch::plan(
        &LaunchRequest {
            harness: Harness::Codex,
            provider: Some("tokener-dev".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert_eq!(codex.args[1], "model_provider=\"tokener-dev\"");
    assert!(arg_str(&codex.args[3]).contains("model_providers.tokener-dev="));
    assert!(arg_str(&codex.args[3]).contains(&format!("base_url=\"{dev_base_url}/v1\"")));
    assert_eq!(codex.env_set, vec![("TOKENER_DEV_API_KEY".to_string(), "sk-dev".to_string())]);

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 2048];
        let size = stream.read(&mut request).unwrap();
        let req = String::from_utf8_lossy(&request[..size]);
        assert!(req.contains("GET /v1/models "), "{req}");
        let body = r#"{"data":[{"id":"gpt-dev"}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });

    let opencode = launch::plan(
        &LaunchRequest {
            harness: Harness::OpenCode,
            provider: Some("tokener-dev".to_string()),
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
            provider: Some("tokener-dev".to_string()),
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
            provider: Some("tokener-prod".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert!(
        claude.env_set.iter().any(
            |(key, value)| key == "ANTHROPIC_BASE_URL" && value == "https://prod.provider.test"
        )
    );
    assert!(
        claude
            .env_set
            .iter()
            .any(|(key, value)| key == "ANTHROPIC_AUTH_TOKEN" && value == "sk-prod")
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

    let error = launch::plan(
        &LaunchRequest { harness: Harness::Pi, provider: None, passthrough: Vec::new() },
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("invalid provider name '../prod'"), "{error}");
}

#[test]
fn url_helpers_strip_or_add_v1() {
    assert_eq!(
        launch::anthropic_base("https://openrouter.ai/api/v1/"),
        "https://openrouter.ai/api"
    );
    assert_eq!(launch::openai_base("https://api.tokener.dev"), "https://api.tokener.dev/v1");
    assert_eq!(launch::openai_base("https://openrouter.ai/api/v1"), "https://openrouter.ai/api/v1");
    assert_eq!(launch::openai_base("https://api.z.ai/api/paas/v4"), "https://api.z.ai/api/paas/v4");
    assert_eq!(
        launch::openai_base("https://api.z.ai/api/coding/paas/v4/"),
        "https://api.z.ai/api/coding/paas/v4"
    );
}

#[test]
fn claude_base_uses_explicit_anthropic_origin() {
    let openrouter = crate::provider::find("openrouter").unwrap();
    assert_eq!(crate::provider::claude_base(openrouter), "https://openrouter.ai/api");

    let override_entry = crate::config::ProviderConfig {
        anthropic_base: Some("https://api.deepseek.com/anthropic/v1".to_string()),
        ..crate::config::ProviderConfig::default()
    };
    let overridden = crate::provider::resolve("openrouter", Some(&override_entry)).unwrap();
    assert_eq!(crate::provider::claude_base(&overridden), "https://api.deepseek.com/anthropic");
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
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Claude,
            provider: Some("moonshot".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap();
    assert!(
        plan.env_set
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://api.moonshot.ai/anthropic")
    );
    let codex = launch::plan(
        &LaunchRequest {
            harness: Harness::Codex,
            provider: Some("moonshot".to_string()),
            passthrough: Vec::new(),
        },
        &paths,
        &EnvLookup::isolated(HashMap::new()),
    )
    .unwrap();
    assert!(arg_str(&codex.args[3]).contains("base_url=\"https://api.moonshot.ai/v1\""));
}

#[test]
fn base_url_override_clears_bundled_anthropic_base() {
    let deepseek = crate::provider::find("deepseek").unwrap();
    assert_eq!(crate::provider::claude_base(deepseek), "https://api.deepseek.com/anthropic");

    let override_entry = crate::config::ProviderConfig {
        base_url: Some("https://proxy.example.com/v1".to_string()),
        ..crate::config::ProviderConfig::default()
    };
    let overridden = crate::provider::resolve("deepseek", Some(&override_entry)).unwrap();
    assert_eq!(overridden.endpoint, "https://proxy.example.com/v1");
    assert_eq!(crate::provider::claude_base(&overridden), "https://proxy.example.com");
}

#[test]
fn openai_data_becomes_codex_model_catalog_json() {
    let models = crate::catalog::parse_openai_models(
        r#"{"data":[{"id":"gpt-5.6-sol","name":"GPT 5.6 Sol"},{"id":"claude-sonnet-5"}]}"#,
    )
    .unwrap();
    let catalog = crate::catalog::synthesize_codex_catalog(&models);
    assert_eq!(catalog["models"][0]["slug"], "gpt-5.6-sol");
    assert_eq!(catalog["models"][0]["display_name"], "GPT 5.6 Sol");
    assert_eq!(catalog["models"][0]["visibility"], "list");
    assert_eq!(catalog["models"][0]["supported_in_api"], true);
    assert_eq!(catalog["models"][0]["context_window"], 200_000);
    assert_eq!(catalog["models"][0]["supported_reasoning_levels"], serde_json::json!([]));
    assert_eq!(catalog["models"][0]["shell_type"], "shell_command");
    assert_eq!(catalog["models"][0]["priority"], 1);
    assert_eq!(catalog["models"][0]["truncation_policy"]["mode"], "bytes");
    assert_eq!(catalog["models"][0]["base_instructions"], "");
    assert_eq!(catalog["models"][1]["slug"], "claude-sonnet-5");
    assert_eq!(catalog["models"][1]["display_name"], "claude-sonnet-5");
}

#[test]
fn openai_data_without_context_still_seeds_claude_picker() {
    let models =
        crate::catalog::parse_openai_models(r#"{"data":[{"id":"openai/gpt-5.6-sol"}]}"#).unwrap();
    let seed = crate::claude_catalog::seed_from_listed("tokener", &models);
    assert_eq!(seed.provider_id, "tokener");
    assert_eq!(seed.additional_model_options.len(), 1);
    assert_eq!(seed.additional_model_options[0].value, "openai/gpt-5.6-sol");
}

#[test]
fn openai_body_without_context_falls_back_to_listed_claude_seed() {
    let body = r#"{"data":[{"id":"deepseek-v4-flash"},{"id":"deepseek-v4-pro"}]}"#;
    let models = crate::catalog::parse_openai_models(body).unwrap();
    let seed = crate::claude_catalog::seed_from_openai_body("deepseek", body, &models);
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
    let models = crate::catalog::parse_openai_models(body).unwrap();
    let seed = crate::claude_catalog::seed_from_openai_body("tokener", body, &models);
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
fn isolated_codex_plan_does_not_write_model_catalog_json() {
    let (_dir, paths) = temp_paths();
    let env = EnvLookup::isolated(HashMap::from([(
        "TOKENER_API_KEY".to_string(),
        "sk-tokener".to_string(),
    )]));
    let plan = launch::plan(
        &LaunchRequest {
            harness: Harness::Codex,
            provider: Some("tokener".to_string()),
            passthrough: os(&["exec"]),
        },
        &paths,
        &env,
    )
    .unwrap();
    assert!(!plan.args.iter().any(|arg| arg_str(arg).contains("model_catalog_json")));
    assert!(!paths.dir.join("catalogs").exists());
}

#[test]
fn prepare_codex_catalog_writes_processed_provider_file() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models(
        r#"{"data":[{"id":"gpt-5.6-sol","name":"Sol","context_length":200000}]}"#,
    );
    let path = crate::catalog::prepare_codex_catalog(&paths, "tokener", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    server.join().unwrap();
    assert_eq!(path, paths.dir.join("catalogs/tokener.json"));
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(document["models"][0]["slug"], "gpt-5.6-sol");
    assert_eq!(document["models"][0]["display_name"], "Sol");
    assert!(document.get("fetched_at").is_none());
    assert!(paths.dir.join("catalogs/tokener.claude.json").is_file());
    assert!(paths.dir.join("catalogs/tokener.opencode.json").is_file());
    assert!(paths.dir.join("catalogs/tokener.pi.json").is_file());
    assert!(paths.dir.join("catalogs/tokener.meta.json").is_file());
}

#[test]
fn missing_context_uses_models_dev_provider_default() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models(r#"{"data":[{"id":"deepseek-v4-flash"}]}"#);
    let path = crate::catalog::prepare_codex_catalog(&paths, "deepseek", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    server.join().unwrap();
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(document["models"][0]["context_window"], 1_000_000);
    let seed: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(paths.dir.join("catalogs/deepseek.claude.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(seed["additional_model_options"][0]["description"], "1M context");
}

#[test]
fn providers_models_update_bypasses_fresh_cache() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) =
        serve_openai_models_times(r#"{"data":[{"id":"first"},{"id":"second"}]}"#, 2);
    crate::catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    let count = crate::catalog::update_models(&paths, "openrouter", &base_url, "sk-test").unwrap();
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
        ProvidersCommand::Models(ModelsCommand::Update { provider: Some("lab".to_string()) }),
        &paths,
        &EnvLookup::isolated(HashMap::new()),
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
    let first = crate::catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    server.join().unwrap();
    let second = crate::catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
    let models =
        crate::catalog::load_opencode_models(&paths, "openrouter", &base_url, "sk-test", false)
            .unwrap();
    assert!(models.contains_key("gpt-5.6-sol"));
    let pi =
        crate::catalog::load_pi_models(&paths, "openrouter", &base_url, "sk-test", false).unwrap();
    assert_eq!(pi[0]["id"], "gpt-5.6-sol");
}

#[test]
fn catalogs_are_written_per_provider() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models_times(r#"{"data":[{"id":"shared-model"}]}"#, 2);
    crate::catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-or")
        .unwrap()
        .unwrap();
    crate::catalog::prepare_codex_catalog(&paths, "tokener", &base_url, "sk-tokener")
        .unwrap()
        .unwrap();
    server.join().unwrap();
    assert!(paths.dir.join("catalogs/openrouter.json").is_file());
    assert!(paths.dir.join("catalogs/tokener.json").is_file());
    assert_ne!(paths.dir.join("catalogs/openrouter.json"), paths.dir.join("catalogs/tokener.json"));
}

#[test]
fn expired_catalog_is_refetched() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models_times(r#"{"data":[{"id":"gpt-5.6-sol"}]}"#, 2);
    crate::catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    let meta_path = paths.dir.join("catalogs/openrouter.meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
    meta["fetched_at"] = serde_json::json!(0);
    fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
    crate::catalog::prepare_codex_catalog(&paths, "openrouter", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    server.join().unwrap();
}

#[test]
fn catalog_endpoint_change_misses_cache() {
    let (_dir, paths) = temp_paths();
    let (first_url, first_server) = serve_openai_models(r#"{"data":[{"id":"first"}]}"#);
    crate::catalog::prepare_codex_catalog(&paths, "tokener", &first_url, "sk-test")
        .unwrap()
        .unwrap();
    first_server.join().unwrap();
    let (second_url, second_server) = serve_openai_models(r#"{"data":[{"id":"second"}]}"#);
    let path = crate::catalog::prepare_codex_catalog(&paths, "tokener", &second_url, "sk-test")
        .unwrap()
        .unwrap();
    second_server.join().unwrap();
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(document["models"][0]["slug"], "second");
}

#[test]
fn catalog_refresh_failure_does_not_reuse_other_endpoint() {
    let (_dir, paths) = temp_paths();
    let (first_url, first_server) = serve_openai_models(r#"{"data":[{"id":"first"}]}"#);
    crate::catalog::prepare_codex_catalog(&paths, "tokener", &first_url, "sk-test")
        .unwrap()
        .unwrap();
    first_server.join().unwrap();
    let (error_url, error_server) = serve_openai_error(500);
    let error = crate::catalog::prepare_codex_catalog(&paths, "tokener", &error_url, "sk-test")
        .unwrap_err();
    error_server.join().unwrap();
    assert!(error.to_string().contains("HTTP 500"), "{error:#}");
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(paths.dir.join("catalogs/tokener.json")).unwrap())
            .unwrap();
    assert_eq!(document["models"][0]["slug"], "first");
}

#[test]
fn catalog_refresh_failure_reuses_same_endpoint() {
    let (_dir, paths) = temp_paths();
    let (base_url, server) = serve_openai_models_then_error(r#"{"data":[{"id":"first"}]}"#, 500);
    crate::catalog::prepare_codex_catalog(&paths, "tokener", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    let meta_path = paths.dir.join("catalogs/tokener.meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
    meta["fetched_at"] = serde_json::json!(0);
    fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
    let path = crate::catalog::prepare_codex_catalog(&paths, "tokener", &base_url, "sk-test")
        .unwrap()
        .unwrap();
    server.join().unwrap();
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(document["models"][0]["slug"], "first");
}

#[test]
fn claude_seed_uses_openai_models_and_merges_from_cache() {
    let (_dir, paths) = temp_paths();
    let config_dir = tempfile::tempdir().unwrap();
    let (base_url, server) = serve_openai_models(
        r#"{"data":[{"id":"claude-sonnet-5","display_name":"Sonnet 5","max_input_tokens":200000}]}"#,
    );
    let env = EnvLookup::isolated(HashMap::from([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.path().display().to_string(),
    )]));
    let first = crate::claude_catalog::try_seed_user_catalog(
        &paths,
        "openrouter",
        &base_url,
        "sk-test",
        &env,
    )
    .unwrap();
    server.join().unwrap();
    assert!(matches!(first, crate::claude_catalog::SeedOutcome::Seeded { model_count: 1 }));
    let cached: crate::claude_catalog::SeedCaches = serde_json::from_str(
        &fs::read_to_string(paths.dir.join("catalogs/openrouter.claude.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cached.provider_id, "openrouter");
    let second = crate::claude_catalog::try_seed_user_catalog(
        &paths,
        "openrouter",
        &base_url,
        "sk-test",
        &env,
    )
    .unwrap();
    assert!(matches!(second, crate::claude_catalog::SeedOutcome::Seeded { model_count: 1 }));
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(config_dir.path().join(".claude.json")).unwrap())
            .unwrap();
    assert_eq!(document["additionalModelOptionsCache"][0]["value"], "claude-sonnet-5");
}

#[test]
fn debug_is_not_a_harness() {
    let error = parse(&args(&["rx", "debug", "models"])).unwrap_err();
    assert!(error.to_string().contains("unknown harness: debug"), "{error}");
}

#[test]
fn generated_provider_seeded_plan_uses_auth_token_and_settings() {
    let plan = launch::inject_claude_generated_seeded_for_test(
        &LaunchRequest {
            harness: Harness::Claude,
            provider: Some("tokener".to_string()),
            passthrough: os(&["fix it"]),
        },
        "TOKENER_API_KEY",
        "http://localhost:8080",
        "sk-tokener",
        None,
    );
    assert_eq!(plan.args[0], "--settings");
    assert!(arg_str(&plan.args[1]).contains("TOKENER_API_KEY"));
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
fn claude_seed_replaces_previous_provider_catalog_without_dropping_unowned_entries() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    let openrouter = crate::catalog::parse_openai_models(
        r#"{"data":[{"id":"google/gemini-3.7-flash","name":"Gemini","context_length":200000}]}"#,
    )
    .unwrap();
    crate::claude_catalog::write_seed_for_test(
        &config_path,
        &crate::claude_catalog::seed_from_listed("openrouter", &openrouter),
    )
    .unwrap();

    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    document["additionalModelOptionsCache"].as_array_mut().unwrap().push(serde_json::json!({
        "value": "user-model",
        "label": "User",
        "description": "manual",
    }));
    document["modelAccessCache"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({ "apiName": "user-model", "entitled": true }));
    document["additionalModelCostsCache"]["user-model"] =
        serde_json::json!({ "inputTokens": 42.0 });
    document["autoCompactWindowsCache"]["user-model"] = serde_json::json!(123_456);
    document["cachedGrowthBookFeatures"]["tengu_tool_search_unsupported_models"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("user-model"));
    fs::write(&config_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    let tokener = crate::catalog::parse_openai_models(
        r#"{"data":[{"id":"claude-sonnet-5","name":"Sonnet 5","context_length":200000}]}"#,
    )
    .unwrap();
    crate::claude_catalog::write_seed_for_test(
        &config_path,
        &crate::claude_catalog::seed_from_listed("tokener", &tokener),
    )
    .unwrap();
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let values = document["additionalModelOptionsCache"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["value"].as_str())
        .collect::<Vec<_>>();
    assert!(!values.contains(&"google/gemini-3.7-flash"));
    assert!(values.contains(&"claude-sonnet-5"));
    assert!(values.contains(&"user-model"));
    assert_eq!(document["rxSeededCatalog"]["provider_id"], "tokener");

    let access = document["modelAccessCache"].as_array().unwrap();
    assert!(!access.iter().any(|entry| entry["apiName"] == "google/gemini-3.7-flash"));
    assert!(access.iter().any(|entry| entry["apiName"] == "claude-sonnet-5"));
    assert!(access.iter().any(|entry| entry["apiName"] == "user-model"));
    assert_eq!(document["additionalModelCostsCache"]["user-model"]["inputTokens"], 42.0);
    assert_eq!(document["autoCompactWindowsCache"]["user-model"], 123_456);
    let denylist = document["cachedGrowthBookFeatures"]["tengu_tool_search_unsupported_models"]
        .as_array()
        .unwrap();
    assert!(!denylist.iter().any(|entry| entry == "google/gemini-3.7-flash"));
    assert!(denylist.iter().any(|entry| entry == "user-model"));
}

#[test]
fn claude_seed_first_run_preserves_preexisting_catalog_entries() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    fs::write(
        &config_path,
        r#"{"additionalModelOptionsCache":[{"value":"user-model","label":"User","description":"manual"}],"modelAccessCache":[{"apiName":"user-model","entitled":true}]}"#,
    )
    .unwrap();
    let models = crate::catalog::parse_openai_models(
        r#"{"data":[{"id":"claude-sonnet-5","name":"Sonnet 5","context_length":200000}]}"#,
    )
    .unwrap();
    crate::claude_catalog::write_seed_for_test(
        &config_path,
        &crate::claude_catalog::seed_from_listed("openrouter", &models),
    )
    .unwrap();

    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let values = document["additionalModelOptionsCache"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["value"].as_str())
        .collect::<Vec<_>>();
    assert!(values.contains(&"user-model"), "preexisting entry was dropped: {values:?}");
    assert!(values.contains(&"claude-sonnet-5"));
    assert!(
        document["modelAccessCache"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["apiName"] == "user-model"),
        "preexisting model access entry was dropped"
    );
    assert_eq!(document["rxSeededCatalog"]["provider_id"], "openrouter");
}

#[test]
fn claude_seed_refreshes_and_removes_unmodified_owned_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    let first_seed = crate::claude_catalog::build_seed(&[
        crate::claude_catalog::UserModel {
            id: "anthropic/claude-current".to_string(),
            name: Some("Current Old".to_string()),
            context_length: Some(200_000),
            canonical_slug: None,
            supported_efforts: vec!["high".to_string()],
            pricing: Some(crate::claude_catalog::Pricing {
                prompt: Some("0.000001".to_string()),
                completion: Some("0.000003".to_string()),
                input_cache_read: None,
                input_cache_write: None,
                web_search: None,
            }),
        },
        crate::claude_catalog::UserModel {
            id: "anthropic/claude-removed".to_string(),
            name: Some("Removed".to_string()),
            context_length: Some(200_000),
            canonical_slug: None,
            supported_efforts: vec![],
            pricing: Some(crate::claude_catalog::Pricing {
                prompt: Some("0.000001".to_string()),
                completion: Some("0.000003".to_string()),
                input_cache_read: None,
                input_cache_write: None,
                web_search: None,
            }),
        },
    ]);
    crate::claude_catalog::write_seed_for_test(&config_path, &first_seed).unwrap();

    let second_seed = crate::claude_catalog::build_seed(&[crate::claude_catalog::UserModel {
        id: "anthropic/claude-current".to_string(),
        name: Some("Current New".to_string()),
        context_length: Some(300_000),
        canonical_slug: None,
        supported_efforts: vec!["max".to_string()],
        pricing: Some(crate::claude_catalog::Pricing {
            prompt: Some("0.000002".to_string()),
            completion: Some("0.000004".to_string()),
            input_cache_read: None,
            input_cache_write: None,
            web_search: None,
        }),
    }]);
    crate::claude_catalog::write_seed_for_test(&config_path, &second_seed).unwrap();

    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let options = document["additionalModelOptionsCache"].as_array().unwrap();
    assert!(options.iter().any(|entry| {
        entry["value"] == "claude-current"
            && entry["label"] == "Current New"
            && entry["description"] == "300K context"
    }));
    assert!(!options.iter().any(|entry| entry["value"] == "claude-removed"));

    let access = document["modelAccessCache"].as_array().unwrap();
    assert!(
        access.iter().any(|entry| {
            entry["apiName"] == "claude-current" && entry["maxEffortLevel"] == "max"
        })
    );
    assert!(!access.iter().any(|entry| entry["apiName"] == "claude-removed"));
    assert_eq!(document["additionalModelCostsCache"]["claude-current"]["inputTokens"], 2.0);
    assert!(document["additionalModelCostsCache"].get("claude-removed").is_none());
    assert_eq!(document["autoCompactWindowsCache"]["claude-current"], 300_000);
    assert!(document["autoCompactWindowsCache"].get("claude-removed").is_none());
}

#[test]
fn claude_seed_reclaims_modified_owned_payloads_and_preserves_unowned() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    fs::write(
        &config_path,
        r#"{"additionalModelOptionsCache":[{"value":"user-owned","label":"User","description":"manual"}]}"#,
    )
    .unwrap();
    let seed = crate::claude_catalog::build_seed(&[crate::claude_catalog::UserModel {
        id: "anthropic/claude-edited".to_string(),
        name: Some("RX".to_string()),
        context_length: Some(200_000),
        canonical_slug: None,
        supported_efforts: vec!["high".to_string()],
        pricing: Some(crate::claude_catalog::Pricing {
            prompt: Some("0.000001".to_string()),
            completion: Some("0.000003".to_string()),
            input_cache_read: None,
            input_cache_write: None,
            web_search: None,
        }),
    }]);
    crate::claude_catalog::write_seed_for_test(&config_path, &seed).unwrap();

    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let edited_option = document["additionalModelOptionsCache"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["value"] == "claude-edited")
        .unwrap();
    edited_option["label"] = serde_json::json!("User Override");
    let edited_access = document["modelAccessCache"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["apiName"] == "claude-edited")
        .unwrap();
    edited_access["entitled"] = serde_json::json!(false);
    document["additionalModelCostsCache"]["claude-edited"]["inputTokens"] = serde_json::json!(99.0);
    document["autoCompactWindowsCache"]["claude-edited"] = serde_json::json!(123_456);
    fs::write(&config_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    crate::claude_catalog::write_seed_for_test(
        &config_path,
        &crate::claude_catalog::SeedCaches::default(),
    )
    .unwrap();
    crate::claude_catalog::write_seed_for_test(&config_path, &seed).unwrap();

    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let options = document["additionalModelOptionsCache"].as_array().unwrap();
    assert!(
        options.iter().any(|entry| { entry["value"] == "claude-edited" && entry["label"] == "RX" })
    );
    assert!(options.iter().any(|entry| entry["value"] == "user-owned"));
    assert!(
        document["modelAccessCache"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["apiName"] == "claude-edited" && entry["entitled"] == true })
    );
    assert_eq!(document["additionalModelCostsCache"]["claude-edited"]["inputTokens"], 1.0);
    assert_eq!(document["autoCompactWindowsCache"]["claude-edited"], 200_000);
}

#[test]
fn claude_seed_recreates_deleted_owned_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    let seed = crate::claude_catalog::build_seed(&[crate::claude_catalog::UserModel {
        id: "openai/deleted".to_string(),
        name: Some("Deleted".to_string()),
        context_length: Some(200_000),
        canonical_slug: None,
        supported_efforts: vec!["high".to_string()],
        pricing: Some(crate::claude_catalog::Pricing {
            prompt: Some("0.000001".to_string()),
            completion: Some("0.000003".to_string()),
            input_cache_read: None,
            input_cache_write: None,
            web_search: None,
        }),
    }]);
    crate::claude_catalog::write_seed_for_test(&config_path, &seed).unwrap();

    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    document["additionalModelOptionsCache"]
        .as_array_mut()
        .unwrap()
        .retain(|entry| entry["value"] != "openai/deleted");
    document["modelAccessCache"]
        .as_array_mut()
        .unwrap()
        .retain(|entry| entry["apiName"] != "openai/deleted");
    document["additionalModelCostsCache"].as_object_mut().unwrap().remove("openai/deleted");
    document["autoCompactWindowsCache"].as_object_mut().unwrap().remove("openai/deleted");
    document["cachedGrowthBookFeatures"]["tengu_tool_search_unsupported_models"]
        .as_array_mut()
        .unwrap()
        .retain(|entry| entry != "openai/deleted");
    fs::write(&config_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    crate::claude_catalog::write_seed_for_test(&config_path, &seed).unwrap();
    crate::claude_catalog::write_seed_for_test(&config_path, &seed).unwrap();

    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(
        document["additionalModelOptionsCache"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["value"] == "openai/deleted")
    );
    assert!(
        document["modelAccessCache"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["apiName"] == "openai/deleted")
    );
    assert_eq!(document["additionalModelCostsCache"]["openai/deleted"]["inputTokens"], 1.0);
    assert_eq!(document["autoCompactWindowsCache"]["openai/deleted"], 200_000);
    assert!(
        document["cachedGrowthBookFeatures"]["tengu_tool_search_unsupported_models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "openai/deleted")
    );
}

#[test]
fn claude_seed_does_not_claim_unmarked_matching_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    fs::write(
        &config_path,
        r#"{"additionalModelOptionsCache":[{"value":"claude-existing","label":"User","description":"manual"}]}"#,
    )
    .unwrap();
    let seed = crate::claude_catalog::build_seed(&[crate::claude_catalog::UserModel {
        id: "anthropic/claude-existing".to_string(),
        name: Some("RX".to_string()),
        context_length: Some(200_000),
        canonical_slug: None,
        supported_efforts: vec![],
        pricing: None,
    }]);
    crate::claude_catalog::write_seed_for_test(&config_path, &seed).unwrap();
    crate::claude_catalog::write_seed_for_test(
        &config_path,
        &crate::claude_catalog::SeedCaches::default(),
    )
    .unwrap();

    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(document["additionalModelOptionsCache"].as_array().unwrap().iter().any(|entry| {
        entry["value"] == "claude-existing"
            && entry["label"] == "User"
            && entry["description"] == "manual"
    }));
}

#[test]
fn concurrent_claude_seed_writes_leave_one_complete_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    fs::write(
        &config_path,
        serde_json::to_vec(&serde_json::json!({
            "padding": "x".repeat(4_000_000)
        }))
        .unwrap(),
    )
    .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let mut writers = Vec::new();
    for id in ["anthropic/claude-race-a", "anthropic/claude-race-b"] {
        let path = config_path.clone();
        let barrier = Arc::clone(&barrier);
        let caches = crate::claude_catalog::build_seed(&[crate::claude_catalog::UserModel {
            id: id.to_string(),
            name: Some(id.to_string()),
            context_length: Some(200_000),
            canonical_slug: None,
            supported_efforts: vec![],
            pricing: None,
        }]);
        writers.push(thread::spawn(move || {
            barrier.wait();
            crate::claude_catalog::write_seed_for_test(&path, &caches).unwrap();
        }));
    }
    for writer in writers {
        writer.join().unwrap();
    }
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let values = document["additionalModelOptionsCache"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["value"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 1);
    assert!(matches!(values[0], "claude-race-a" | "claude-race-b"));
    assert_eq!(
        document["rxSeededCatalog"]["additionalModelOptionsCache"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        values
    );
    assert_eq!(document["rxSeededCatalog"]["modelAccessCache"].as_object().unwrap().len(), 1);
    assert_eq!(
        document["rxSeededCatalog"]["autoCompactWindowsCache"].as_object().unwrap().len(),
        1
    );
}

#[test]
fn claude_seed_remerges_external_config_changes() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".claude.json");
    fs::write(&config_path, r#"{"before":true}"#).unwrap();
    let caches = crate::claude_catalog::build_seed(&[crate::claude_catalog::UserModel {
        id: "anthropic/claude-race-rx".to_string(),
        name: Some("Race RX".to_string()),
        context_length: Some(200_000),
        canonical_slug: None,
        supported_efforts: vec![],
        pricing: None,
    }]);
    let external_path = config_path.clone();
    crate::claude_catalog::write_seed_with_hook_for_test(&config_path, &caches, |attempt| {
        if attempt == 0 {
            fs::write(&external_path, r#"{"external":"keep"}"#).unwrap();
        }
    })
    .unwrap();
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(document["external"], "keep");
    assert!(
        document["additionalModelOptionsCache"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["value"] == "claude-race-rx")
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
            provider: Some("openrouter".to_string()),
            passthrough: os(&["fix it"]),
        },
        "https://openrouter.ai/api",
        "sk-or-test",
        Some("~anthropic/claude-sonnet-latest"),
        crate::claude_catalog::SeedOutcome::Seeded { model_count: 2 },
    );
    assert_eq!(plan.args[0], "--settings");
    assert!(arg_str(&plan.args[1]).contains("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"));
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
    fs::write(
        recall.join("rx.toml"),
        "default_provider = \"openrouter\"\n\n[provider.openrouter]\n",
    )
    .unwrap();
    let key = std::env::var("OPENROUTER_API_KEY")
        .or_else(|_| std::env::var("ORI_OPENROUTER_API_KEY"))
        .expect("set OPENROUTER_API_KEY for live seed test");
    fs::write(recall.join("rx.keys"), format!("openrouter = \"{key}\"\n")).unwrap();
    let env = EnvLookup::isolated(HashMap::from([
        ("CLAUDE_CONFIG_DIR".to_string(), dir.path().display().to_string()),
        ("OPENROUTER_API_KEY".to_string(), key),
    ]));
    let paths = Paths::in_dir(recall);
    let outcome = crate::claude_catalog::try_seed_user_catalog(
        &paths,
        "openrouter",
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

    let dsh = crate::install::spec(Harness::Dsh);
    assert_eq!(dsh.program, "dsh");
    assert_eq!(dsh.display, "DeepSeek Harness");
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
    let dsh = crate::install::ensure(Harness::Dsh, &EnvLookup::isolated(HashMap::new())).unwrap();
    assert_eq!(dsh, std::path::PathBuf::from("dsh"));
}
