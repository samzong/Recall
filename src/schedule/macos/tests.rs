use super::*;

#[test]
fn job_environment_only_preserves_variables_allowed_for_each_job() {
    let home = Path::new("/Users/example");
    for (credentials, config) in [
        (None, None),
        (Some("~/.aws/custom"), None),
        (None, Some("/profiles/config")),
        (Some("~/.aws/custom"), Some("/profiles/config")),
    ] {
        let variables = BTreeMap::from([
            ("AWS_SHARED_CREDENTIALS_FILE", credentials),
            ("AWS_CONFIG_FILE", config),
            ("PATH", Some("/custom/bin")),
            ("AWS_ACCESS_KEY_ID", Some("ignored")),
            ("AWS_SECRET_ACCESS_KEY", Some("ignored")),
            ("AWS_PROFILE", Some("recall-storage")),
            ("AWS_DEFAULT_PROFILE", Some("fallback-storage")),
            ("RECALL_OPENDAL_CONFIG", Some("ignored")),
        ]);
        let variable = |name: &str| variables.get(name).copied().flatten().map(OsString::from);
        let local = job_environment_with(Job::LocalSync, home, variable).unwrap();
        assert_eq!(local, BTreeMap::from([("HOME", "/Users/example".to_string())]));
        let remote = job_environment_with(Job::RemoteSync, home, variable).unwrap();
        let mut expected = BTreeMap::from([
            ("HOME", "/Users/example".to_string()),
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin".to_string()),
            ("AWS_PROFILE", "recall-storage".to_string()),
            ("AWS_DEFAULT_PROFILE", "fallback-storage".to_string()),
        ]);
        if credentials.is_some() {
            expected
                .insert("AWS_SHARED_CREDENTIALS_FILE", "/Users/example/.aws/custom".to_string());
        }
        if config.is_some() {
            expected.insert("AWS_CONFIG_FILE", "/profiles/config".to_string());
        }
        assert_eq!(remote, expected);
    }
}

#[test]
fn aws_file_overrides_keep_the_same_location_without_a_shell() {
    let home = Path::new("/Users/example");
    assert_eq!(
        aws_file_path(Path::new("~/.aws/custom"), home).unwrap(),
        "/Users/example/.aws/custom"
    );
    assert_eq!(aws_file_path(Path::new("/profiles/r2"), home).unwrap(), "/profiles/r2");
    assert_eq!(aws_file_path(Path::new(""), home).unwrap(), "");
    assert_eq!(
        aws_file_path(Path::new("profiles/r2"), home).unwrap(),
        std::env::current_dir().unwrap().join("profiles/r2").to_str().unwrap()
    );
}

#[test]
fn launchctl_exit_status_preserves_codes_and_pending_state() {
    for value in ["0", "7", "(never exited)"] {
        let job =
            format!("gui/501/recall = {{\n\tstate = not running\n\tlast exit code = {value}\n}}");
        assert_eq!(last_exit_code(&job), Some(value));
    }
    assert_eq!(last_exit_code("state = running\nlast terminating signal = 9"), None);
}

#[test]
fn plist_roundtrips_paths_and_global_sync_arguments() {
    let binary = Path::new("/Applications/Recall & <Tools>/recall");
    let stdout = Path::new("/Users/example/Library/Logs/recall/out\r.log");
    let stderr = Path::new("/Users/example/Library/Logs/recall/err.log");
    let environment =
        job_environment_with(Job::LocalSync, Path::new("/Users/example"), |_| None).unwrap();
    let plist = render_plist(Job::LocalSync, binary, 1800, stdout, stderr, &environment).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), plist).unwrap();
    let output = checked(
        Command::new("/usr/bin/plutil").args(["-convert", "json", "-o", "-"]).arg(file.path()),
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["Label"], "io.github.samzong.recall.sync");
    assert_eq!(
        value["ProgramArguments"],
        serde_json::json!([binary.to_str().unwrap(), "sync", "--project", "all"])
    );
    assert_eq!(value["StartInterval"], 1800);
    assert_eq!(value["KeepAlive"], false);
    assert_eq!(value["AbandonProcessGroup"], true);
    assert_eq!(value["Umask"], 0o77);
    let definition: Definition = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(definition.standard_out_path, stdout);
    assert_eq!(definition.standard_error_path, stderr);
    assert_eq!(value["EnvironmentVariables"], serde_json::json!({"HOME": "/Users/example"}));
    assert!(
        render_plist(Job::LocalSync, Path::new("recall"), 1800, stdout, stderr, &environment)
            .is_err()
    );
    assert!(
        render_plist(
            Job::LocalSync,
            Path::new("/bad\u{1}/recall"),
            1800,
            stdout,
            stderr,
            &environment
        )
        .is_err()
    );
}

#[test]
fn schedules_keep_separate_identity_paths_arguments_and_environment() {
    let home = Path::new("/Users/example");
    let local = Agent::for_home(Job::LocalSync, home, "gui/501".to_string());
    let remote = Agent::for_home(Job::RemoteSync, home, "gui/501".to_string());
    assert_ne!(local.target(), remote.target());
    assert_ne!(local.plist, remote.plist);
    assert_ne!(local.stdout, remote.stdout);
    assert_ne!(local.stderr, remote.stderr);
    assert_eq!(local.plist, home.join("Library/LaunchAgents/io.github.samzong.recall.sync.plist"));
    assert_eq!(local.stdout, home.join("Library/Logs/recall/sync.stdout.log"));
    assert_eq!(local.stderr, home.join("Library/Logs/recall/sync.stderr.log"));
    let environment = job_environment_with(Job::RemoteSync, home, |name| match name {
        "AWS_SHARED_CREDENTIALS_FILE" => Some("/profiles/R2 & <test>\r.ini".into()),
        _ => None,
    })
    .unwrap();
    let plist = render_plist(
        Job::RemoteSync,
        Path::new("/opt/homebrew/bin/recall"),
        120,
        &remote.stdout,
        &remote.stderr,
        &environment,
    )
    .unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), plist).unwrap();
    let output = checked(
        Command::new("/usr/bin/plutil").args(["-convert", "json", "-o", "-"]).arg(file.path()),
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["Label"], Job::RemoteSync.label());
    assert_eq!(
        value["ProgramArguments"],
        serde_json::json!(["/opt/homebrew/bin/recall", "remote", "sync"])
    );
    assert_eq!(value["StartInterval"], 120);
    assert_eq!(value["EnvironmentVariables"], serde_json::to_value(environment).unwrap());
    assert_eq!(value["StandardOutPath"], remote.stdout.to_str().unwrap());
    assert_eq!(value["StandardErrorPath"], remote.stderr.to_str().unwrap());
}
