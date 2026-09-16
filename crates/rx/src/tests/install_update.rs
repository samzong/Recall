use super::*;

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
    let manifest: toml::Value = toml::from_str(include_str!("../../../../Cargo.toml")).unwrap();
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
            crate::update::self_update_blocker(Path::new(path)),
            Some(crate::update::HOMEBREW_UPDATE_HINT)
        );
    }
    assert_eq!(crate::update::self_update_blocker(Path::new("/Users/x/.cargo/bin/rx")), None);
    assert_eq!(
        crate::update::self_update_blocker(Path::new("/opt/homebrew/Cellar/other/0.5.1/bin/rx")),
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
        crate::update::self_update_blocker(&linked_rx),
        Some(crate::update::HOMEBREW_UPDATE_HINT)
    );
    assert_eq!(
        crate::update::homebrew_launch_update_notice(&linked_rx, "0.6.0"),
        Some("rx 0.6.0 is available — run `brew upgrade recall`".to_string())
    );
}

#[test]
fn release_asset_name_matches_host() {
    let name = crate::update::release_asset_name().unwrap();
    assert!(name.starts_with("recall-"));
}

#[test]
fn install_specs_use_official_urls() {
    let claude = crate::install::spec(Harness::Claude).unwrap();
    assert_eq!(claude.url, "https://claude.ai/install.sh");
    assert_eq!(claude.shell, "bash");
    assert_eq!(
        crate::install::command_line(&claude),
        "curl -fsSL https://claude.ai/install.sh | bash"
    );

    let codex = crate::install::spec(Harness::Codex).unwrap();
    assert_eq!(codex.url, "https://chatgpt.com/codex/install.sh");
    assert_eq!(codex.shell, "sh");

    let opencode = crate::install::spec(Harness::OpenCode).unwrap();
    assert_eq!(opencode.url, "https://opencode.ai/install");
    assert_eq!(opencode.shell, "bash");

    let pi = crate::install::spec(Harness::Pi).unwrap();
    assert_eq!(pi.url, "https://pi.dev/install.sh");
    assert_eq!(pi.shell, "sh");

    let kimi = crate::install::spec(Harness::Kimi).unwrap();
    assert_eq!(kimi.program, "kimi");
    assert_eq!(kimi.display, "Kimi Code");
    assert_eq!(kimi.url, "https://code.kimi.com/kimi-code/install.sh");
    assert_eq!(kimi.shell, "bash");
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
    let path = crate::install::ensure(Harness::Pi, &isolated(&[])).unwrap();
    assert_eq!(path, std::path::PathBuf::from("pi"));
    let dsh = crate::install::ensure(Harness::Dsh, &isolated(&[])).unwrap();
    assert_eq!(dsh, std::path::PathBuf::from("dsh"));
    let kimi = crate::install::ensure(Harness::Kimi, &isolated(&[])).unwrap();
    assert_eq!(kimi, std::path::PathBuf::from("kimi"));
}
