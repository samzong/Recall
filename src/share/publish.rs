use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{AppConfig, ShareConfig};
use crate::types::{Message, Session, SessionEventRecord, SessionUsageEventRecord};

use super::assets::{HEADERS, ROBOTS};
use super::inventory::parse_bundle_sessions_meta;
use super::meta::collect_session_display_meta;
use super::render::{
    BundledSessionInput, ShareRenderOptions, render_bundle_html, render_session_preview_html,
    share_id_for_session,
};

const PROVIDER_CLOUDFLARE_PAGES: &str = "cloudflare-pages";
const PAGES_PROJECT_NAME_FIELD: &str = "Project Name";
const PAGES_PROJECT_DOMAINS_FIELD: &str = "Project Domains";
const MAX_PAGES_ASSET_BYTES: usize = 25 * 1024 * 1024;
const PUBLISH_DIR_MARKER: &str = ".recall-share";
const PUBLISH_DIR_MARKER_CONTENT: &str = "Recall-managed share directory\n";
const LEGACY_HEADERS: &str = "/*\n  X-Robots-Tag: noindex, nofollow\n  X-Frame-Options: DENY\n  X-Content-Type-Options: nosniff\n  Referrer-Policy: no-referrer\n";
const SHARE_HTML_PREFIX: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"robots\" content=\"noindex,nofollow\">";
const SHARE_HTML_SIGNATURES: [&str; 3] = [
    "body{margin:0;background:#f6f7f9;color:#17181c;font:15px/1.6",
    "--page-bg: #F5F5F7;\n  --content-bg: #FFFFFF;",
    "--page-bg:#FAF9F6;--surface:#FFFFFF;--user-surface:#FFFFFF;",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct BundleDiff {
    pub(crate) added: Vec<String>,
    pub(crate) removed: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct PublishBundleResult {
    pub(crate) url: String,
    pub(crate) session_count: usize,
    pub(crate) bundle_diff: Option<BundleDiff>,
}

#[derive(Debug, Clone)]
pub(crate) struct SharePreview {
    pub(crate) provider: String,
    pub(crate) project_name: String,
    pub(crate) project_domain: String,
    pub(crate) publish_dir: PathBuf,
    pub(crate) file_path: PathBuf,
    pub(crate) share_id: String,
    pub(crate) url: String,
    pub(crate) html_bytes: usize,
    pub(crate) bundle_diff: Option<BundleDiff>,
}

pub(crate) fn default_project_name() -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("recall-share-{}", &suffix[..6])
}

pub(crate) fn default_publish_dir() -> Result<PathBuf> {
    if let Some(dir) = dirs::data_local_dir().or_else(dirs::data_dir) {
        return Ok(dir.join("recall").join("share-pages"));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(home.join(".local").join("share").join("recall").join("share-pages"))
}

pub(crate) fn expand_path(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

pub(crate) fn preflight_cloudflare_pages() -> Result<()> {
    ensure_wrangler_available()?;
    ensure_wrangler_login()?;
    list_pages_projects()?;
    Ok(())
}

pub(crate) fn init_cloudflare_pages(
    config: &mut AppConfig,
    project_name: String,
    publish_dir: PathBuf,
) -> Result<()> {
    validate_project_name(&project_name)?;
    ensure_wrangler_available()?;
    ensure_wrangler_login()?;
    ensure_pages_project(&project_name)?;
    let project_domain = resolve_pages_project_domain(&project_name)?;
    init_publish_dir(&publish_dir)?;
    config.share = Some(ShareConfig {
        provider: PROVIDER_CLOUDFLARE_PAGES.to_string(),
        project_name,
        project_domain,
        publish_dir: publish_dir.to_string_lossy().to_string(),
    });
    config.save()
}

pub(crate) fn create_session_preview(
    session: &Session,
    messages: &[Message],
    events: &[SessionEventRecord],
    usage_events: &[SessionUsageEventRecord],
) -> Result<tempfile::NamedTempFile> {
    let display_meta = collect_session_display_meta(session, usage_events);
    let html = render_session_preview_html(session, messages, events, &display_meta);
    write_temp_preview_html(&html)
}

pub(crate) fn create_bundle_preview(
    sessions: &[BundledSessionInput],
) -> Result<tempfile::NamedTempFile> {
    let html = render_bundle_html(sessions, None, false)?;
    write_temp_preview_html(&html)
}

fn write_temp_preview_html(html: &str) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("recall-preview-")
        .suffix(".html")
        .tempfile()
        .context("failed to create temporary preview")?;
    file.write_all(html.as_bytes()).context("failed to write temporary preview")?;
    file.flush().context("failed to flush temporary preview")?;
    Ok(file)
}

pub(crate) fn open_preview_file(path: &Path) -> Result<()> {
    let path_arg = path.as_os_str();
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(path_arg).status()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", "", &path.to_string_lossy()]).status()
    } else {
        Command::new("xdg-open").arg(path_arg).status()
    }
    .map_err(|e| anyhow!("failed to open browser for {}: {e}", path.display()))?;
    if status.success() {
        Ok(())
    } else {
        bail!("failed to open browser for {}", path.display());
    }
}

pub(crate) fn preview_bundle_with_options(
    config: &AppConfig,
    sessions: &[BundledSessionInput],
    options: &ShareRenderOptions,
) -> Result<SharePreview> {
    let (preview, _) = build_bundle_publish_preview(config, sessions, options)?;
    Ok(preview)
}

pub(crate) fn dry_run_bundle_to_output(
    config: &AppConfig,
    sessions: &[BundledSessionInput],
    options: &ShareRenderOptions,
    output: &Path,
) -> Result<SharePreview> {
    let tldr = options.tldr_markdown.as_deref().map(str::trim).filter(|tldr| !tldr.is_empty());
    let html = render_bundle_html(sessions, tldr, true)?;
    if html.len() > MAX_PAGES_ASSET_BYTES {
        bail!(
            "session page is {} bytes ({} sessions), which exceeds Cloudflare Pages' {} byte (25 MiB) asset limit",
            html.len(),
            sessions.len(),
            MAX_PAGES_ASSET_BYTES
        );
    }
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(output, html.as_bytes())
        .with_context(|| format!("failed to write {}", output.display()))?;

    let primary = &sessions[0].session;
    let share_id = share_id_for_session(primary);
    if let Some(share) = config.share.as_ref()
        && share.provider == PROVIDER_CLOUDFLARE_PAGES
        && validate_project_name(&share.project_name).is_ok()
    {
        let project_domain = configured_project_domain(share).unwrap_or_default();
        let publish_dir = expand_path(&share.publish_dir);
        let existing_file = publish_dir.join(format!("{share_id}.html"));
        let bundle_diff = compute_bundle_diff(&existing_file, primary, sessions);
        let url = if project_domain.is_empty() {
            String::new()
        } else {
            share_page_url(&project_domain, &share_id)
        };
        return Ok(SharePreview {
            provider: share.provider.clone(),
            project_name: share.project_name.clone(),
            project_domain,
            publish_dir,
            file_path: output.to_path_buf(),
            share_id,
            url,
            html_bytes: html.len(),
            bundle_diff,
        });
    }

    Ok(SharePreview {
        provider: PROVIDER_CLOUDFLARE_PAGES.to_string(),
        project_name: String::new(),
        project_domain: String::new(),
        publish_dir: PathBuf::new(),
        file_path: output.to_path_buf(),
        share_id,
        url: String::new(),
        html_bytes: html.len(),
        bundle_diff: None,
    })
}

pub(crate) fn publish_bundle_with_options(
    config: &AppConfig,
    sessions: &[BundledSessionInput],
    options: &ShareRenderOptions,
) -> Result<PublishBundleResult> {
    let (preview, html) = build_bundle_publish_preview(config, sessions, options)?;
    init_publish_dir(&preview.publish_dir)?;
    fs::write(&preview.file_path, html)
        .with_context(|| format!("failed to write {}", preview.file_path.display()))?;
    deploy_pages(&preview.publish_dir, &preview.project_name)?;
    Ok(PublishBundleResult {
        url: preview.url,
        session_count: sessions.len(),
        bundle_diff: preview.bundle_diff,
    })
}

pub(super) fn require_share_config(config: &AppConfig) -> Result<&ShareConfig> {
    let share = config
        .share
        .as_ref()
        .ok_or_else(|| anyhow!("sharing is not initialized; run `recall share init` first"))?;
    if share.provider != PROVIDER_CLOUDFLARE_PAGES {
        bail!("unsupported share provider '{}'", share.provider);
    }
    validate_project_name(&share.project_name)?;
    Ok(share)
}

pub(super) fn share_page_url(project_domain: &str, share_id: &str) -> String {
    format!("https://{project_domain}/{share_id}")
}

fn html_is_recall_share_page(html: &str) -> bool {
    html.starts_with(SHARE_HTML_PREFIX)
        && SHARE_HTML_SIGNATURES.iter().any(|signature| html.contains(signature))
        && html.ends_with("</body></html>")
}

fn compute_bundle_diff(
    existing_path: &Path,
    primary: &Session,
    sessions: &[BundledSessionInput],
) -> Option<BundleDiff> {
    if !existing_path.is_file() {
        return None;
    }
    let existing_html = fs::read_to_string(existing_path).ok()?;
    if !html_is_recall_share_page(&existing_html) {
        return None;
    }
    let prior_keys: Vec<String> = parse_bundle_sessions_meta(&existing_html)
        .unwrap_or_else(|| vec![format!("{}:{}", primary.source, primary.source_id)]);
    let next_keys: Vec<String> =
        sessions.iter().map(|s| format!("{}:{}", s.session.source, s.session.source_id)).collect();
    let prior_set: HashSet<&str> = prior_keys.iter().map(String::as_str).collect();
    let next_set: HashSet<&str> = next_keys.iter().map(String::as_str).collect();
    let added: Vec<String> =
        next_keys.iter().filter(|k| !prior_set.contains(k.as_str())).cloned().collect();
    let removed: Vec<String> =
        prior_keys.iter().filter(|k| !next_set.contains(k.as_str())).cloned().collect();
    if added.is_empty() && removed.is_empty() { None } else { Some(BundleDiff { added, removed }) }
}

fn build_bundle_publish_preview(
    config: &AppConfig,
    sessions: &[BundledSessionInput],
    options: &ShareRenderOptions,
) -> Result<(SharePreview, String)> {
    let share = require_share_config(config)?;
    let project_domain = configured_project_domain(share)?;
    let publish_dir = expand_path(&share.publish_dir);
    let primary = &sessions[0].session;
    let share_id = share_id_for_session(primary);
    let tldr = options.tldr_markdown.as_deref().map(str::trim).filter(|tldr| !tldr.is_empty());
    let html = render_bundle_html(sessions, tldr, true)?;
    if html.len() > MAX_PAGES_ASSET_BYTES {
        bail!(
            "session page is {} bytes ({} sessions), which exceeds Cloudflare Pages' {} byte (25 MiB) asset limit",
            html.len(),
            sessions.len(),
            MAX_PAGES_ASSET_BYTES
        );
    }
    let file_path = publish_dir.join(format!("{share_id}.html"));
    let bundle_diff = compute_bundle_diff(&file_path, primary, sessions);
    Ok((
        SharePreview {
            provider: share.provider.clone(),
            project_name: share.project_name.clone(),
            project_domain: project_domain.clone(),
            publish_dir,
            file_path,
            share_id: share_id.clone(),
            url: share_page_url(&project_domain, &share_id),
            html_bytes: html.len(),
            bundle_diff,
        },
        html,
    ))
}

#[cfg(test)]
fn build_publish_preview(
    config: &AppConfig,
    session: &Session,
    messages: &[Message],
    events: &[SessionEventRecord],
    usage_events: &[SessionUsageEventRecord],
    options: &ShareRenderOptions,
) -> Result<(SharePreview, String)> {
    let share = require_share_config(config)?;
    let project_domain = configured_project_domain(share)?;

    let publish_dir = expand_path(&share.publish_dir);

    let share_id = share_id_for_session(session);
    let display_meta = collect_session_display_meta(session, usage_events);
    let tldr = options.tldr_markdown.as_deref().map(str::trim).filter(|tldr| !tldr.is_empty());
    let html = super::render::render_session_html_with_tldr(
        session,
        messages,
        events,
        &display_meta,
        tldr,
    );
    if html.len() > MAX_PAGES_ASSET_BYTES {
        bail!(
            "session page is {} bytes, which exceeds Cloudflare Pages' {} byte (25 MiB) asset limit",
            html.len(),
            MAX_PAGES_ASSET_BYTES
        );
    }

    let file_path = publish_dir.join(format!("{share_id}.html"));
    Ok((
        SharePreview {
            provider: share.provider.clone(),
            project_name: share.project_name.clone(),
            project_domain: project_domain.clone(),
            publish_dir,
            file_path,
            share_id: share_id.clone(),
            url: share_page_url(&project_domain, &share_id),
            html_bytes: html.len(),
            bundle_diff: None,
        },
        html,
    ))
}

pub(super) fn configured_project_domain(share: &ShareConfig) -> Result<String> {
    if share.project_domain.is_empty() {
        resolve_pages_project_domain(&share.project_name).with_context(|| {
            format!(
                "share config is missing project_domain and Cloudflare Pages project '{}' \
                 was not found in the current account; run `recall share init` to \
                 create or select a Pages project",
                share.project_name
            )
        })
    } else {
        Ok(share.project_domain.clone())
    }
}

pub(super) fn ensure_readable_publish_dir(publish_dir: &Path) -> Result<bool> {
    if !publish_dir.exists() {
        return Ok(false);
    }
    if publish_dir_is_unmanaged(publish_dir, publish_dir_has_marker(publish_dir))? {
        bail!("publish directory {} is not managed by Recall", publish_dir.display());
    }
    Ok(true)
}

fn publish_dir_has_marker(publish_dir: &Path) -> bool {
    let marker = publish_dir.join(PUBLISH_DIR_MARKER);
    fs::read_to_string(&marker).is_ok_and(|contents| contents == PUBLISH_DIR_MARKER_CONTENT)
}

pub(super) fn init_publish_dir(publish_dir: &Path) -> Result<()> {
    fs::create_dir_all(publish_dir)
        .with_context(|| format!("failed to create {}", publish_dir.display()))?;
    let managed = publish_dir_has_marker(publish_dir);
    if publish_dir_is_unmanaged(publish_dir, managed)? {
        bail!(
            "publish directory {} is not managed by Recall; choose an empty directory",
            publish_dir.display()
        );
    }
    if !managed {
        fs::write(publish_dir.join(PUBLISH_DIR_MARKER), PUBLISH_DIR_MARKER_CONTENT)?;
    }
    fs::write(publish_dir.join("_headers"), HEADERS)?;
    fs::write(publish_dir.join("robots.txt"), ROBOTS)?;
    Ok(())
}

fn publish_dir_is_unmanaged(publish_dir: &Path, managed: bool) -> Result<bool> {
    let mut has_entries = false;
    let mut has_headers = false;
    let mut has_robots = false;
    for entry in fs::read_dir(publish_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Ok(true);
        }
        has_entries = true;
        match entry.file_name().to_str() {
            Some(PUBLISH_DIR_MARKER) if managed => {}
            Some("_headers") => {
                let contents = fs::read_to_string(entry.path())?;
                if contents != HEADERS && contents != LEGACY_HEADERS {
                    return Ok(true);
                }
                has_headers = true;
            }
            Some("robots.txt") => {
                if fs::read_to_string(entry.path())? != ROBOTS {
                    return Ok(true);
                }
                has_robots = true;
            }
            Some(name) if name.ends_with(".html") => {
                let html = fs::read_to_string(entry.path())?;
                if !html_is_recall_share_page(&html) {
                    return Ok(true);
                }
            }
            _ => return Ok(true),
        }
    }
    Ok(!managed && has_entries && !(has_headers && has_robots))
}

fn ensure_wrangler_available() -> Result<()> {
    let output = wrangler_command()?
        .arg("--version")
        .output()
        .map_err(|e| anyhow!("wrangler is not available on PATH: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("wrangler --version failed: {}", command_output_text(&output));
    }
}

fn ensure_wrangler_login() -> Result<()> {
    let output = wrangler_command()?
        .arg("whoami")
        .output()
        .map_err(|e| anyhow!("failed to run wrangler whoami: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("wrangler is not logged in: {}", command_output_text(&output));
    }
}

fn ensure_pages_project(project_name: &str) -> Result<()> {
    if json_has_project_name(&list_pages_projects()?, project_name) {
        return Ok(());
    }
    let status = wrangler_command()?
        .args(["pages", "project", "create", project_name, "--production-branch", "main"])
        .status()
        .map_err(|e| anyhow!("failed to run wrangler pages project create: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("failed to create Cloudflare Pages project '{project_name}'");
    }
}

fn list_pages_projects() -> Result<serde_json::Value> {
    let output = wrangler_command()?
        .args(["pages", "project", "list", "--json"])
        .output()
        .map_err(|e| anyhow!("failed to run wrangler pages project list: {e}"))?;
    if !output.status.success() {
        bail!("failed to list Cloudflare Pages projects: {}", command_output_text(&output));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| anyhow!("failed to parse wrangler project list JSON: {e}"))
}

pub(super) fn deploy_pages(publish_dir: &Path, project_name: &str) -> Result<()> {
    let output = wrangler_command()?
        .args(["pages", "deploy"])
        .arg(publish_dir)
        .args(["--project-name", project_name])
        .output()
        .map_err(|e| anyhow!("failed to run wrangler pages deploy: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("wrangler pages deploy failed: {}", command_output_text(&output));
    }
}

fn wrangler_command() -> Result<Command> {
    let mut command = Command::new("wrangler");
    command.current_dir(wrangler_work_dir()?);
    Ok(command)
}

fn wrangler_work_dir() -> Result<PathBuf> {
    let root = dirs::cache_dir()
        .or_else(dirs::data_local_dir)
        .or_else(dirs::data_dir)
        .ok_or_else(|| anyhow!("cannot determine cache directory"))?;
    let dir = root.join("recall").join("wrangler");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

fn command_output_text(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() { "no output".to_string() } else { stdout }
}

fn resolve_pages_project_domain(project_name: &str) -> Result<String> {
    lookup_pages_project_domain(&list_pages_projects()?, project_name).ok_or_else(|| {
        anyhow!("Cloudflare Pages project '{project_name}' was not found in wrangler project list")
    })
}

fn json_has_project_name(value: &serde_json::Value, project_name: &str) -> bool {
    lookup_pages_project_domain(value, project_name).is_some()
}

pub(crate) fn lookup_pages_project_domain(
    value: &serde_json::Value,
    project_name: &str,
) -> Option<String> {
    let projects = value.as_array()?;
    for project in projects {
        let name = project.get(PAGES_PROJECT_NAME_FIELD).and_then(|v| v.as_str())?;
        if name != project_name {
            continue;
        }
        let domains = project.get(PAGES_PROJECT_DOMAINS_FIELD).and_then(|v| v.as_str())?;
        return pages_dev_host_from_domains(domains);
    }
    None
}

fn pages_dev_host_from_domains(domains: &str) -> Option<String> {
    domains
        .split(',')
        .map(str::trim)
        .find(|domain| domain.ends_with(".pages.dev"))
        .map(str::to_string)
}

pub(crate) fn validate_project_name(project_name: &str) -> Result<()> {
    let valid = !project_name.is_empty()
        && project_name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && project_name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && project_name
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        bail!("Cloudflare Pages project name must use lowercase letters, digits, and hyphens");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::render::render_session_html;
    use crate::share::test_session as session;
    use crate::types::{Message, Role, SessionEventRecord};

    #[test]
    fn share_id_prefers_source_id() {
        assert_eq!(
            share_id_for_session(&session("019e6d8d-588b-7fd2-a326-c525469ed120")),
            "019e6d8d-588b-7fd2-a326-c525469ed120"
        );
    }

    #[test]
    fn lookup_pages_project_domain_uses_wrangler_project_domains() {
        let projects = serde_json::json!([
            {
                "Project Name": "other",
                "Project Domains": "other.pages.dev"
            },
            {
                "Project Name": "recall-share-a1b2c3d4e5f6",
                "Project Domains": "custom.example.com, recall-share-a1b2c3d4e5f6.pages.dev"
            }
        ]);
        assert_eq!(
            lookup_pages_project_domain(&projects, "recall-share-a1b2c3d4e5f6").as_deref(),
            Some("recall-share-a1b2c3d4e5f6.pages.dev")
        );
        assert_eq!(lookup_pages_project_domain(&projects, "missing"), None);
    }

    #[test]
    fn share_id_sanitizes_path_chars() {
        assert_eq!(share_id_for_session(&session("foo/bar baz")), "foo-bar-baz");
    }

    #[test]
    fn local_preview_and_publish_dry_run_share_identical_timeline_semantics() {
        let session = session("same-timeline");
        let messages = [Message {
            session_id: "local-id".to_string(),
            role: Role::Assistant,
            content: "Answer".to_string(),
            timestamp: None,
            seq: 0,
        }];
        let events = [session_event(0, "tool_call"), session_event(1, "tool_result")];
        let local = create_session_preview(&session, &messages, &events, &[]).unwrap();
        let local_html = fs::read_to_string(local.path()).unwrap();
        let publish_dir = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.share = Some(ShareConfig {
            provider: PROVIDER_CLOUDFLARE_PAGES.to_string(),
            project_name: "recall-share-test".to_string(),
            project_domain: "recall-share-test.pages.dev".to_string(),
            publish_dir: publish_dir.path().to_string_lossy().to_string(),
        });
        let (_, published_html) = build_publish_preview(
            &config,
            &session,
            &messages,
            &events,
            &[],
            &ShareRenderOptions::default(),
        )
        .unwrap();

        let normalized = published_html
            .replace("<link rel=\"preconnect\" href=\"https://fonts.googleapis.com\">", "")
            .replace(
                "<link rel=\"preconnect\" href=\"https://fonts.gstatic.com\" crossorigin>",
                "",
            )
            .replace("<link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2?family=Newsreader:opsz,wght@6..72,400;6..72,500;6..72,600&family=JetBrains+Mono:wght@400;500&display=swap\">", "");
        assert_eq!(local_html, normalized);
        assert!(local_html.contains("call + result"));
        assert!(
            local_html.find("Target: src/lib.rs").unwrap()
                < local_html.find("Result: Tool result").unwrap()
        );
    }

    #[test]
    fn publish_dir_rejects_unmanaged_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("canary-secret"), "do not publish").unwrap();

        let error = init_publish_dir(dir.path()).unwrap_err();

        assert!(error.to_string().contains("not managed by Recall"));
        assert!(!dir.path().join("_headers").exists());
        assert!(!dir.path().join("robots.txt").exists());

        let spoofed = tempfile::tempdir().unwrap();
        fs::write(spoofed.path().join(PUBLISH_DIR_MARKER), "not Recall").unwrap();
        assert!(init_publish_dir(spoofed.path()).is_err());

        let managed = tempfile::tempdir().unwrap();
        init_publish_dir(managed.path()).unwrap();
        assert!(managed.path().join(PUBLISH_DIR_MARKER).is_file());
        init_publish_dir(managed.path()).unwrap();
        fs::write(managed.path().join("later-canary"), "do not publish").unwrap();
        assert!(init_publish_dir(managed.path()).is_err());

        let legacy = tempfile::tempdir().unwrap();
        fs::write(legacy.path().join("_headers"), HEADERS).unwrap();
        fs::write(legacy.path().join("robots.txt"), ROBOTS).unwrap();
        let session = session("legacy-session");
        let meta = collect_session_display_meta(&session, &[]);
        fs::write(
            legacy.path().join("legacy-session.html"),
            render_session_html(&session, &[], &[], &meta),
        )
        .unwrap();
        init_publish_dir(legacy.path()).unwrap();
        assert!(legacy.path().join(PUBLISH_DIR_MARKER).is_file());
        init_publish_dir(legacy.path()).unwrap();
    }

    #[test]
    fn dry_run_output_and_overwrite_diff_work_with_multi_session_bundle() {
        use crate::session::related::RelatedRelation;
        use crate::share::meta::SessionDisplayMeta;
        use crate::types::{Message, Role};

        let publish_dir = tempfile::tempdir().unwrap();
        init_publish_dir(publish_dir.path()).unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let out_path = output_dir.path().join("nested/share.html");

        let mut config = AppConfig::default();
        config.share = Some(ShareConfig {
            provider: PROVIDER_CLOUDFLARE_PAGES.to_string(),
            project_name: "recall-share-test".to_string(),
            project_domain: "recall-share-test.pages.dev".to_string(),
            publish_dir: publish_dir.path().to_string_lossy().to_string(),
        });

        let mut s1 = session("root-src");
        s1.id = "r1".into();
        s1.source = "codex".into();
        let mut s2 = session("child-a");
        s2.id = "c1".into();
        s2.source = "claude-code".into();
        let mut s3 = session("child-b");
        s3.id = "c2".into();
        s3.source = "codex".into();

        let make_input = |s: Session, parent: Option<&str>, depth: usize| BundledSessionInput {
            messages: vec![Message {
                session_id: s.id.clone(),
                role: Role::User,
                content: "hi".into(),
                timestamp: None,
                seq: 0,
            }],
            events: vec![],
            display_meta: SessionDisplayMeta::default(),
            parent_id: parent.map(String::from),
            depth,
            relation: parent.map(|_| RelatedRelation::Spawn),
            via: None,
            role_label: None,
            display_title: s.title.clone(),
            spawn_anchor: None,
            session: s,
        };

        let prior_bundle = vec![make_input(s1.clone(), None, 0), make_input(s2, Some("r1"), 1)];
        let prior_html = render_bundle_html(&prior_bundle, None, true).unwrap();
        fs::write(publish_dir.path().join("root-src.html"), &prior_html).unwrap();
        init_publish_dir(publish_dir.path()).unwrap();

        let next_bundle = vec![make_input(s1, None, 0), make_input(s3, Some("r1"), 1)];
        let preview = dry_run_bundle_to_output(
            &config,
            &next_bundle,
            &ShareRenderOptions::default(),
            &out_path,
        )
        .unwrap();
        assert!(out_path.is_file());
        let written = fs::read_to_string(&out_path).unwrap();
        assert!(written.contains("codex:child-b"));
        assert_eq!(
            fs::read_to_string(publish_dir.path().join("root-src.html")).unwrap(),
            prior_html
        );
        assert_eq!(
            preview.bundle_diff,
            Some(BundleDiff {
                added: vec!["codex:child-b".to_string()],
                removed: vec!["claude-code:child-a".to_string()],
            })
        );
    }

    fn session_event(event_seq: u32, kind: &str) -> SessionEventRecord {
        SessionEventRecord {
            command_evidence_status: None,
            files: Vec::new(),
            event_seq,
            timestamp: None,
            kind: kind.to_string(),
            actor: if kind == "tool_result" { "tool" } else { "assistant" }.to_string(),
            name: (kind == "tool_call").then(|| "Read".to_string()),
            status: None,
            target: (kind == "tool_call").then(|| "src/lib.rs".to_string()),
            message_seq: Some(0),
            summary: None,
            source_path: None,
            source_event_id: Some(format!("source-{event_seq}")),
            tool_call_id: Some("call-1".to_string()),
            is_meta: None,
            visibility: None,
            attrs_json: None,
            parser_version: 1,
        }
    }
}
