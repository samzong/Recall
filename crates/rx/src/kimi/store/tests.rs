use super::super::take_model;
use super::*;
use std::ffi::OsString;
use std::fs;

fn target(key: &str) -> ProviderTarget {
    let mut provider = crate::provider::find("tokener").unwrap().clone();
    provider.endpoint = "https://provider.test".to_string();
    ProviderTarget { provider, key: key.to_string(), model: None }
}

fn seed(path: &Path, alias: &str, key: &str, models: &[ListedModel]) -> Result<File> {
    seed_catalog(path, alias, &target(key), models, false)
}

fn read_config(path: &Path) -> toml::Value {
    toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn listed(id: &str, name: &str, context: i64) -> ListedModel {
    ListedModel { id: id.to_string(), name: Some(name.to_string()), context_length: Some(context) }
}

fn os(argv: &[&str]) -> Vec<OsString> {
    argv.iter().map(OsString::from).collect()
}

fn run_isolated(name: &str) -> bool {
    if std::env::var_os("RX_TEST_KIMI_CATALOG_CHILD").is_some() {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("RX_TEST_KIMI_CATALOG_CHILD", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[test]
fn model_flag_selects_catalog_model_and_preserves_literal_arguments() {
    assert_eq!(
        take_model(&os(&["--plan", "-m", "kimi-k3", "--", "--model", "literal"])).unwrap(),
        (Some("kimi-k3".to_string()), os(&["--plan", "--", "--model", "literal"]))
    );
    assert_eq!(
        take_model(&os(&["--model=glm-5", "-m=deepseek-v4"])).unwrap(),
        (Some("deepseek-v4".to_string()), Vec::new())
    );
}

#[test]
fn catalog_refresh_preserves_user_config_and_replaces_owned_models() {
    if run_isolated(
        "kimi::store::tests::catalog_refresh_preserves_user_config_and_replaces_owned_models",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        "default_model = \"native/model\"\n\n[providers.native]\n# keep this\ntype = \"openai\"\nbase_url = \"https://native.test/v1\"\napi_key = \"native-key\"\n\n[models.\"native/model\"]\nprovider = \"native\"\nmodel = \"native-model\"\nmax_context_size = 100000\n",
    )
    .unwrap();
    seed(
        &path,
        "rx-tokener",
        "rx-secret",
        &[listed("model-a", "Model A", 200_000), listed("model-b", "Model B", 300_000)],
    )
    .unwrap();
    seed(
        &path,
        "rx-tokener",
        "rx-secret",
        &[listed("model-b", "Model B", 300_000), listed("model-c", "Model C", 400_000)],
    )
    .unwrap();
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("# keep this"));
    let document: toml::Value = toml::from_str(&body).unwrap();
    assert_eq!(document["default_model"].as_str(), Some("native/model"));
    assert_eq!(document["providers"]["native"]["api_key"].as_str(), Some("native-key"));
    assert!(document["models"].get("native/model").is_some());
    assert!(document["models"].get("rx-tokener/model-a").is_none());
    assert!(document["models"].get("rx-tokener/model-b").is_some());
    assert!(document["models"].get("rx-tokener/model-c").is_some());
    let marker = fs::read_to_string(appended_path(&path, ".rx-catalog.json")).unwrap();
    assert!(!marker.contains("rx-secret"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(appended_path(&path, ".rx-catalog.json")).unwrap().permissions().mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn modified_owned_model_is_preserved_and_blocks_reclaim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let models = [listed("model-a", "Model A", 200_000)];
    seed(&path, "rx-tokener", "rx-secret", &models).unwrap();
    let mut document = fs::read_to_string(&path).unwrap().parse::<DocumentMut>().unwrap();
    document["models"]["rx-tokener/model-a"]["model"] = value("user-model");
    fs::write(&path, document.to_string()).unwrap();
    let error = seed(&path, "rx-tokener", "rx-secret", &models).unwrap_err();
    assert!(error.to_string().contains("outside rx ownership"), "{error:#}");
    let document = read_config(&path);
    assert_eq!(document["models"]["rx-tokener/model-a"]["model"].as_str(), Some("user-model"));
}

#[test]
fn overlapping_catalogs_keep_live_routes_and_reclaim_only_exited_launches() {
    if run_isolated(
        "kimi::store::tests::overlapping_catalogs_keep_live_routes_and_reclaim_only_exited_launches",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let first_models = [listed("model-a", "Model A", 200_000)];
    let second_models = [listed("model-b", "Model B", 300_000)];
    let first = seed(&path, "rx-first", "first-key", &first_models).unwrap();
    let same = seed(&path, "rx-first", "first-key", &first_models).unwrap();
    let second = seed(&path, "rx-first", "first-key", &second_models).unwrap();
    let third = seed(&path, "rx-second", "second-key", &first_models).unwrap();
    let document = read_config(&path);
    assert_eq!(document["models"]["rx-first/model-a"]["provider"].as_str(), Some("rx-first"));
    assert_eq!(document["models"]["rx-first/model-b"]["provider"].as_str(), Some("rx-first"));
    assert_eq!(document["models"]["rx-second/model-a"]["provider"].as_str(), Some("rx-second"));
    assert_eq!(document["providers"]["rx-first"]["api_key"].as_str(), Some("first-key"));
    assert_eq!(document["providers"]["rx-second"]["api_key"].as_str(), Some("second-key"));

    drop(first);
    seed(&path, "rx-second", "second-key", &first_models).unwrap();
    let document = read_config(&path);
    assert!(document["models"].get("rx-first/model-a").is_some());
    drop(same);
    let fourth = seed(&path, "rx-second", "second-key", &first_models).unwrap();
    let document = read_config(&path);
    assert!(document["models"].get("rx-first/model-a").is_none());
    assert!(document["models"].get("rx-first/model-b").is_some());
    assert!(document["providers"].get("rx-first").is_some());

    drop(second);
    drop(third);
    let _fifth = seed(&path, "rx-second", "second-key", &first_models).unwrap();
    let document = read_config(&path);
    assert!(document["models"].get("rx-first/model-b").is_none());
    assert!(document["providers"].get("rx-first").is_none());
    let marker = read_marker(&appended_path(&path, ".rx-catalog.json")).unwrap().unwrap();
    let CatalogMarker::Leased { catalogs, .. } = marker else { panic!("expected leased catalog") };
    assert_eq!(catalogs.len(), 1);
    assert_eq!(
        fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".rx-kimi-"))
            .count(),
        3
    );
    drop(fourth);
}

#[test]
fn historical_lease_reuse_preserves_files_without_claiming_config() {
    if run_isolated(
        "kimi::store::tests::historical_lease_reuse_preserves_files_without_claiming_config",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let models = [listed("model-a", "Model A", 200_000)];
    let desired = desired_catalog("rx-first", &target("first-key"), &models);
    seed(&path, "rx-first", "first-key", &models).unwrap();
    let lease_path = dir.path().join(catalog_lease_name(&desired).unwrap());
    assert_eq!(fs::metadata(&lease_path).unwrap().len(), 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&lease_path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    fs::write(&lease_path, b"user bytes").unwrap();
    for _ in 0..3 {
        seed(&path, "rx-second", "second-key", &models).unwrap();
        seed(&path, "rx-first", "first-key", &models).unwrap();
    }
    assert_eq!(fs::read(&lease_path).unwrap(), b"user bytes");
    assert_eq!(
        fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".rx-kimi-"))
            .count(),
        2
    );
    let owned = fs::read_to_string(&path).unwrap().parse::<DocumentMut>().unwrap();
    seed(&path, "rx-second", "second-key", &models).unwrap();
    let mut document = fs::read_to_string(&path).unwrap().parse::<DocumentMut>().unwrap();
    document["providers"]["rx-first"] = owned["providers"]["rx-first"].clone();
    fs::write(&path, document.to_string()).unwrap();
    let before = fs::read(&path).unwrap();
    let marker_path = appended_path(&path, ".rx-catalog.json");
    let marker = fs::read(&marker_path).unwrap();
    assert!(seed(&path, "rx-first", "first-key", &models).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&marker_path).unwrap(), marker);
}

#[test]
fn live_route_conflicts_and_user_edits_leave_catalog_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let models = [listed("model-a", "Model A", 200_000)];
    let _first = seed(&path, "rx-first", "first-key", &models).unwrap();
    let marker_path = appended_path(&path, ".rx-catalog.json");
    let marker = fs::read(&marker_path).unwrap();
    let before = fs::read(&path).unwrap();
    assert!(seed(&path, "rx-first", "rotated-key", &models).is_err());
    assert!(
        seed(&path, "rx-first", "first-key", &[listed("model-a", "Model A", 400_000)]).is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&marker_path).unwrap(), marker);
    let mut document = String::from_utf8(before).unwrap().parse::<DocumentMut>().unwrap();
    document["models"]["rx-first/model-a"]["model"] = value("user-model");
    fs::write(&path, document.to_string()).unwrap();
    let edited = fs::read(&path).unwrap();
    assert!(seed(&path, "rx-first", "first-key", &models).is_err());
    assert_eq!(fs::read(&path).unwrap(), edited);
    assert_eq!(fs::read(&marker_path).unwrap(), marker);
}

#[test]
fn legacy_and_unverifiable_leases_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let models = [listed("model-a", "Model A", 200_000)];
    let _lease = seed(&path, "rx-first", "first-key", &models).unwrap();
    let marker_path = appended_path(&path, ".rx-catalog.json");
    let before = fs::read(&path).unwrap();
    let current: serde_json::Value =
        serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
    let legacy = desired_catalog("rx-first", &target("first-key"), &models);
    let mut missing = current.clone();
    let name = missing["catalogs"].as_object().unwrap().keys().next().unwrap().clone();
    let catalog = missing["catalogs"].as_object_mut().unwrap().remove(&name).unwrap();
    missing["catalogs"][".rx-kimi-missing"] = catalog;
    let mut invalid = current.clone();
    invalid["version"] = 999.into();
    for marker in [serde_json::to_value(legacy).unwrap(), missing, invalid] {
        fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        let marker_bytes = fs::read(&marker_path).unwrap();
        assert!(seed(&path, "rx-first", "first-key", &models).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(&marker_path).unwrap(), marker_bytes);
    }
    let marker_bytes = serde_json::to_vec(&current).unwrap();
    fs::write(&marker_path, &marker_bytes).unwrap();
    drop(_lease);
    fs::remove_file(dir.path().join(name)).unwrap();
    assert!(seed(&path, "rx-first", "first-key", &models).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&marker_path).unwrap(), marker_bytes);
}
