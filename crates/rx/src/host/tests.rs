use super::*;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn request(harness: Option<&str>) -> String {
    serde_json::json!({
        "harness": harness,
        "gateway": {
            "provider_id": "tokener",
            "name": "Tokener",
            "endpoint": "https://api.tokener.dev/v1",
            "credential_env": "TOKENER_API_KEY"
        },
        "state_dir": std::env::temp_dir().join("tokener-agent"),
        "permission_policy": "standard",
        "install_policy": "prompt"
    })
    .to_string()
}

#[test]
fn capabilities_are_stable() {
    let value: serde_json::Value = serde_json::from_str(&capabilities_json().unwrap()).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "protocol": {"major": 1, "minor": 1},
            "harnesses": ["claude", "codex", "opencode", "pi", "dsh", "kimi"],
            "version": crate::RELEASE_VERSION,
        })
    );
}

#[test]
fn host_entrypoint_rejects_malformed_requests() {
    let env = EnvLookup::isolated(HashMap::from([(REQUEST_ENV.to_string(), "{".to_string())]));
    assert!(run(Vec::new(), &env).is_err());
}

#[test]
fn hosted_request_allows_missing_harness() {
    assert!(parse_request(&request(None)).unwrap().harness.is_none());
    assert_eq!(parse_request(&request(Some("codex"))).unwrap().harness.as_deref(), Some("codex"));
}

#[test]
fn request_rejects_unknown_fields_and_unsafe_profile_values() {
    let mut value: serde_json::Value = serde_json::from_str(&request(None)).unwrap();
    value["tokener_key"] = serde_json::json!("secret");
    assert!(parse_request(&value.to_string()).is_err());
    let mut value: serde_json::Value = serde_json::from_str(&request(None)).unwrap();
    value["gateway"]["credential_env"] = serde_json::json!("KEY;bad");
    assert!(parse_request(&value.to_string()).is_err());
}

#[test]
fn route_guards_are_harness_specific() {
    for (harness, allowed, rejected) in [
        (
            Harness::Claude,
            vec!["--resume", "session", "--tools", "Read"],
            vec!["--settings", "route.json"],
        ),
        (
            Harness::Codex,
            vec!["resume", "--last", "-c", "sandbox_mode=read-only"],
            vec!["-c", "model_provider=ollama"],
        ),
        (
            Harness::OpenCode,
            vec!["--model", "tokener/model-a", "--fork"],
            vec!["--model", "openai/model-a"],
        ),
        (
            Harness::Pi,
            vec!["--provider", "tokener", "--model", "model-a", "--resume"],
            vec!["--api-key", "secret"],
        ),
        (Harness::Dsh, vec![], vec!["--patch", "other.yml"]),
    ] {
        validate_route_args(harness, &os(&allowed), "tokener").unwrap();
        assert!(validate_route_args(harness, &os(&rejected), "tokener").is_err());
    }
    validate_route_args(Harness::Kimi, &os(&["--session", "session-id", "--plan"]), "tokener")
        .unwrap();
}

#[test]
fn codex_route_overrides_are_rejected_in_native_config_forms() {
    for value in [
        "model_provider=other",
        "openai_base_url='http://127.0.0.1:1'",
        "model_providers.acme.base_url='http://127.0.0.1:1'",
        "model_providers={acme={name='Other',wire_api='responses'}}",
        "model_providers={acme={experimental_bearer_token='synthetic-gateway-secret'}}",
    ] {
        for override_args in [
            os(&["-c", value]),
            os(&["--config", value]),
            os(&[&format!("-c={value}")]),
            os(&[&format!("--config={value}")]),
            os(&[&format!("-c{value}")]),
        ] {
            let error = validate_route_args(Harness::Codex, &override_args, "acme").unwrap_err();
            assert!(!error.to_string().contains("synthetic-gateway-secret"));
            let mut literal = os(&["--"]);
            literal.extend(override_args);
            validate_route_args(Harness::Codex, &literal, "acme").unwrap();
        }
    }
    validate_route_args(Harness::Codex, &os(&["-csandbox_mode='read-only'"]), "acme").unwrap();
}

#[test]
fn model_scopes_follow_the_requested_gateway() {
    for provider in ["acme", "tokener"] {
        for harness in [Harness::OpenCode, Harness::Pi] {
            for flag in ["-m", "--model"] {
                for (model, accepted) in
                    [(format!("{provider}/model-a"), true), ("other/model-a".to_string(), false)]
                {
                    assert_eq!(
                        validate_route_args(harness, &os(&[flag, &model]), provider).is_ok(),
                        accepted
                    );
                }
            }
        }
        for (argv, accepted) in [
            (vec!["--provider", provider, "--models", &format!("{provider}/*")], true),
            (vec!["--provider", "other"], false),
            (vec!["--models", &format!("{provider}/*,other/*")], false),
        ] {
            assert_eq!(validate_route_args(Harness::Pi, &os(&argv), provider).is_ok(), accepted);
        }
    }
}

#[test]
fn native_arguments_after_double_dash_are_literal() {
    validate_route_args(
        Harness::Claude,
        &os(&["--resume", "session", "--", "--settings", "literal"]),
        "tokener",
    )
    .unwrap();
}

#[test]
fn hosted_controls_are_scoped_and_leave_harness_homes_native() {
    assert_eq!(planning_overrides(), HashMap::from([("RX_NO_YOLO".to_string(), "1".to_string())]));
    for (policy, value) in [(InstallPolicy::Prompt, "0"), (InstallPolicy::Deny, "1")] {
        assert_eq!(
            install_overrides(policy),
            HashMap::from([("RX_NO_INSTALL".to_string(), value.to_string())])
        );
    }
}
