use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{AppConfig, ShareConfig};
use crate::types::{Message, Session, SessionUsageEventRecord};

use super::assets::{HEADERS, ROBOTS};
use super::meta::collect_session_display_meta;
use super::render::{
    ShareRenderOptions, render_session_html, render_session_html_with_tldr, share_id_for_session,
};

const PROVIDER_CLOUDFLARE_PAGES: &str = "cloudflare-pages";
const PAGES_PROJECT_NAME_FIELD: &str = "Project Name";
const PAGES_PROJECT_DOMAINS_FIELD: &str = "Project Domains";
const MAX_PAGES_ASSET_BYTES: usize = 25 * 1024 * 1024;

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

pub(crate) fn default_preview_dir() -> Result<PathBuf> {
    let root = dirs::cache_dir()
        .or_else(dirs::data_local_dir)
        .or_else(dirs::data_dir)
        .ok_or_else(|| anyhow!("cannot determine cache directory"))?;
    Ok(root.join("recall").join("preview"))
}

pub(crate) fn write_preview_file(
    session: &Session,
    messages: &[Message],
    usage_events: &[SessionUsageEventRecord],
) -> Result<PathBuf> {
    let preview_dir = default_preview_dir()?;
    fs::create_dir_all(&preview_dir)
        .with_context(|| format!("failed to create {}", preview_dir.display()))?;
    let share_id = share_id_for_session(session);
    let file_path = preview_dir.join(format!("{share_id}.html"));
    let display_meta = collect_session_display_meta(session, usage_events);
    let html = render_session_html(session, messages, &display_meta);
    fs::write(&file_path, html)
        .with_context(|| format!("failed to write {}", file_path.display()))?;
    Ok(file_path)
}

pub(crate) fn open_path_in_browser(path: &Path) -> Result<()> {
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

pub(crate) fn open_session_preview(
    session: &Session,
    messages: &[Message],
    usage_events: &[SessionUsageEventRecord],
) -> Result<PathBuf> {
    let path = write_preview_file(session, messages, usage_events)?;
    open_path_in_browser(&path)?;
    Ok(path)
}

pub(crate) fn preview_session_with_options(
    config: &AppConfig,
    session: &Session,
    messages: &[Message],
    usage_events: &[SessionUsageEventRecord],
    options: &ShareRenderOptions,
) -> Result<SharePreview> {
    let (preview, _) = build_publish_preview(config, session, messages, usage_events, options)?;
    Ok(preview)
}

pub(crate) fn publish_session(
    config: &AppConfig,
    session: &Session,
    messages: &[Message],
    usage_events: &[SessionUsageEventRecord],
) -> Result<String> {
    publish_session_with_options(
        config,
        session,
        messages,
        usage_events,
        &ShareRenderOptions::default(),
    )
}

pub(crate) fn publish_session_with_options(
    config: &AppConfig,
    session: &Session,
    messages: &[Message],
    usage_events: &[SessionUsageEventRecord],
    options: &ShareRenderOptions,
) -> Result<String> {
    let (preview, html) = build_publish_preview(config, session, messages, usage_events, options)?;

    init_publish_dir(&preview.publish_dir)?;

    fs::write(&preview.file_path, html)
        .with_context(|| format!("failed to write {}", preview.file_path.display()))?;

    deploy_pages(&preview.publish_dir, &preview.project_name)?;
    Ok(preview.url)
}
fn build_publish_preview(
    config: &AppConfig,
    session: &Session,
    messages: &[Message],
    usage_events: &[SessionUsageEventRecord],
    options: &ShareRenderOptions,
) -> Result<(SharePreview, String)> {
    let share = config
        .share
        .as_ref()
        .ok_or_else(|| anyhow!("sharing is not initialized; run `recall share init` first"))?;
    if share.provider != PROVIDER_CLOUDFLARE_PAGES {
        bail!("unsupported share provider '{}'", share.provider);
    }
    validate_project_name(&share.project_name)?;
    let project_domain = configured_project_domain(share)?;

    let publish_dir = expand_path(&share.publish_dir);

    let share_id = share_id_for_session(session);
    let display_meta = collect_session_display_meta(session, usage_events);
    let tldr = options.tldr_markdown.as_deref().map(str::trim).filter(|tldr| !tldr.is_empty());
    let html = render_session_html_with_tldr(session, messages, &display_meta, tldr);
    if html.len() > MAX_PAGES_ASSET_BYTES {
        bail!("session page is larger than Cloudflare Pages' 25 MiB asset limit");
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
            url: format!("https://{project_domain}/{share_id}"),
            html_bytes: html.len(),
        },
        html,
    ))
}

fn configured_project_domain(share: &ShareConfig) -> Result<String> {
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
fn init_publish_dir(publish_dir: &Path) -> Result<()> {
    fs::create_dir_all(publish_dir)
        .with_context(|| format!("failed to create {}", publish_dir.display()))?;
    fs::write(publish_dir.join("_headers"), HEADERS)?;
    fs::write(publish_dir.join("robots.txt"), ROBOTS)?;
    Ok(())
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

fn deploy_pages(publish_dir: &Path, project_name: &str) -> Result<()> {
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
    use crate::types::Session;

    #[test]
    fn share_id_prefers_source_id() {
        assert_eq!(
            share_id_for_session(&session("019e6d8d-588b-7fd2-a326-c525469ed120")),
            "019e6d8d-588b-7fd2-a326-c525469ed120"
        );
    }

    #[test]
    fn default_project_name_is_specific_and_valid() {
        let name = default_project_name();
        assert!(name.starts_with("recall-share-"));
        assert_eq!(name.len(), "recall-share-".len() + 6);
        validate_project_name(&name).unwrap();
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

    fn session(source_id: &str) -> Session {
        Session {
            id: "local-id".to_string(),
            source: "codex".to_string(),
            source_id: source_id.to_string(),
            title: "Fix <bug>".to_string(),
            directory: Some("/tmp/project".to_string()),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 0,
            updated_at: None,
            message_count: 1,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }
}
