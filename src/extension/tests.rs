use super::{
    Catalog, CatalogExtension, CatalogTarget, CatalogVersion, ExtensionManifest,
    InstalledExtension, InstalledState, load_installed_state, managed_binary_path,
    run_external_with_root, save_installed_state, select_latest, unpack_archive, validate_manifest,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn unpack_zip_extension_preserves_binary_and_safe_permissions() {
    let root = tempfile::tempdir().unwrap();
    let archive = include_bytes!("../../tests/fixtures/extensions/privileged.zip");

    unpack_archive(archive, "https://example.invalid/probe.zip", root.path()).unwrap();

    let binary = super::find_unpacked_binary(root.path(), "recall-probe").unwrap();
    assert_eq!(fs::read(&binary).unwrap(), b"#!/bin/sh\nprintf 'zip-smoke\\n'\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(fs::metadata(binary).unwrap().permissions().mode() & 0o7777, 0o755);
    }
}

#[test]
#[cfg(unix)]
fn unpack_zip_extension_rejects_external_symlink_without_changing_target() {
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, "unchanged").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o640)).unwrap();
    let destination = root.path().join("package");
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive
        .add_symlink(
            "recall-probe",
            outside.to_str().unwrap(),
            zip::write::SimpleFileOptions::default().unix_permissions(0o777),
        )
        .unwrap();
    let archive = archive.finish().unwrap().into_inner();

    assert!(unpack_archive(&archive, "probe.zip", &destination).is_err());
    assert!(fs::symlink_metadata(destination.join("recall-probe")).is_err());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "unchanged");
    assert_eq!(fs::metadata(outside).unwrap().permissions().mode() & 0o7777, 0o640);
}

#[test]
#[cfg(unix)]
fn external_dispatch_runs_managed_binary() {
    let root = temp_dir("dispatch");
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let output = root.join("out.txt");
    write_executable(
        &managed_binary_path(&root, "demo"),
        r#"#!/bin/sh
printf "%s" "$1" > "$2"
"#,
    );

    let status = run_external_with_root(
        vec![OsString::from("demo"), OsString::from("hello"), output.as_os_str().to_os_string()],
        &root,
    )
    .unwrap();

    assert!(status.success());
    assert_eq!(fs::read_to_string(output).unwrap(), "hello");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn select_latest_compatible_release_for_current_target() {
    let target = super::target_triple().unwrap();
    let mut versions = BTreeMap::new();
    versions.insert("0.1.0".to_string(), catalog_version(&target, "0.2.10", 1, "old"));
    versions.insert("0.2.0".to_string(), catalog_version(&target, "999.0.0", 1, "too-new-recall"));
    versions.insert(
        "0.3.0".to_string(),
        catalog_version(&target, "0.2.10", crate::PROTOCOL_VERSION + 1, "too-new-protocol"),
    );
    versions.insert("0.1.1".to_string(), catalog_version(&target, "0.2.10", 1, "newer-compatible"));
    let mut future_transport = catalog_version(&target, "0.2.10", 1, "too-new-transport");
    future_transport.transport_version = Some(super::transport::VERSION + 1);
    versions.insert("0.4.0".to_string(), future_transport);
    versions.get_mut("0.1.1").unwrap().transport_version = Some(super::transport::VERSION);
    let mut extensions = BTreeMap::new();
    extensions
        .insert("probe".to_string(), CatalogExtension { description: String::new(), versions });
    let catalog = Catalog { schema: 1, extensions };

    let selected = select_latest("probe", &catalog).unwrap();

    assert_eq!(selected.version, "0.1.1");
    assert_eq!(selected.transport_version, Some(super::transport::VERSION));
    assert!(selected.target.url.ends_with("newer-compatible.tar.gz"));
}

fn catalog_version(target: &str, min_recall: &str, protocol: u32, slug: &str) -> CatalogVersion {
    let mut targets = BTreeMap::new();
    targets.insert(
        target.to_string(),
        CatalogTarget {
            url: format!("https://example.invalid/{slug}.tar.gz"),
            sha256: "00".repeat(32),
        },
    );
    CatalogVersion {
        protocol,
        min_recall: min_recall.to_string(),
        transport_version: None,
        targets,
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("recall-extension-{label}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
#[cfg(unix)]
fn remote_provider_requires_matching_identity_and_compatible_transport() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("bin")).unwrap();
    let binary = managed_binary_path(root.path(), "opendal");
    let base = serde_json::json!({"name":"opendal","version":"0.1.0","protocol":2,
        "min_recall":"0.6.4","transport_version":1});
    let write = |manifest: &serde_json::Value| {
        write_executable(&binary, &format!("#!/bin/sh\nprintf '%s' '{}'\n", manifest));
    };
    fs::write(
        root.path().join("installed.json"),
        r#"{"schema":1,"extensions":{"opendal":{"version":"0.1.0"}}}"#,
    )
    .unwrap();
    write(&base);
    assert_eq!(super::remote_binary_with_root("opendal", root.path()).unwrap(), binary);
    for (field, value) in [
        ("name", serde_json::json!("r2")),
        ("protocol", serde_json::json!(crate::PROTOCOL_VERSION + 1)),
        ("min_recall", serde_json::json!("999.0.0")),
        ("transport_version", serde_json::json!(2)),
        ("transport_version", serde_json::Value::Null),
    ] {
        let mut manifest = base.clone();
        manifest[field] = value;
        write(&manifest);
        assert!(super::remote_binary_with_root("opendal", root.path()).is_err(), "{manifest}");
    }
    assert!(super::remote_binary_with_root("missing", root.path()).is_err());
    assert!(super::remote_binary_with_root("../opendal", root.path()).is_err());
}

#[test]
#[cfg(unix)]
fn legacy_r2_manifest_requires_upgrade_before_remote_use() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("bin")).unwrap();
    let binary = managed_binary_path(root.path(), "r2");
    write_executable(
        &binary,
        r#"#!/bin/sh
printf '%s' '{"name":"r2","version":"0.1.0","protocol":2,"min_recall":"0.6.3"}'
"#,
    );
    fs::write(
        root.path().join("installed.json"),
        r#"{"schema":1,"extensions":{"r2":{"version":"0.1.0"}}}"#,
    )
    .unwrap();
    let error = super::remote_binary_with_root("r2", root.path()).unwrap_err().to_string();
    assert!(error.contains("recall ext upgrade r2"), "{error}");
    write_executable(
        &binary,
        r#"#!/bin/sh
printf '%s' '{"name":"r2","version":"0.1.2","protocol":2,"min_recall":"0.6.5","transport_version":1}'
"#,
    );
    fs::write(
        root.path().join("installed.json"),
        r#"{"schema":1,"extensions":{"r2":{"version":"0.1.2"}}}"#,
    )
    .unwrap();
    assert_eq!(super::remote_binary_with_root("r2", root.path()).unwrap(), binary);
}

#[test]
#[cfg(unix)]
fn remote_provider_requires_installed_catalog_record() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("bin")).unwrap();
    let binary = managed_binary_path(root.path(), "opendal");
    write_executable(
        &binary,
        r#"#!/bin/sh
printf '%s' '{"name":"opendal","version":"0.1.0","protocol":2,"min_recall":"0.6.4","transport_version":1}'
"#,
    );
    assert!(super::remote_binary_with_root("opendal", root.path()).is_err());
    fs::write(
        root.path().join("installed.json"),
        r#"{"schema":1,"extensions":{"opendal":{"version":"0.2.0"}}}"#,
    )
    .unwrap();
    assert!(super::remote_binary_with_root("opendal", root.path()).is_err());
    fs::write(
        root.path().join("installed.json"),
        r#"{"schema":1,"extensions":{"opendal":{"version":"0.1.0"}}}"#,
    )
    .unwrap();
    assert_eq!(super::remote_binary_with_root("opendal", root.path()).unwrap(), binary);
}

#[test]
fn installed_state_ignores_removed_metadata_fields() {
    let root = temp_dir("state-old-fields");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("installed.json"),
        r#"{
  "schema": 1,
  "extensions": {
"probe": {
  "version": "0.1.0",
  "target": "x86_64-unknown-linux-gnu",
  "protocol": 1,
  "min_recall": "0.2.10",
  "commands": ["probe"],
  "binary": "recall-probe",
  "description": "Probe extension"
}
  }
}
"#,
    )
    .unwrap();

    let loaded = load_installed_state(&root).unwrap();

    assert_eq!(loaded.extensions["probe"].version, "0.1.0");
    assert_eq!(loaded.extensions["probe"].description, "Probe extension");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn catalog_description_is_not_part_of_manifest_validation() {
    let manifest: ExtensionManifest = serde_json::from_str(
        r#"{"name":"probe","version":"0.1.0","protocol":1,"min_recall":"0.2.10"}"#,
    )
    .unwrap();

    validate_manifest("probe", "0.1.0", 1, "0.2.10", None, &manifest).unwrap();
    assert!(validate_manifest("probe", "0.1.0", 1, "0.2.10", Some(1), &manifest).is_err());
    let manifest = ExtensionManifest { transport_version: Some(1), ..manifest };
    validate_manifest("probe", "0.1.0", 1, "0.2.10", None, &manifest).unwrap();
    validate_manifest("probe", "0.1.0", 1, "0.2.10", Some(1), &manifest).unwrap();
    assert!(validate_manifest("probe", "0.1.0", 1, "0.2.10", Some(2), &manifest).is_err());
}

#[test]
fn installed_state_round_trips() {
    let root = temp_dir("state");
    let mut state = InstalledState { schema: 1, extensions: BTreeMap::new() };
    state.extensions.insert(
        "probe".to_string(),
        InstalledExtension {
            version: "0.1.0".to_string(),
            description: "Probe extension".to_string(),
        },
    );

    save_installed_state(&root, &state).unwrap();
    let loaded = load_installed_state(&root).unwrap();

    assert_eq!(loaded.extensions["probe"].version, "0.1.0");
    assert_eq!(loaded.extensions["probe"].description, "Probe extension");
    fs::remove_dir_all(root).unwrap();
}
