use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{AppConfig, ShareConfig};
use crate::types::{Message, Role, Session};
use crate::utils;

const PROVIDER_CLOUDFLARE_PAGES: &str = "cloudflare-pages";
const PAGES_PROJECT_NAME_FIELD: &str = "Project Name";
const PAGES_PROJECT_DOMAINS_FIELD: &str = "Project Domains";
const MAX_PAGES_ASSET_BYTES: usize = 25 * 1024 * 1024;
const HEADERS: &str = "/*\n  X-Robots-Tag: noindex, nofollow\n  X-Frame-Options: DENY\n  X-Content-Type-Options: nosniff\n  Referrer-Policy: no-referrer\n";
const ROBOTS: &str = "User-agent: *\nDisallow: /\n";

#[derive(Debug, Clone)]
pub struct SharePreview {
    pub provider: String,
    pub project_name: String,
    pub project_domain: String,
    pub publish_dir: PathBuf,
    pub file_path: PathBuf,
    pub share_id: String,
    pub url: String,
    pub html_bytes: usize,
}

pub fn default_project_name() -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("recall-share-{}", &suffix[..6])
}

pub fn default_publish_dir() -> Result<PathBuf> {
    if let Some(dir) = dirs::data_local_dir().or_else(dirs::data_dir) {
        return Ok(dir.join("recall").join("share-pages"));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(home.join(".local").join("share").join("recall").join("share-pages"))
}

pub fn expand_path(path: &str) -> PathBuf {
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

pub fn preflight_cloudflare_pages() -> Result<()> {
    ensure_wrangler_available()?;
    ensure_wrangler_login()?;
    list_pages_projects()?;
    Ok(())
}

pub fn init_cloudflare_pages(
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

pub fn publish_session(
    config: &AppConfig,
    session: &Session,
    messages: &[Message],
) -> Result<String> {
    let preview = preview_session(config, session, messages)?;
    let html = render_session_html(session, messages);

    init_publish_dir(&preview.publish_dir)?;

    fs::write(&preview.file_path, html)
        .with_context(|| format!("failed to write {}", preview.file_path.display()))?;

    deploy_pages(&preview.publish_dir, &preview.project_name)?;
    Ok(preview.url)
}

pub fn preview_session(
    config: &AppConfig,
    session: &Session,
    messages: &[Message],
) -> Result<SharePreview> {
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
    let html = render_session_html(session, messages);
    if html.len() > MAX_PAGES_ASSET_BYTES {
        bail!("session page is larger than Cloudflare Pages' 25 MiB asset limit");
    }

    let file_path = publish_dir.join(format!("{share_id}.html"));
    Ok(SharePreview {
        provider: share.provider.clone(),
        project_name: share.project_name.clone(),
        project_domain: project_domain.clone(),
        publish_dir,
        file_path,
        share_id: share_id.clone(),
        url: format!("https://{project_domain}/{share_id}"),
        html_bytes: html.len(),
    })
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

pub fn share_id_for_session(session: &Session) -> String {
    let candidate =
        if session.source_id.trim().is_empty() { &session.id } else { &session.source_id };
    let mut out = String::with_capacity(candidate.len());
    for c in candidate.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() { session.id.clone() } else { trimmed }
}

pub fn render_session_html(session: &Session, messages: &[Message]) -> String {
    let title = session.custom_title.as_deref().unwrap_or(&session.title);
    let mut out = String::new();
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    out.push_str("<meta name=\"robots\" content=\"noindex,nofollow\">");
    out.push_str("<title>");
    out.push_str(&escape_html(title));
    out.push_str("</title><style>");
    out.push_str("body{margin:0;background:#f6f7f9;color:#17181c;font:15px/1.6 -apple-system,BlinkMacSystemFont,\"Segoe UI\",sans-serif}main{max-width:960px;margin:0 auto;padding:32px 20px 56px}header{border-bottom:1px solid #d8dce3;margin-bottom:24px;padding-bottom:18px}h1{font-size:26px;line-height:1.25;margin:0 0 12px}.meta{color:#687080;font-size:13px;display:flex;flex-wrap:wrap;gap:10px 18px}.msg{background:#fff;border:1px solid #dde1e8;border-radius:8px;margin:16px 0;overflow:hidden;box-shadow:0 1px 2px rgba(15,23,42,.04)}.role{font-size:12px;font-weight:700;letter-spacing:.04em;text-transform:uppercase;padding:10px 16px;border-bottom:1px solid #eef0f4;color:#596171;background:#fbfcfe}.user .role{color:#0f766e}.assistant .role{color:#6d28d9}.text{white-space:pre-wrap;word-break:break-word;padding:16px 18px;font-size:15px}.assistant .text{font-size:16px}.tool{border-top:1px solid #eef0f4;background:#fafbfc}.tool summary{cursor:pointer;padding:10px 16px;color:#687080;font-size:13px}.tool pre{white-space:pre-wrap;word-break:break-word;margin:0;padding:12px 16px 16px;color:#3f4654;background:#f3f5f8;font:12px/1.5 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}.empty{color:#687080;background:#fff;border:1px solid #dde1e8;border-radius:8px;padding:16px}</style></head><body><main>");
    out.push_str("<header><h1>");
    out.push_str(&escape_html(title));
    out.push_str("</h1><div class=\"meta\"><span>");
    out.push_str(&escape_html(&session.source));
    out.push_str("</span><span>");
    out.push_str(&escape_html(&format_started_at(session.started_at)));
    out.push_str("</span><span>");
    out.push_str(&messages.len().to_string());
    out.push_str(" messages</span>");
    out.push_str("</div></header>");
    if messages.is_empty() {
        out.push_str("<div class=\"empty\">No messages in this session.</div>");
    } else {
        for message in messages {
            render_message_html(&mut out, message);
        }
    }
    out.push_str("</main></body></html>");
    out
}

fn render_message_html(out: &mut String, message: &Message) {
    let role = match message.role {
        Role::User => "User",
        Role::Assistant => "Assistant",
    };
    let class = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    out.push_str("<section class=\"msg ");
    out.push_str(class);
    out.push_str("\"><div class=\"role\">");
    out.push_str(role);
    out.push_str("</div>");

    let mut text = String::new();
    let mut rendered = false;
    for line in message.content.lines() {
        let sanitized = utils::sanitize_line(line);
        if is_tool_line(&sanitized) {
            if !text.trim().is_empty() {
                render_text_segment(out, &text);
            }
            text.clear();
            render_tool_segment(out, &sanitized);
            rendered = true;
        } else {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&sanitized);
        }
    }
    if !text.trim().is_empty() {
        render_text_segment(out, &text);
        rendered = true;
    }
    if !rendered && !message.content.is_empty() {
        let sanitized =
            message.content.lines().map(utils::sanitize_line).collect::<Vec<_>>().join("\n");
        render_text_segment(out, &sanitized);
    }
    out.push_str("</section>");
}

fn render_text_segment(out: &mut String, text: &str) {
    out.push_str("<div class=\"text\">");
    out.push_str(&escape_html(text.trim()));
    out.push_str("</div>");
}

fn render_tool_segment(out: &mut String, text: &str) {
    out.push_str("<details class=\"tool\"><summary>");
    out.push_str(&escape_html(&tool_summary(text)));
    out.push_str("</summary><pre>");
    out.push_str(&escape_html(text));
    out.push_str("</pre></details>");
}

fn is_tool_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("[tool:")
        || trimmed.starts_with("[tool_result:")
        || trimmed.starts_with("[tool_use:")
}

fn tool_summary(line: &str) -> String {
    let trimmed = line.trim_start();
    for (prefix, label) in
        [("[tool:", "Tool call"), ("[tool_result:", "Tool result"), ("[tool_use:", "Tool use")]
    {
        if let Some(name) = trimmed
            .strip_prefix(prefix)
            .and_then(|rest| rest.split(']').next())
            .filter(|name| !name.trim().is_empty())
        {
            return format!("{label}: {name}");
        }
    }
    "Tool event".to_string()
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

fn lookup_pages_project_domain(value: &serde_json::Value, project_name: &str) -> Option<String> {
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

fn validate_project_name(project_name: &str) -> Result<()> {
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

fn format_started_at(started_at: i64) -> String {
    chrono::DateTime::from_timestamp_millis(started_at)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown time".to_string())
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(source_id: &str) -> Session {
        Session {
            id: "local-id".to_string(),
            source: "codex".to_string(),
            source_id: source_id.to_string(),
            title: "Fix <bug>".to_string(),
            directory: Some("/tmp/project".to_string()),
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

    #[test]
    fn html_renderer_escapes_content() {
        let html = render_session_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::User,
                content: "<script>alert('x')</script>".to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
        assert!(!html.contains("<script>alert"));
    }

    #[test]
    fn html_renderer_omits_local_directory() {
        let html = render_session_html(&session("s1"), &[]);
        assert!(!html.contains("/tmp/project"));
    }

    #[test]
    fn html_renderer_collapses_tool_lines() {
        let html = render_session_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::Assistant,
                content: "I will inspect it.\n[tool:run_terminal_command_v2]\n[tool_result:run_terminal_command_v2] {\"output\":\"huge\"}\nThe answer is here.".to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("<div class=\"text\">I will inspect it.</div>"));
        assert!(html.contains("<summary>Tool call: run_terminal_command_v2</summary>"));
        assert!(html.contains("<summary>Tool result: run_terminal_command_v2</summary>"));
        assert!(html.contains("<div class=\"text\">The answer is here.</div>"));
    }
}
