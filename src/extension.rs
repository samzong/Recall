use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) mod transport;

pub(crate) fn remote_binary(provider: &str) -> Result<PathBuf> {
    remote_binary_with_root(provider, &extension_root()?)
}

fn remote_binary_with_root(provider: &str, root: &Path) -> Result<PathBuf> {
    validate_extension_name(provider)?;
    let installed = load_installed_state(root)?;
    let recorded = installed.extensions.get(provider).with_context(|| {
        format!("remote provider '{provider}' is not installed from the official Recall extension catalog. Run recall ext install {provider}")
    })?;
    let path = managed_binary_path(root, provider);
    if !is_executable_file(&path) {
        bail!("remote provider is not installed; run recall ext install {provider}");
    }
    let manifest = read_manifest(&path)?;
    if manifest.name != provider {
        bail!("provider manifest name '{}' does not match '{provider}'", manifest.name);
    }
    if manifest.version != recorded.version {
        bail!(
            "remote provider '{provider}' manifest version '{}' does not match installed version '{}'. Run recall ext remove {provider}, then recall ext install {provider}",
            manifest.version,
            recorded.version
        );
    }
    validate_compatibility(&manifest)?;
    match manifest.transport_version {
        Some(transport::VERSION) => {}
        None if provider == "r2" => {}
        None => bail!("extension '{provider}' does not declare a remote transport"),
        Some(version) => bail!("unsupported remote transport version {version} for '{provider}'"),
    }
    Ok(path)
}

const CATALOG_URL: &str = "https://samzong.github.io/Recall/extensions/catalog.json";
const EMPTY_INSTALLED_HELP: &str = "\nExtensions:\n  none\n";

#[derive(Debug, Deserialize, Serialize)]
struct ExtensionManifest {
    name: String,
    version: String,
    protocol: u32,
    min_recall: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_version: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Catalog {
    schema: u32,
    extensions: BTreeMap<String, CatalogExtension>,
}

#[derive(Debug, Deserialize)]
struct CatalogExtension {
    #[serde(default)]
    description: String,
    versions: BTreeMap<String, CatalogVersion>,
}

#[derive(Debug, Deserialize)]
struct CatalogVersion {
    protocol: u32,
    min_recall: String,
    targets: BTreeMap<String, CatalogTarget>,
}

#[derive(Clone, Debug, Deserialize)]
struct CatalogTarget {
    url: String,
    sha256: String,
}

#[derive(Debug)]
struct SelectedExtension {
    version: String,
    description: String,
    protocol: u32,
    min_recall: String,
    target: CatalogTarget,
}

#[derive(Debug, Deserialize, Serialize)]
struct InstalledState {
    schema: u32,
    extensions: BTreeMap<String, InstalledExtension>,
}

#[derive(Debug, Deserialize, Serialize)]
struct InstalledExtension {
    version: String,
    #[serde(default)]
    description: String,
}

pub(crate) fn run_list(available: bool) -> Result<()> {
    let root = extension_root()?;
    if available { list_available(&root) } else { list_installed(&root) }
}

pub(crate) fn run_install(name: &str) -> Result<()> {
    validate_extension_name(name)?;
    let root = extension_root()?;
    let catalog = fetch_catalog()?;
    let selected = select_latest(name, &catalog)?;
    install_selected(&root, name, &selected)
}

pub(crate) fn run_remove(name: &str) -> Result<()> {
    validate_extension_name(name)?;
    let root = extension_root()?;
    let mut state = load_installed_state(&root)?;
    if state.extensions.remove(name).is_none() {
        bail!("extension is not installed: {name}");
    }

    remove_if_exists(&managed_binary_path(&root, name))?;
    remove_dir_if_exists(&root.join("packages").join(name))?;
    save_installed_state(&root, &state)?;
    println!("Removed {name}");
    Ok(())
}

pub(crate) fn run_upgrade(name: Option<String>) -> Result<()> {
    let root = extension_root()?;
    let state = load_installed_state(&root)?;
    if state.extensions.is_empty() {
        println!("No installed extensions.");
        return Ok(());
    }

    let names = if let Some(name) = name {
        validate_extension_name(&name)?;
        if !state.extensions.contains_key(&name) {
            bail!("extension is not installed: {name}");
        }
        vec![name]
    } else {
        state.extensions.keys().cloned().collect()
    };

    let catalog = fetch_catalog()?;
    for name in names {
        let current = state.extensions.get(&name).expect("checked installed state");
        let selected = select_latest(&name, &catalog)?;
        if semver_key(&selected.version)? <= semver_key(&current.version)? {
            println!("{name} {} is already up to date", current.version);
            continue;
        }
        install_selected(&root, &name, &selected)?;
    }
    Ok(())
}

pub(crate) fn run_external(args: Vec<OsString>) -> Result<ExitStatus> {
    let root = extension_root()?;
    run_external_with_root(args, &root)
}

pub(crate) fn installed_help() -> String {
    let Ok(root) = extension_root() else {
        return EMPTY_INSTALLED_HELP.to_string();
    };
    let Ok(state) = load_installed_state(&root) else {
        return EMPTY_INSTALLED_HELP.to_string();
    };
    format_installed_help(state)
}

fn format_installed_help(state: InstalledState) -> String {
    if state.extensions.is_empty() {
        return EMPTY_INSTALLED_HELP.to_string();
    }
    let mut out = String::from("\nExtensions:\n");
    for (name, extension) in state.extensions {
        if extension.description.is_empty() {
            out.push_str(&format!("  {name}\n"));
        } else {
            out.push_str(&format!("  {name:<12} {}\n", extension.description));
        }
    }
    out
}

fn list_installed(root: &Path) -> Result<()> {
    let state = load_installed_state(root)?;
    if state.extensions.is_empty() {
        println!("No installed extensions.");
        return Ok(());
    }

    println!("Name              Installed   Description");
    for (name, extension) in state.extensions {
        println!("{name:<17} {:<11} {}", extension.version, extension.description);
    }
    Ok(())
}

fn list_available(root: &Path) -> Result<()> {
    let catalog = fetch_catalog()?;
    let installed = load_installed_state(root)?;
    if catalog.extensions.is_empty() {
        println!("No official extensions available.");
        return Ok(());
    }

    println!("Name              Latest      Installed   Description");
    for (name, extension) in catalog.extensions {
        let latest = select_latest_from_extension(&name, &extension)
            .map(|selected| selected.version)
            .unwrap_or_else(|_| "unsupported".to_string());
        let installed_version = installed
            .extensions
            .get(&name)
            .map(|extension| extension.version.as_str())
            .unwrap_or("-");
        println!("{name:<17} {latest:<11} {installed_version:<11} {}", extension.description);
    }
    Ok(())
}

fn run_external_with_root(args: Vec<OsString>, root: &Path) -> Result<ExitStatus> {
    let mut args = args.into_iter();
    let name = args.next().ok_or_else(|| anyhow!("missing extension name"))?;
    let Some(name) = name.to_str() else {
        bail!("extension name must be valid UTF-8");
    };
    validate_extension_name(name)?;
    let path = managed_binary_path(root, name);
    if !is_executable_file(&path) {
        bail!("unknown recall command '{name}' and official extension is not installed");
    }
    Command::new(&path)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {}", path.display()))
}

fn install_selected(root: &Path, name: &str, selected: &SelectedExtension) -> Result<()> {
    let state = load_installed_state(root)?;
    if state.extensions.get(name).map(|extension| extension.version.as_str())
        == Some(selected.version.as_str())
        && is_executable_file(&managed_binary_path(root, name))
    {
        println!("{name} {} is already installed", selected.version);
        return Ok(());
    }

    fs::create_dir_all(root)?;
    fs::create_dir_all(root.join("bin"))?;
    fs::create_dir_all(root.join("packages").join(name))?;

    println!("Installing {name} {}...", selected.version);
    let bytes = download(&selected.target.url)?;
    verify_sha256(&bytes, &selected.target.sha256)?;

    let temp = tempfile::Builder::new()
        .prefix("install-")
        .tempdir_in(root)
        .context("failed to create extension install temp dir")?;
    unpack_archive(&bytes, &selected.target.url, temp.path())?;

    let binary_name = binary_name(name);
    let unpacked_binary = find_unpacked_binary(temp.path(), &binary_name)?;
    let manifest = read_manifest(&unpacked_binary)?;
    validate_manifest(name, &selected.version, selected.protocol, &selected.min_recall, &manifest)?;

    let package_dir = root.join("packages").join(name).join(&selected.version);
    remove_dir_if_exists(&package_dir)?;
    fs::create_dir_all(&package_dir)?;
    let package_binary = package_dir.join(&binary_name);
    fs::copy(&unpacked_binary, &package_binary)
        .with_context(|| format!("failed to install {}", package_binary.display()))?;
    make_executable(&package_binary)?;
    fs::write(package_dir.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;

    let bin_path = managed_binary_path(root, name);
    fs::copy(&package_binary, &bin_path)
        .with_context(|| format!("failed to install {}", bin_path.display()))?;
    make_executable(&bin_path)?;

    let mut state = load_installed_state(root)?;
    state.extensions.insert(
        name.to_string(),
        InstalledExtension {
            version: selected.version.clone(),
            description: selected.description.clone(),
        },
    );
    save_installed_state(root, &state)?;
    println!("Installed {name} {}", selected.version);
    Ok(())
}

fn fetch_catalog() -> Result<Catalog> {
    let mut response =
        ureq::get(CATALOG_URL).call().with_context(|| format!("failed to fetch {CATALOG_URL}"))?;
    let body = response.body_mut().read_to_string().context("failed to read catalog")?;
    let catalog: Catalog = serde_json::from_str(&body).context("invalid extension catalog JSON")?;
    if catalog.schema != 1 {
        bail!("unsupported extension catalog schema {}", catalog.schema);
    }
    Ok(catalog)
}

fn download(url: &str) -> Result<Vec<u8>> {
    let mut response =
        ureq::get(url).call().with_context(|| format!("failed to download {url}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .context("failed to read extension archive")?;
    Ok(bytes)
}

fn select_latest(name: &str, catalog: &Catalog) -> Result<SelectedExtension> {
    let extension = catalog
        .extensions
        .get(name)
        .ok_or_else(|| anyhow!("unknown official extension: {name}"))?;
    select_latest_from_extension(name, extension)
}

fn select_latest_from_extension(
    name: &str,
    extension: &CatalogExtension,
) -> Result<SelectedExtension> {
    let target_triple = target_triple()?;
    let current_recall = semver_key(env!("CARGO_PKG_VERSION"))?;
    let mut selected: Option<((u64, u64, u64), SelectedExtension)> = None;

    for (version, info) in &extension.versions {
        if info.protocol > crate::PROTOCOL_VERSION {
            continue;
        }
        if semver_key(&info.min_recall)? > current_recall {
            continue;
        }
        let Some(target) = info.targets.get(&target_triple) else {
            continue;
        };
        let key = semver_key(version)?;
        let candidate = SelectedExtension {
            version: version.clone(),
            description: extension.description.clone(),
            protocol: info.protocol,
            min_recall: info.min_recall.clone(),
            target: target.clone(),
        };
        if selected.as_ref().is_none_or(|(selected_key, _)| key > *selected_key) {
            selected = Some((key, candidate));
        }
    }

    selected
        .map(|(_, selected)| selected)
        .ok_or_else(|| anyhow!("no compatible release for extension {name} on {target_triple}"))
}

fn validate_manifest(
    name: &str,
    version: &str,
    protocol: u32,
    min_recall: &str,
    manifest: &ExtensionManifest,
) -> Result<()> {
    if manifest.name != name {
        bail!("manifest name '{}' does not match extension '{name}'", manifest.name);
    }
    if manifest.version != version {
        bail!("manifest version '{}' does not match catalog version '{version}'", manifest.version);
    }
    if manifest.protocol != protocol {
        bail!(
            "manifest protocol {} does not match catalog protocol {}",
            manifest.protocol,
            protocol
        );
    }
    if manifest.min_recall != min_recall {
        bail!(
            "manifest min_recall '{}' does not match catalog min_recall '{}'",
            manifest.min_recall,
            min_recall
        );
    }
    validate_compatibility(manifest)
}

fn validate_compatibility(manifest: &ExtensionManifest) -> Result<()> {
    if manifest.protocol > crate::PROTOCOL_VERSION {
        bail!("extension requires CLI protocol {}; upgrade Recall", manifest.protocol);
    }
    if semver_key(&manifest.min_recall)? > semver_key(env!("CARGO_PKG_VERSION"))? {
        bail!("extension requires Recall {} or later; upgrade Recall", manifest.min_recall);
    }
    if let Some(version) = manifest.transport_version
        && version != transport::VERSION
    {
        bail!("unsupported remote transport version {version}");
    }
    Ok(())
}

fn load_installed_state(root: &Path) -> Result<InstalledState> {
    let path = installed_path(root);
    if !path.exists() {
        return Ok(InstalledState { schema: 1, extensions: BTreeMap::new() });
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let state: InstalledState =
        serde_json::from_str(&content).context("invalid installed extension state")?;
    if state.schema != 1 {
        bail!("unsupported installed extension state schema {}", state.schema);
    }
    Ok(state)
}

fn save_installed_state(root: &Path, state: &InstalledState) -> Result<()> {
    fs::create_dir_all(root)?;
    let path = installed_path(root);
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("failed to write {}", temp.display()))?;
    fs::rename(&temp, &path).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<ExtensionManifest> {
    let output = transport::manifest_output(path)?;
    let manifest: ExtensionManifest =
        serde_json::from_slice(&output).context("invalid extension manifest JSON")?;
    Ok(manifest)
}

fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!("sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn unpack_archive(bytes: &[u8], url: &str, destination: &Path) -> Result<()> {
    if url.ends_with(".zip") {
        let cursor = Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor).context("invalid zip archive")?;
        archive.extract(destination).context("failed to extract zip archive")?;
        return Ok(());
    }
    if url.ends_with(".tar.gz") {
        let decoder = GzDecoder::new(Cursor::new(bytes));
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(destination).context("failed to extract tar.gz archive")?;
        return Ok(());
    }
    bail!("unsupported extension archive type: {url}");
}

fn find_unpacked_binary(root: &Path, binary_name: &str) -> Result<PathBuf> {
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if entry.file_type().is_file() && entry.file_name() == binary_name {
            return Ok(entry.into_path());
        }
    }
    bail!("extension archive did not contain {binary_name}");
}

fn target_triple() -> Result<String> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin".to_string()),
        ("x86_64", "macos") => Ok("x86_64-apple-darwin".to_string()),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu".to_string()),
        ("x86_64", "windows") => Ok("x86_64-pc-windows-msvc".to_string()),
        (arch, os) => bail!("unsupported extension target: {arch}-{os}"),
    }
}

fn semver_key(version: &str) -> Result<(u64, u64, u64)> {
    let core = version.split_once('-').map(|(core, _)| core).unwrap_or(version);
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        bail!("invalid semver: {version}");
    }
    Ok((parts[0].parse()?, parts[1].parse()?, parts[2].parse()?))
}

pub(crate) fn validate_extension_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || !name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        bail!("invalid extension name: {name}");
    }
    Ok(())
}

fn extension_root() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| anyhow!("cannot determine data directory"))?
        .join("recall")
        .join("extensions"))
}

fn installed_path(root: &Path) -> PathBuf {
    root.join("installed.json")
}

fn managed_binary_path(root: &Path, name: &str) -> PathBuf {
    root.join("bin").join(binary_name(name))
}

fn binary_name(name: &str) -> String {
    let extension = std::env::consts::EXE_EXTENSION;
    if extension.is_empty() {
        format!("recall-{name}")
    } else {
        format!("recall-{name}.{extension}")
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
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

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to mark {} executable", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests;
