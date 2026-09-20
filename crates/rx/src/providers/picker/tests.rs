use ratatui::{Terminal, backend::TestBackend};

use super::*;
use crate::provider::Setup;

fn state(id: &str, name: &str, configured: bool, default: bool) -> ProviderState {
    ProviderState {
        provider: Provider {
            id: id.to_string(),
            name: name.to_string(),
            endpoint: format!("https://{id}.test/v1"),
            anthropic_base: None,
            default_context: None,
            env: format!("{}_API_KEY", id.to_ascii_uppercase()),
            setup: Setup::Generated,
            default_model: None,
            claude_default_model: None,
        },
        orphaned: false,
        configured,
        stored_key: configured,
        environment_active: false,
        default,
    }
}

#[test]
fn provider_selection_matches_launch_credentials_and_overrides() {
    for id in ["tokener", "custom"] {
        for auth in [AuthMode::ApiKey, AuthMode::Env] {
            for stored in [false, true] {
                for environment in [false, true] {
                    let dir = tempfile::tempdir().unwrap();
                    let paths = Paths::in_dir(dir.path().to_path_buf());
                    let config = crate::config::RxConfig {
                        provider: std::collections::BTreeMap::from([(
                            id.to_string(),
                            crate::config::ProviderConfig {
                                base_url: Some("http://127.0.0.1:1/v1".to_string()),
                                env: Some("CUSTOM_TOKEN".to_string()),
                                auth,
                                ..Default::default()
                            },
                        )]),
                        ..Default::default()
                    };
                    std::fs::write(&paths.config, toml::to_string(&config).unwrap()).unwrap();
                    if stored {
                        std::fs::write(&paths.keys, format!("{id} = \"stored-key\"")).unwrap();
                    }
                    let mut values = std::collections::HashMap::new();
                    if environment {
                        values.insert("CUSTOM_TOKEN".to_string(), "env-key".to_string());
                    }
                    let env = EnvLookup::isolated(values);
                    let states = provider_states(&paths, &env).unwrap();
                    let state = states.iter().find(|state| state.provider.id == id).unwrap();
                    let expected = match auth {
                        AuthMode::ApiKey if stored => Some("stored-key"),
                        AuthMode::Env if environment => Some("env-key"),
                        AuthMode::ApiKey if environment && id == "tokener" => Some("env-key"),
                        _ => None,
                    };
                    assert_eq!(state.configured, expected.is_some());
                    assert_eq!(
                        state.environment_active,
                        environment && (auth == AuthMode::Env || id == "tokener")
                    );
                    assert_eq!(state.provider.endpoint, "http://127.0.0.1:1/v1");
                    assert_eq!(state.provider.env, "CUSTOM_TOKEN");
                    let selection = use_provider(&paths, &env, Some(id));
                    let launched = launch::configured_provider(Some(id), &paths, &env);
                    if let Some(key) = expected {
                        selection.unwrap();
                        let launch::ProviderResolution::Target(target) = launched.unwrap() else {
                            panic!("configured provider must launch");
                        };
                        assert_eq!(target.provider, state.provider);
                        assert_eq!(target.key, key);
                    } else {
                        assert!(selection.is_err());
                        assert!(launched.is_err());
                    }
                    if stored || expected.is_some() {
                        let app = App::new(Action::Logout, &states);
                        assert!(
                            app.filtered_providers()
                                .iter()
                                .any(|index| app.providers[*index].provider.id == id)
                        );
                        logout(&paths, &env, Some(id)).unwrap();
                        assert!(crate::config::stored_key(&paths, id).unwrap().is_none());
                    }
                }
            }
        }
    }
}

#[test]
fn picker_pins_openrouter_then_configured_then_alpha() {
    let mut states = vec![
        state("zenmux", "Zenmux", true, false),
        state("acme", "Acme", false, false),
        state("abacus", "Abacus", false, false),
        state("openrouter", "OpenRouter", false, false),
        state("deepseek", "DeepSeek", true, false),
    ];
    sort_provider_states(&mut states);
    let app = App::new(Action::Login, &states);
    let names = app
        .filtered_providers()
        .into_iter()
        .map(|index| app.providers[index].provider.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["OpenRouter", "DeepSeek", "Zenmux", "Abacus", "Acme"]);
}

#[test]
fn picker_searches_hidden_provider_id() {
    let providers = vec![
        state("openrouter", "OpenRouter", false, false),
        state("acme-edge", "Acme", false, false),
    ];
    let mut app = App::new(Action::Login, &providers);
    app.query = "edge".to_string();
    assert_eq!(app.filtered_providers(), vec![1]);
}

#[test]
fn logout_picker_only_contains_configured_providers() {
    let providers =
        vec![state("openrouter", "OpenRouter", true, true), state("acme", "Acme", false, false)];
    let app = App::new(Action::Logout, &providers);
    assert_eq!(app.filtered_providers(), vec![0]);
}

#[test]
fn use_picker_selects_default_without_authentication_step() {
    let providers = vec![
        state("openrouter", "OpenRouter", true, false),
        state("acme", "Acme", true, true),
        state("unused", "Unused", false, false),
    ];
    let mut app = App::new(Action::Use, &providers);
    assert_eq!(app.filtered_providers(), vec![0, 1]);
    assert_eq!(app.cursor, 1);

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(app.exit);
    assert_eq!(app.step, Step::Provider);
    assert!(matches!(app.outcome, Some(Outcome::Use { index: 1 })));
}

#[test]
fn picker_ignores_enter_without_a_matching_provider() {
    let providers = vec![state("openrouter", "OpenRouter", true, true)];
    let mut app = App::new(Action::Use, &providers);
    app.query = "missing".to_string();

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(!app.exit);
    assert_eq!(app.step, Step::Provider);
    assert!(app.selected.is_none());
    assert!(app.outcome.is_none());
}

#[test]
fn direct_login_starts_at_the_api_key_step() {
    let providers =
        vec![state("openrouter", "OpenRouter", false, false), state("acme", "Acme", false, false)];
    let mut app = App::login_for_provider(&providers, 1);

    assert_eq!(app.selected, Some(1));
    assert_eq!(app.step, Step::ApiKey);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(app.exit);
    assert_eq!(app.step, Step::ApiKey);
}

#[test]
fn clearing_inline_ui_removes_rendered_content_and_resets_cursor() {
    let backend = TestBackend::new(40, 3);
    let mut terminal =
        Terminal::with_options(backend, TerminalOptions { viewport: Viewport::Inline(3) }).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new("stale command suffix"), frame.area()))
        .unwrap();

    terminal.clear().unwrap();

    assert!(terminal.backend().buffer().content.iter().skip(1).all(|cell| cell.symbol() == " "));
    assert_eq!(terminal.get_cursor_position().unwrap(), Position::ORIGIN);
}

#[test]
fn api_key_is_masked_in_the_rendered_terminal() {
    let providers = vec![state("openrouter", "OpenRouter", false, false)];
    let mut app = App::new(Action::Login, &providers);
    app.step = Step::ApiKey;
    app.selected = Some(0);
    app.api_key = "sk-secret".to_string();
    let palette = Palette::current(&EnvLookup::isolated(Default::default()));
    let backend = TestBackend::new(80, 13);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &app, palette)).unwrap();
    let rendered =
        terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
    assert!(!rendered.contains("sk-secret"));
    assert!(rendered.contains("•••••••••"));
}

#[test]
fn list_uses_one_mutually_exclusive_status_marker() {
    let default = state("openrouter", "OpenRouter", true, true);
    let configured = state("acme", "Acme", true, false);
    let output = render_list(&[&default, &configured]);
    assert!(output.contains("* OpenRouter"));
    assert!(output.contains("• Acme"));
}

#[test]
fn list_keeps_a_stored_key_visible_after_its_provider_disappears() {
    let configured = state("openrouter", "OpenRouter", true, true);
    let unconfigured = state("acme", "Acme", false, false);
    let mut orphan = state("retired", "retired", true, false);
    orphan.provider = crate::provider::orphan("retired");
    orphan.orphaned = true;

    let output = render_list(&[&configured, &unconfigured, &orphan]);

    assert!(output.contains("retired"), "{output}");
    assert!(!output.contains("Acme"), "{output}");
    assert!(!orphan.selectable(Action::Use));
    assert!(!orphan.selectable(Action::Login));
    assert!(orphan.selectable(Action::Logout));
}
