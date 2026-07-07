use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ExtensionManifest {
    name: String,
    version: String,
    protocol: u32,
    min_recall: String,
    commands: Vec<String>,
}

#[derive(Debug)]
struct ExtensionEntry {
    name: String,
    path: PathBuf,
    manifest: Option<ExtensionManifest>,
    error: Option<String>,
}

pub(crate) fn run_list() -> Result<()> {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let extensions = discover_in_path(&path_env);

    if extensions.is_empty() {
        println!("No extensions found on PATH.");
        return Ok(());
    }

    let name_width = extensions
        .iter()
        .map(|extension| extension.name.len())
        .max()
        .unwrap_or(9)
        .max("Extension".len());
    let version_width = extensions
        .iter()
        .filter_map(|extension| extension.manifest.as_ref())
        .map(|manifest| manifest.version.len())
        .max()
        .unwrap_or(7)
        .max("Version".len());
    let protocol_width = extensions
        .iter()
        .filter_map(|extension| extension.manifest.as_ref())
        .map(|manifest| manifest.protocol.to_string().len())
        .max()
        .unwrap_or(8)
        .max("Protocol".len());
    let min_recall_width = extensions
        .iter()
        .filter_map(|extension| extension.manifest.as_ref())
        .map(|manifest| manifest.min_recall.len())
        .max()
        .unwrap_or(10)
        .max("Min Recall".len());

    println!(
        "{name:<name_width$}  {version:<version_width$}  {protocol:>protocol_width$}  {min_recall:<min_recall_width$}  Commands",
        name = "Extension",
        version = "Version",
        protocol = "Protocol",
        min_recall = "Min Recall"
    );
    for extension in extensions {
        match (extension.manifest, extension.error) {
            (Some(manifest), _) => {
                println!(
                    "{name:<name_width$}  {version:<version_width$}  {protocol:>protocol_width$}  {min_recall:<min_recall_width$}  {commands}",
                    name = extension.name,
                    version = manifest.version,
                    protocol = manifest.protocol,
                    min_recall = manifest.min_recall,
                    commands = manifest.commands.join(",")
                );
                println!("  {}", extension.path.display());
            }
            (_, Some(error)) => {
                println!(
                    "{name:<name_width$}  {version:<version_width$}  {protocol:>protocol_width$}  {min_recall:<min_recall_width$}  error: {error}",
                    name = extension.name,
                    version = "-",
                    protocol = "-",
                    min_recall = "-"
                );
                println!("  {}", extension.path.display());
            }
            _ => {}
        }
    }

    Ok(())
}

pub(crate) fn run_external(args: Vec<OsString>) -> Result<ExitStatus> {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    run_external_with_path(args, &path_env)
}

fn run_external_with_path(args: Vec<OsString>, path_env: &OsStr) -> Result<ExitStatus> {
    let mut args = args.into_iter();
    let name = args.next().ok_or_else(|| anyhow!("missing extension name"))?;
    let Some(name) = name.to_str() else {
        bail!("extension name must be valid UTF-8");
    };
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        bail!("invalid extension name: {name}");
    }
    let path = find_in_path(name, path_env).ok_or_else(|| {
        anyhow!("unknown recall command '{name}' and recall-{name} was not found on PATH")
    })?;
    Command::new(&path)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {}", path.display()))
}

fn discover_in_path(path_env: &OsStr) -> Vec<ExtensionEntry> {
    let mut extensions = BTreeMap::new();
    for dir in std::env::split_paths(path_env) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_executable_file(&path) {
                continue;
            }
            let Some(name) = extension_name(&path) else {
                continue;
            };
            extensions.entry(name).or_insert(path);
        }
    }

    extensions
        .into_iter()
        .map(|(name, path)| match read_manifest(&name, &path) {
            Ok(manifest) => ExtensionEntry { name, path, manifest: Some(manifest), error: None },
            Err(error) => {
                ExtensionEntry { name, path, manifest: None, error: Some(error.to_string()) }
            }
        })
        .collect()
}

fn find_in_path(name: &str, path_env: &OsStr) -> Option<PathBuf> {
    let extension = std::env::consts::EXE_EXTENSION;
    let binary = if extension.is_empty() {
        format!("recall-{name}")
    } else {
        format!("recall-{name}.{extension}")
    };
    std::env::split_paths(path_env)
        .map(|dir| dir.join(&binary))
        .find(|path| is_executable_file(path))
}

fn read_manifest(name: &str, path: &Path) -> Result<ExtensionManifest> {
    let output = Command::new(path)
        .arg("--recall-extension-manifest")
        .output()
        .with_context(|| format!("failed to read manifest from {}", path.display()))?;
    if !output.status.success() {
        bail!("manifest command exited with status {}", output.status);
    }
    let manifest: ExtensionManifest =
        serde_json::from_slice(&output.stdout).context("invalid extension manifest JSON")?;
    if manifest.name != name {
        bail!("manifest name '{}' does not match executable name '{}'", manifest.name, name);
    }
    Ok(manifest)
}

fn extension_name(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let name = file_name.strip_prefix("recall-")?;
    let extension = std::env::consts::EXE_EXTENSION;
    let name = if extension.is_empty() {
        name
    } else {
        name.strip_suffix(&format!(".{extension}")).unwrap_or(name)
    };
    if name.is_empty() { None } else { Some(name.to_string()) }
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = path.metadata() else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::{discover_in_path, run_external_with_path};
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    #[cfg(unix)]
    fn external_dispatch_runs_recall_binary_from_path() {
        let dir = temp_dir("dispatch");
        let output = dir.join("out.txt");
        let binary = dir.join("recall-demo");
        write_executable(
            &binary,
            r#"#!/bin/sh
printf "%s" "$1" > "$2"
"#,
        );

        let status = run_external_with_path(
            vec![
                OsString::from("demo"),
                OsString::from("hello"),
                output.as_os_str().to_os_string(),
            ],
            dir.as_os_str(),
        )
        .unwrap();

        assert!(status.success());
        assert_eq!(fs::read_to_string(output).unwrap(), "hello");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn external_dispatch_returns_child_exit_status() {
        let dir = temp_dir("exit-status");
        let binary = dir.join("recall-demo");
        write_executable(
            &binary,
            r#"#!/bin/sh
exit "$1"
"#,
        );

        let status = run_external_with_path(
            vec![OsString::from("demo"), OsString::from("7")],
            dir.as_os_str(),
        )
        .unwrap();

        assert_eq!(status.code(), Some(7));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn list_reads_extension_manifest() {
        let dir = temp_dir("manifest");
        let binary = dir.join("recall-demo");
        write_executable(
            &binary,
            r#"#!/bin/sh
cat <<'JSON'
{"name":"demo","version":"0.1.0","protocol":1,"min_recall":"0.2.10","commands":["demo"]}
JSON
"#,
        );

        let extensions = discover_in_path(dir.as_os_str());

        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].name, "demo");
        let manifest = extensions[0].manifest.as_ref().unwrap();
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.protocol, 1);
        assert_eq!(manifest.min_recall, "0.2.10");
        assert_eq!(manifest.commands, ["demo"]);
        fs::remove_dir_all(dir).unwrap();
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
}
