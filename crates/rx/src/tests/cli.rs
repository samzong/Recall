use super::*;

#[test]
fn launch_syntax_preserves_harness_provider_and_literal_arguments() {
    use Harness::*;
    type LaunchCase<'a> = (&'a [&'a str], Harness, Option<&'a str>, &'a [&'a str]);
    let cases: &[LaunchCase<'_>] = &[
        (&["/usr/local/bin/rxc", "fix login"], Claude, None, &["fix login"]),
        (&["rxx", "exec", "cargo test"], Codex, None, &["exec", "cargo test"]),
        (&["rxc", "--provider", "tokener"], Claude, Some("tokener"), &[]),
        (
            &["rx", "--provider", "openrouter", "claude", "--resume", "abc"],
            Claude,
            Some("openrouter"),
            &["--resume", "abc"],
        ),
        (
            &["rx", "claude", "--provider=openrouter", "--resume", "abc"],
            Claude,
            Some("openrouter"),
            &["--resume", "abc"],
        ),
        (
            &["rx", "claude", "--", "--provider", "openrouter"],
            Claude,
            None,
            &["--", "--provider", "openrouter"],
        ),
        (&["rx", "claude", "--help"], Claude, None, &["--help"]),
        (&["rxo", "run", "hello"], OpenCode, None, &["run", "hello"]),
        (&["rxp", "--print", "hi"], Pi, None, &["--print", "hi"]),
        (&["rxd", "--resume"], Dsh, None, &["--resume"]),
        (&["rx", "dsh", "--resume"], Dsh, None, &["--resume"]),
        (&["rxk", "--continue"], Kimi, None, &["--continue"]),
        (&["rx", "kimi", "web", "--port", "58627"], Kimi, None, &["web", "--port", "58627"]),
    ];
    for (argv, harness, provider, passthrough) in cases {
        assert_eq!(
            parse_line(argv),
            Command::Launch(request(*harness, *provider, passthrough)),
            "{argv:?}"
        );
    }
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
    assert!(crate::help_text().contains("rx --provider <provider> <harness>"));
    assert!(crate::help_text().contains("rx --provider none <harness>"));
    assert!(crate::help_text().contains("rx providers <list|login|logout|use>"));
    assert!(crate::help_text().contains("rx providers models update [provider]"));
    assert!(crate::help_text().contains("rx completions <bash|zsh|fish>"));
    assert!(!crate::help_text().contains("rx providers list\n"));
    assert!(!crate::help_text().contains("rx completions --providers"));
}

#[test]
fn completions_parses_shells_and_id_lists() {
    use CompletionsCommand::*;
    for (suffix, expected) in [
        (None, Help),
        (Some("zsh"), Generate { shell: CompletionShell::Zsh }),
        (Some("bash"), Generate { shell: CompletionShell::Bash }),
        (Some("fish"), Generate { shell: CompletionShell::Fish }),
        (Some("--providers"), ListProviders(ProviderIdFilter::All)),
        (Some("--configured"), ListProviders(ProviderIdFilter::Configured)),
        (Some("--targets"), ListProviders(ProviderIdFilter::Targets)),
    ] {
        let mut argv = vec!["rx", "completions"];
        argv.extend(suffix);
        assert_eq!(parse_line(&argv), Command::Completions(expected), "{argv:?}");
    }
}

#[test]
fn invalid_commands_report_the_rejected_argument() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["rx", "--provider", "openrouter", "completions", "zsh"],
            "--provider is not valid with rx completions",
        ),
        (&["rx", "completions", "zsh", "extra"], "unexpected argument: extra"),
        (&["rx", "completions", "powershell"], "unknown completions command: powershell"),
        (&["rx", "gemini"], "unknown harness: gemini"),
        (&["rx", "claude", "--provider"], "--provider requires a value"),
        (
            &["rx", "providers", "use", "openrouter", "tokener"],
            "usage: rx providers use [provider]",
        ),
        (&["rx", "providers", "models", "list"], "unknown providers models command: list"),
        (
            &["rx", "providers", "models", "update", "openrouter", "tokener"],
            "usage: rx providers models update [provider]",
        ),
    ];
    for (argv, expected) in cases {
        let error = parse(&os(argv)).unwrap_err();
        assert!(error.to_string().contains(expected), "{argv:?}: {error}");
    }
}

#[cfg(unix)]
#[test]
fn bash_completions_follow_argument_context() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("rx");
    fs::write(
        &bin,
        "#!/bin/sh\ncase \"$*\" in\n'completions --targets') printf 'openrouter\\nnone\\n';;\n'completions --providers') echo all-provider;;\n'completions --configured') echo configured-provider;;\n*) exit 1;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    let script = format!(
        "{}\nCOMP_WORDS=(\"$@\"); COMP_CWORD=$(($# - 1)); _rx; if ((${{#COMPREPLY[@]}})); then printf '%s\\n' \"${{COMPREPLY[@]}}\"; fi",
        crate::completions::script(CompletionShell::Bash)
    );
    let cases: &[(&[&str], &[&str])] = &[
        (&["rx", "co"], &["codex", "completions"]),
        (&["rx", "codex", "--prov"], &["--provider"]),
        (&["rx", "--provider", "open"], &["openrouter"]),
        (&["rx", "--provider=open"], &["--provider=openrouter"]),
        (&["rx", "--provider", "=", "open"], &["openrouter"]),
        (&["rx", "--provider", "="], &["openrouter", "none"]),
        (&["rx", "--provider", "=", "none", "co"], &["codex"]),
        (&["rx", "--provider=none", "co"], &["codex"]),
        (&["rx", "codex", "--", "--prov"], &[]),
        (&["rx", "codex", "--", "--provider", "open"], &[]),
        (&["rx", "codex", "--", "--provider=open"], &[]),
        (&["rx", "--provider", "none", "codex", "--", "--prov"], &[]),
        (&["rx", "providers", "lo"], &["login", "logout"]),
        (&["rx", "providers", "login", "all"], &["all-provider"]),
        (&["rx", "providers", "logout", "conf"], &["configured-provider"]),
        (&["rx", "providers", "use", "open"], &["openrouter"]),
        (&["rx", "providers", "models", "update", "conf"], &["configured-provider"]),
        (&["rx", "completions", "z"], &["zsh"]),
        (&["rx", "update", "--y"], &["--yes"]),
        (&["rx", "codex", "--provider", "none", "--prov"], &[]),
    ];
    let check = |words: &[&str], expected: &[&str]| {
        let output = std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c", &script, "completion-test"])
            .args(words)
            .env_clear()
            .env("HOME", dir.path())
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
            .output()
            .unwrap();
        assert!(output.status.success(), "{words:?}: {}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.lines().collect::<Vec<_>>(), expected, "{words:?}");
    };
    for (words, expected) in cases {
        check(words, expected);
    }
    for alias in ["rxc", "rxx", "rxo", "rxp", "rxd", "rxk"] {
        check(&[alias, "--prov"], &["--provider"]);
        check(&[alias, "--", "--prov"], &[]);
        check(&[alias, "--provider", "=", "open"], &["openrouter"]);
    }
}

#[test]
fn argv0_harness_resolves_aliases() {
    assert_eq!(crate::args::argv0_harness("/usr/local/bin/rxc"), Some("claude"));
    assert_eq!(crate::args::argv0_harness("rxx"), Some("codex"));
    assert_eq!(crate::args::argv0_harness("rxo.exe"), Some("opencode"));
    assert_eq!(crate::args::argv0_harness("rxp"), Some("pi"));
    assert_eq!(crate::args::argv0_harness("rxd"), Some("dsh"));
    assert_eq!(crate::args::argv0_harness("rxk"), Some("kimi"));
    assert_eq!(crate::args::argv0_harness("/home/u/.cargo/bin/rx"), None);
}

#[test]
fn providers_commands_parse() {
    use ProvidersCommand::*;
    assert_eq!(parse_line(&["rx", "providers"]), Command::Providers(Help));
    assert_eq!(parse_line(&["rx", "providers", "list"]), Command::Providers(List));
    assert_eq!(parse_line(&["rx", "providers", "models"]), Command::Providers(ModelsHelp));
    for provider in [None, Some("tokener-dev")] {
        let selected = provider.map(str::to_string);
        for (command, expected) in [
            ("login", Login { provider: selected.clone() }),
            ("logout", Logout { provider: selected.clone() }),
            ("use", Use { provider: selected.clone() }),
        ] {
            let mut argv = vec!["rx", "providers", command];
            argv.extend(provider);
            assert_eq!(parse_line(&argv), Command::Providers(expected), "{argv:?}");
        }
    }
    for provider in [None, Some("openrouter")] {
        let mut argv = vec!["rx", "providers", "models", "update"];
        argv.extend(provider);
        assert_eq!(
            parse_line(&argv),
            Command::Providers(ModelsUpdate { provider: provider.map(str::to_string) }),
            "{argv:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_survive_passthrough_but_cannot_select_a_provider() {
    use std::os::unix::ffi::OsStringExt;
    let value = OsString::from_vec(vec![b'x', 0xff]);
    for prefix in [&["rx", "claude"][..], &["rx", "claude", "--", "--provider"][..]] {
        let mut argv = os(prefix);
        argv.push(value.clone());
        let Command::Launch(request) = parse(&argv).unwrap() else { panic!("expected launch") };
        assert_eq!(request.passthrough, argv[2..]);
        assert_eq!(request.provider, None);
    }
    let mut argv = os(&["rx", "claude", "--provider"]);
    argv.push(value);
    assert!(parse(&argv).is_err());
}
