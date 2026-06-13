use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::config::{AppConfig, ShareConfig};
use crate::types::{Message, Role, Session, SessionUsageEventRecord};
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

const SESSION_PAGE_CSS: &str = r#"
:root {
  --page-bg: #F5F5F7;
  --content-bg: #FFFFFF;
  --text-primary: #1D1D1F;
  --text-secondary: #86868B;
  --user-block-bg: #FFF3D6;
  --user-block-border: rgba(255, 149, 0, 0.22);
  --user-block-accent: #FF9500;
  --log-border: #E5E5EA;
  --layout-width: 1040px;
  --code-bg: #1E1E1E;
  --code-text: #F5F5F7;
  --border-radius: 12px;
  --read-width: 700px;
  --font-system: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Helvetica Neue", sans-serif;
  --font-mono: "SF Mono", Menlo, monospace;
}
*,*::before,*::after{box-sizing:border-box}
body{margin:0;overflow-x:hidden;background:var(--page-bg);color:var(--text-primary);font:15px/1.65 var(--font-system);-webkit-font-smoothing:antialiased}
.site-header{position:sticky;top:0;z-index:10;backdrop-filter:blur(16px);-webkit-backdrop-filter:blur(16px);background:rgba(255,255,255,.72);border-bottom:1px solid rgba(0,0,0,.05)}
.site-header-inner{max-width:var(--layout-width);margin:0 auto;padding:22px 24px 18px}
.site-header h1{margin:0 0 6px;font-size:22px;font-weight:600;line-height:1.3;letter-spacing:-.02em;color:var(--text-primary)}
.meta{margin:0;color:var(--text-secondary);font-size:13px;line-height:1.5}
.layout{display:flex;align-items:flex-start;justify-content:center;gap:36px;max-width:var(--layout-width);margin:0 auto;padding:28px 24px 64px}
.page{flex:0 1 var(--read-width);min-width:0;padding:0;margin:0}
.document{min-width:0;background:var(--content-bg);border-radius:var(--border-radius);padding:40px 36px 52px}
.user-toc{flex:0 0 220px;position:sticky;top:88px;max-height:calc(100vh - 104px);overflow-y:auto;padding:4px 0 12px}
.user-toc-title{margin:0 0 12px;font-size:11px;font-weight:600;letter-spacing:.06em;text-transform:uppercase;color:var(--text-secondary)}
.user-toc nav{display:flex;flex-direction:column;gap:2px}
.user-toc a{display:block;padding:7px 10px;border-left:2px solid transparent;border-radius:0 8px 8px 0;color:var(--text-secondary);font-size:12px;line-height:1.45;text-decoration:none;transition:color .15s ease,background .15s ease,border-color .15s ease}
.user-toc a:hover{color:var(--text-primary);background:rgba(0,0,0,.03);border-left-color:var(--user-block-accent)}
.turn{margin:0}
.turn.user{scroll-margin-top:96px}
.role-label{display:block;margin:0 0 10px;font-size:12px;font-weight:500;letter-spacing:.02em;color:var(--text-secondary)}
.turn.user .role-label{color:#B25000}
.turn.assistant .role-label{color:#5856D6}
.turn.user:not(:first-child){margin-top:48px;padding-top:36px;border-top:1px solid var(--log-border)}
.turn.assistant{margin-top:24px}
.turn.user+.turn.assistant{margin-top:20px}
.user-block{min-width:0;max-width:100%;background:var(--user-block-bg);border-radius:var(--border-radius);padding:16px 20px;border:1px solid var(--user-block-border);box-shadow:inset 3px 0 0 var(--user-block-accent)}
.assistant-body{min-width:0;max-width:100%;color:var(--text-primary);font-size:16px}
.prose{min-width:0;max-width:100%;overflow-wrap:anywhere;word-break:break-word}
.assistant-body .tool-run{margin:1.4em 0}
.tool-group{margin:0;color:var(--text-secondary);font-size:13px;line-height:1.5}
.tool-group>summary{cursor:pointer;list-style:none;color:var(--text-secondary);padding:2px 0}
.tool-group>summary::-webkit-details-marker{display:none}
.tool-group>summary::before{content:"▸ ";display:inline-block;transition:transform .15s ease}
.tool-group[open]>summary::before{transform:rotate(90deg)}
.tool-group-items{margin:8px 0 0;padding:0 0 0 8px}
.prose p{margin:0 0 1.2em}
.prose p:last-child{margin-bottom:0}
.prose h2,.prose h3{margin:1.4em 0 .6em;font-weight:600;line-height:1.35;letter-spacing:-.02em;color:var(--text-primary)}
.prose h2{font-size:19px}
.prose h3{font-size:17px}
.prose ul{margin:0 0 1.2em;padding-left:1.4em}
.prose li{margin:0 0 .45em}
.prose strong{font-weight:600}
.prose code{font:13px/1.5 var(--font-mono);background:var(--user-block-bg);border-radius:4px;padding:2px 6px;color:var(--text-primary)}
pre.code-block,pre.preformatted{margin:1.2em 0;padding:16px;border-radius:8px;font:13px/1.55 var(--font-mono);overflow-x:auto;white-space:pre;word-break:normal;max-width:100%}
pre.code-block{background:var(--code-bg);color:var(--code-text)}
pre.preformatted{background:rgba(0,0,0,.04);color:var(--text-primary)}
.tool-run{margin:1.2em 0;padding:10px 0 10px 14px;border-left:3px solid var(--log-border)}
.tool-run .tool-group{border-left:0;padding:0;margin:0}
.tool-run .log,.tool-group-items .log{margin:0 0 6px;padding:0 0 0 10px;border-left:2px solid var(--log-border)}
.tool-run .log:last-child,.tool-group-items .log:last-child{margin-bottom:0}
.empty{margin:0;color:var(--text-secondary);font-size:15px;line-height:1.65}
@media (max-width:960px){.layout{display:block;padding:28px 20px 64px}.user-toc{display:none}}
.log{margin:1em 0;padding:0 0 0 14px;border-left:3px solid var(--log-border);color:var(--text-secondary);font-size:13px;line-height:1.5}
.log summary{cursor:pointer;list-style:none;color:var(--text-secondary)}
.log summary::-webkit-details-marker{display:none}
.log summary::before{content:"▸ ";display:inline-block;transition:transform .15s ease}
.log[open] summary::before{transform:rotate(90deg)}
.log pre{margin:8px 0 0;padding:0;background:transparent;border:0;color:var(--text-secondary);font:13px/1.5 var(--font-mono);white-space:pre-wrap;word-break:break-word}
"#;

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

pub fn default_preview_dir() -> Result<PathBuf> {
    let root = dirs::cache_dir()
        .or_else(dirs::data_local_dir)
        .or_else(dirs::data_dir)
        .ok_or_else(|| anyhow!("cannot determine cache directory"))?;
    Ok(root.join("recall").join("preview"))
}

#[derive(Debug, Default, Clone)]
pub struct SessionDisplayMeta {
    pub models: Vec<String>,
    pub thinking_depths: Vec<String>,
}

pub fn collect_session_display_meta(
    session: &Session,
    usage_events: &[SessionUsageEventRecord],
) -> SessionDisplayMeta {
    let mut meta = SessionDisplayMeta::default();
    enrich_display_meta_from_usage(&mut meta, usage_events);
    if let Err(err) = enrich_display_meta_from_source(session, &mut meta) {
        tracing::debug!("session display meta source enrichment skipped: {err}");
    }
    meta
}

pub fn write_preview_file(
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

pub fn open_path_in_browser(path: &Path) -> Result<()> {
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

pub fn open_session_preview(
    session: &Session,
    messages: &[Message],
    usage_events: &[SessionUsageEventRecord],
) -> Result<PathBuf> {
    let path = write_preview_file(session, messages, usage_events)?;
    open_path_in_browser(&path)?;
    Ok(path)
}

pub fn preview_session(
    config: &AppConfig,
    session: &Session,
    messages: &[Message],
    usage_events: &[SessionUsageEventRecord],
) -> Result<SharePreview> {
    let (preview, _) = build_publish_preview(config, session, messages, usage_events)?;
    Ok(preview)
}

pub fn publish_session(
    config: &AppConfig,
    session: &Session,
    messages: &[Message],
    usage_events: &[SessionUsageEventRecord],
) -> Result<String> {
    let (preview, html) = build_publish_preview(config, session, messages, usage_events)?;

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
    let html = render_session_html(session, messages, &display_meta);
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

pub fn render_session_html(
    session: &Session,
    messages: &[Message],
    display_meta: &SessionDisplayMeta,
) -> String {
    let title = session.custom_title.as_deref().unwrap_or(&session.title);
    let display_title = display_title(title);
    let blocks = prepare_render_blocks(messages);
    let user_toc = collect_user_toc(&blocks);
    let mut out = String::new();
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    out.push_str("<meta name=\"robots\" content=\"noindex,nofollow\">");
    out.push_str("<title>");
    out.push_str(&escape_html(&display_title));
    out.push_str("</title><style>");
    out.push_str(SESSION_PAGE_CSS);
    out.push_str("</style></head><body>");
    out.push_str("<header class=\"site-header\"><div class=\"site-header-inner\"><h1>");
    out.push_str(&escape_html(&display_title));
    out.push_str("</h1><p class=\"meta\">");
    out.push_str(&escape_html(&format_source_label(&session.source)));
    out.push_str(" · ");
    out.push_str(&escape_html(&format_started_at(session.started_at)));
    out.push_str(" · ");
    out.push_str(&messages.len().to_string());
    out.push_str(" messages");
    append_header_display_meta(&mut out, display_meta);
    out.push_str("</p></div></header>");
    out.push_str("<div class=\"layout\"><div class=\"page\"><article class=\"document\">");
    if blocks.is_empty() {
        out.push_str("<p class=\"empty\">No messages in this session.</p>");
    } else {
        let mut user_index = 0usize;
        for block in blocks {
            if matches!(&block, RenderBlock::User(_)) {
                user_index += 1;
                render_block_html(&mut out, block, Some(user_index));
            } else {
                render_block_html(&mut out, block, None);
            }
        }
    }
    out.push_str("</article></div>");
    render_user_toc(&mut out, &user_toc);
    out.push_str("</div></body></html>");
    out
}

enum RenderBlock {
    User(String),
    Assistant(Vec<AssistantSegment>),
}

enum AssistantSegment {
    Text(String),
    Tools(Vec<String>),
}

fn prepare_render_blocks(messages: &[Message]) -> Vec<RenderBlock> {
    let mut blocks = Vec::new();
    let mut pending_tools = Vec::new();

    let attach_tools = |blocks: &mut Vec<RenderBlock>, pending: &mut Vec<String>| {
        if pending.is_empty() {
            return;
        }
        let tools = std::mem::take(pending);
        if let Some(RenderBlock::Assistant(segments)) = blocks.last_mut() {
            segments.push(AssistantSegment::Tools(tools));
        } else {
            blocks.push(RenderBlock::Assistant(vec![AssistantSegment::Tools(tools)]));
        }
    };

    for message in messages {
        match message.role {
            Role::User => {
                attach_tools(&mut blocks, &mut pending_tools);
                blocks.push(RenderBlock::User(message.content.clone()));
            }
            Role::Assistant if is_tool_message(&message.content) => {
                pending_tools.push(message.content.clone());
            }
            Role::Assistant => {
                attach_tools(&mut blocks, &mut pending_tools);
                if let Some(RenderBlock::Assistant(segments)) = blocks.last_mut() {
                    segments.push(AssistantSegment::Text(message.content.clone()));
                } else {
                    blocks.push(RenderBlock::Assistant(vec![AssistantSegment::Text(
                        message.content.clone(),
                    )]));
                }
            }
        }
    }
    attach_tools(&mut blocks, &mut pending_tools);
    blocks
}

fn collect_user_toc(blocks: &[RenderBlock]) -> Vec<(usize, String)> {
    let mut entries = Vec::new();
    let mut index = 0usize;
    for block in blocks {
        let RenderBlock::User(content) = block else {
            continue;
        };
        index += 1;
        entries.push((index, user_toc_label(content)));
    }
    entries
}

fn render_user_toc(out: &mut String, entries: &[(usize, String)]) {
    if entries.is_empty() {
        return;
    }
    out.push_str("<aside class=\"user-toc\"><p class=\"user-toc-title\">User messages</p><nav>");
    for (index, label) in entries {
        out.push_str("<a href=\"#user-");
        out.push_str(&index.to_string());
        out.push_str("\">");
        out.push_str(&escape_html(&format!("{index}. {label}")));
        out.push_str("</a>");
    }
    out.push_str("</nav></aside>");
}

fn render_block_html(out: &mut String, block: RenderBlock, user_index: Option<usize>) {
    match block {
        RenderBlock::User(content) => {
            let index = user_index.unwrap_or(1);
            out.push_str("<section class=\"turn user\" id=\"user-");
            out.push_str(&index.to_string());
            out.push_str("\"><span class=\"role-label\">User</span><div class=\"user-block\">");
            render_content(out, &content);
            out.push_str("</div></section>");
        }
        RenderBlock::Assistant(segments) => {
            out.push_str(
                "<section class=\"turn assistant\"><span class=\"role-label\">Assistant</span><div class=\"assistant-body\">",
            );
            for segment in segments {
                match segment {
                    AssistantSegment::Text(content) => render_content(out, &content),
                    AssistantSegment::Tools(logs) => {
                        out.push_str("<div class=\"tool-run\">");
                        render_tool_group(out, &logs);
                        out.push_str("</div>");
                    }
                }
            }
            out.push_str("</div></section>");
        }
    }
}

fn render_content(out: &mut String, text: &str) {
    out.push_str("<div class=\"prose\">");
    let mut prose = String::new();
    let mut pending_logs = Vec::new();
    let mut rendered = false;

    let flush_logs = |out: &mut String, pending: &mut Vec<String>| {
        if pending.is_empty() {
            return;
        }
        render_tool_group(out, pending);
        pending.clear();
    };

    for line in text.lines() {
        let sanitized = utils::sanitize_line(line);
        if is_log_line(&sanitized) {
            if !prose.trim().is_empty() {
                render_markdown_text(out, &prose);
                prose.clear();
            }
            pending_logs.push(sanitized);
            rendered = true;
        } else {
            flush_logs(out, &mut pending_logs);
            if !prose.is_empty() {
                prose.push('\n');
            }
            prose.push_str(&sanitized);
        }
    }
    flush_logs(out, &mut pending_logs);
    if !prose.trim().is_empty() {
        render_markdown_text(out, &prose);
        rendered = true;
    }
    if !rendered && !text.trim().is_empty() {
        render_markdown_text(out, text);
    }
    out.push_str("</div>");
}

fn render_markdown_text(out: &mut String, text: &str) {
    for fragment in split_text_fragments(text.trim()) {
        match fragment {
            TextFragment::Prose(prose) => render_markdown_blocks(out, &prose),
            TextFragment::Code(code) => render_code_block(out, &code),
        }
    }
}

fn render_markdown_blocks(out: &mut String, text: &str) {
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim_end();
        if line.trim().is_empty() {
            index += 1;
            continue;
        }
        if let Some(stripped) = line.strip_prefix("### ") {
            out.push_str("<h3>");
            render_inline_markup(out, stripped.trim());
            out.push_str("</h3>");
            index += 1;
            continue;
        }
        if let Some(stripped) = line.strip_prefix("## ") {
            out.push_str("<h2>");
            render_inline_markup(out, stripped.trim());
            out.push_str("</h2>");
            index += 1;
            continue;
        }
        if is_list_marker_line(line) {
            out.push_str("<ul>");
            while index < lines.len() && is_list_marker_line(lines[index]) {
                out.push_str("<li>");
                render_inline_markup(out, list_marker_text(lines[index]));
                out.push_str("</li>");
                index += 1;
            }
            out.push_str("</ul>");
            continue;
        }
        let mut paragraph = String::new();
        while index < lines.len() {
            let current = lines[index].trim_end();
            if current.trim().is_empty()
                || current.starts_with("### ")
                || current.starts_with("## ")
                || is_list_marker_line(current)
            {
                break;
            }
            if is_preformatted_line(current) {
                if !paragraph.is_empty() {
                    out.push_str("<p>");
                    render_inline_markup(out, paragraph.trim());
                    out.push_str("</p>");
                    paragraph.clear();
                }
                let start = index;
                while index < lines.len() && is_preformatted_line(lines[index]) {
                    index += 1;
                }
                render_preformatted_block(out, &lines[start..index]);
                continue;
            }
            if !paragraph.is_empty() {
                paragraph.push('\n');
            }
            paragraph.push_str(current);
            index += 1;
        }
        if !paragraph.is_empty() {
            out.push_str("<p>");
            render_inline_markup(out, paragraph.trim());
            out.push_str("</p>");
        }
    }
}

fn is_preformatted_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed
        .chars()
        .any(|ch| matches!(ch, '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' | '│' | '─'))
    {
        return true;
    }
    if trimmed.starts_with('|') && trimmed.contains('|') {
        return true;
    }
    trimmed.len() >= 16 && trimmed.chars().all(|ch| matches!(ch, '-' | '=' | '|' | ':' | ' ' | '+'))
}

fn render_preformatted_block(out: &mut String, lines: &[&str]) {
    out.push_str("<pre class=\"preformatted\">");
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&escape_html(line.trim_end()));
    }
    out.push_str("</pre>");
}

enum InlineToken {
    Code(usize),
    Bold(usize),
}

fn render_inline_markup(out: &mut String, text: &str) {
    let mut rest = text;
    while !rest.is_empty() {
        let tick = rest.find('`');
        let bold = rest.find("**");
        let next = match (tick, bold) {
            (Some(t), Some(b)) => {
                if t < b {
                    InlineToken::Code(t)
                } else {
                    InlineToken::Bold(b)
                }
            }
            (Some(t), None) => InlineToken::Code(t),
            (None, Some(b)) => InlineToken::Bold(b),
            (None, None) => {
                render_plain_text(out, rest);
                break;
            }
        };
        match next {
            InlineToken::Code(pos) => {
                render_plain_text(out, &rest[..pos]);
                rest = &rest[pos + 1..];
                if let Some(end) = rest.find('`') {
                    let code = &rest[..end];
                    if !code.is_empty() {
                        out.push_str("<code>");
                        out.push_str(&escape_html(code));
                        out.push_str("</code>");
                    }
                    rest = &rest[end + 1..];
                } else {
                    out.push_str(&escape_html(&format!("`{rest}")));
                    break;
                }
            }
            InlineToken::Bold(pos) => {
                render_plain_text(out, &rest[..pos]);
                rest = &rest[pos + 2..];
                if let Some(end) = rest.find("**") {
                    out.push_str("<strong>");
                    out.push_str(&escape_html(&rest[..end]));
                    out.push_str("</strong>");
                    rest = &rest[end + 2..];
                } else {
                    out.push_str("**");
                    render_plain_text(out, rest);
                    break;
                }
            }
        }
    }
}

fn render_plain_text(out: &mut String, text: &str) {
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push_str("<br>");
        }
        out.push_str(&escape_html(line));
    }
}

fn is_list_marker_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("* ") || trimmed.starts_with("- ")
}

fn list_marker_text(line: &str) -> &str {
    let trimmed = line.trim_start();
    trimmed.strip_prefix("* ").or_else(|| trimmed.strip_prefix("- ")).unwrap_or(trimmed)
}

fn render_code_block(out: &mut String, text: &str) {
    let code = strip_code_fence_language(text);
    out.push_str("<pre class=\"code-block\">");
    out.push_str(&escape_html(code.trim_end()));
    out.push_str("</pre>");
}

fn render_tool_group(out: &mut String, logs: &[String]) {
    if logs.is_empty() {
        return;
    }
    if logs.len() == 1 {
        render_log_segment(out, &logs[0]);
        return;
    }
    out.push_str("<details class=\"tool-group\"><summary>");
    out.push_str(&escape_html(&format!("{} tool executions", logs.len())));
    out.push_str("</summary><div class=\"tool-group-items\">");
    for log in logs {
        render_log_segment(out, log);
    }
    out.push_str("</div></details>");
}

fn render_log_segment(out: &mut String, text: &str) {
    out.push_str("<details class=\"log\"><summary>");
    out.push_str(&escape_html(&log_summary(text)));
    out.push_str("</summary><pre>");
    out.push_str(&escape_html(text));
    out.push_str("</pre></details>");
}

enum TextFragment {
    Prose(String),
    Code(String),
}

fn split_text_fragments(text: &str) -> Vec<TextFragment> {
    let mut fragments = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        if start > 0 {
            let prose = rest[..start].trim();
            if !prose.is_empty() {
                fragments.push(TextFragment::Prose(prose.to_string()));
            }
        }
        rest = &rest[start + 3..];
        if let Some(end) = rest.find("```") {
            fragments.push(TextFragment::Code(rest[..end].to_string()));
            rest = &rest[end + 3..];
        } else {
            fragments.push(TextFragment::Code(rest.to_string()));
            return fragments;
        }
    }
    let prose = rest.trim();
    if !prose.is_empty() {
        fragments.push(TextFragment::Prose(prose.to_string()));
    }
    if fragments.is_empty() && !text.is_empty() {
        fragments.push(TextFragment::Prose(text.to_string()));
    }
    fragments
}

fn strip_code_fence_language(text: &str) -> &str {
    if text.starts_with('\n') {
        return text.trim_start_matches('\n');
    }
    let Some(first_newline) = text.find('\n') else {
        return if is_fence_language_tag(text.trim()) { "" } else { text };
    };
    let first_line = text[..first_newline].trim();
    if is_fence_language_tag(first_line) { &text[first_newline + 1..] } else { text }
}

fn is_fence_language_tag(line: &str) -> bool {
    !line.is_empty()
        && line.len() <= 32
        && line.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '+')
}

fn is_tool_message(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }
    is_agent_tool_line(trimmed.lines().next().unwrap_or(""))
}

fn is_log_line(line: &str) -> bool {
    is_agent_tool_line(line) || is_xml_tag_line(line)
}

fn is_agent_tool_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with("[tool:")
        || trimmed.starts_with("[tool_result:")
        || trimmed.starts_with("[tool_use:")
    {
        return true;
    }
    if !trimmed.starts_with('[') {
        return false;
    }
    let Some(end) = trimmed.find(']') else {
        return false;
    };
    if end <= 1 {
        return false;
    }
    let after = trimmed[end + 1..].trim_start();
    after.is_empty() || after.starts_with('{') || after.starts_with("->")
}

fn is_xml_tag_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('<')
        && trimmed.ends_with('>')
        && trimmed.len() > 2
        && !trimmed[1..trimmed.len() - 1].contains(' ')
}

fn log_summary(text: &str) -> String {
    let trimmed = text.trim_start();
    let first_line = trimmed.lines().next().unwrap_or(trimmed);
    for (prefix, label) in
        [("[tool:", "Tool call"), ("[tool_result:", "Tool result"), ("[tool_use:", "Tool use")]
    {
        if let Some(name) = first_line
            .strip_prefix(prefix)
            .and_then(|rest| rest.split(']').next())
            .filter(|name| !name.trim().is_empty())
        {
            return format!("{label}: {name}");
        }
    }
    if let Some(name) = bracket_tool_name(first_line) {
        if first_line[name.len() + 2..].trim_start().starts_with("->") {
            return format!("{name} result");
        }
        return name.to_string();
    }
    if is_xml_tag_line(first_line) {
        if first_line.contains("oai-mem-citation") {
            return "Citation".to_string();
        }
        return "System log".to_string();
    }
    "Log".to_string()
}

fn bracket_tool_name(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') {
        return None;
    }
    let end = trimmed.find(']')?;
    if end <= 1 {
        return None;
    }
    Some(&trimmed[1..end])
}

fn append_header_display_meta(out: &mut String, display_meta: &SessionDisplayMeta) {
    if !display_meta.models.is_empty() {
        out.push_str(" · Model: ");
        out.push_str(&escape_html(&display_meta.models.join(", ")));
    }
    if !display_meta.thinking_depths.is_empty() {
        out.push_str(" · Thinking: ");
        out.push_str(&escape_html(&display_meta.thinking_depths.join(", ")));
    }
}

fn enrich_display_meta_from_usage(
    meta: &mut SessionDisplayMeta,
    usage_events: &[SessionUsageEventRecord],
) {
    for event in usage_events {
        if event.model != "unknown" {
            push_unique(&mut meta.models, &event.model);
        }
        if let Some(raw) = event.raw_usage_json.as_deref() {
            enrich_display_meta_from_json_text(meta, raw);
        }
    }
}

fn enrich_display_meta_from_source(session: &Session, meta: &mut SessionDisplayMeta) -> Result<()> {
    match session.source.as_str() {
        "grok" => enrich_display_meta_from_grok(&session.source_id, meta)?,
        "codex" => {
            if let Some(path) = session.source_file_path.as_deref() {
                enrich_display_meta_from_codex_rollout(Path::new(path), meta)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn enrich_display_meta_from_grok(source_id: &str, meta: &mut SessionDisplayMeta) -> Result<()> {
    let Some(session_dir) = resolve_grok_session_dir(source_id) else {
        return Ok(());
    };
    let summary_path = session_dir.join("summary.json");
    if let Ok(content) = fs::read_to_string(&summary_path)
        && let Ok(doc) = serde_json::from_str::<Value>(&content)
        && let Some(model) = doc.get("current_model_id").and_then(Value::as_str)
    {
        push_unique(&mut meta.models, model);
    }
    let updates_path = session_dir.join("updates.jsonl");
    if !updates_path.exists() {
        return Ok(());
    }
    let file = fs::File::open(&updates_path)
        .with_context(|| format!("failed to open {}", updates_path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if !line.contains("modelId")
            && !line.contains("thinkingDepth")
            && !line.contains("thinking_depth")
            && !line.contains("reasoningEffort")
            && !line.contains("reasoning_effort")
        {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let update = doc.pointer("/params/update").or_else(|| doc.get("update"));
        if let Some(update) = update {
            if let Some(model) = update.pointer("/_meta/modelId").and_then(Value::as_str) {
                push_unique(&mut meta.models, model);
            }
            if let Some(depth) = update.pointer("/_meta/thinkingDepth").and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
            if let Some(depth) = update.pointer("/_meta/thinking_depth").and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
            if let Some(depth) = update.pointer("/_meta/reasoningEffort").and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
            if let Some(depth) = update.pointer("/_meta/reasoning_effort").and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
        }
    }
    Ok(())
}

fn enrich_display_meta_from_codex_rollout(
    path: &Path,
    meta: &mut SessionDisplayMeta,
) -> Result<()> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if !line.contains("turn_context") {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if doc.get("type").and_then(Value::as_str) != Some("turn_context") {
            continue;
        }
        let payload = doc.get("payload").unwrap_or(&doc);
        if let Some(model) = payload.get("model").and_then(Value::as_str) {
            push_unique(&mut meta.models, model);
        }
        for key in ["effort", "reasoning_effort", "thinking_depth", "thinkingDepth"] {
            if let Some(depth) = payload.get(key).and_then(Value::as_str) {
                push_unique(&mut meta.thinking_depths, depth);
            }
        }
    }
    Ok(())
}

fn enrich_display_meta_from_json_text(meta: &mut SessionDisplayMeta, raw: &str) {
    let Ok(doc) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    enrich_display_meta_from_json_value(meta, &doc);
}

fn enrich_display_meta_from_json_value(meta: &mut SessionDisplayMeta, doc: &Value) {
    for key in ["model", "model_name", "model_id", "current_model_id"] {
        if let Some(model) = doc.get(key).and_then(Value::as_str) {
            push_unique(&mut meta.models, model);
        }
    }
    for key in ["effort", "reasoning_effort", "thinking_depth", "thinkingDepth", "reasoningEffort"]
    {
        if let Some(depth) = doc.get(key).and_then(Value::as_str) {
            push_unique(&mut meta.thinking_depths, depth);
        }
    }
    if let Some(model_info) = doc.get("model_info") {
        enrich_display_meta_from_json_value(meta, model_info);
    }
}

fn resolve_grok_session_dir(source_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let sessions_dir = home.join(".grok").join("sessions");
    let workspaces = fs::read_dir(sessions_dir).ok()?;
    for workspace in workspaces.flatten() {
        let session_dir = workspace.path().join(source_id);
        if session_dir.join("summary.json").exists() {
            return Some(session_dir);
        }
    }
    None
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() || values.iter().any(|existing| existing == trimmed) {
        return;
    }
    values.push(trimmed.to_string());
}

fn display_title(title: &str) -> String {
    truncate_display_text(&strip_simple_markdown(title.lines().next().unwrap_or(title).trim()), 100)
}

fn user_toc_label(content: &str) -> String {
    let line =
        content.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("User message");
    let clean = strip_simple_markdown(line);
    if clean.is_empty() { "User message".to_string() } else { truncate_display_text(&clean, 52) }
}

fn strip_simple_markdown(text: &str) -> String {
    text.replace("**", "").replace('`', "")
}

fn truncate_display_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn format_source_label(source: &str) -> String {
    let mut chars = source.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::new();
    out.extend(first.to_uppercase());
    out.extend(chars);
    out
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

    fn render_html(session: &Session, messages: &[Message]) -> String {
        render_session_html(session, messages, &SessionDisplayMeta::default())
    }

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
        let html = render_html(
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
        let html = render_html(&session("s1"), &[]);
        assert!(!html.contains("/tmp/project"));
    }

    #[test]
    fn html_renderer_collapses_tool_lines() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::Assistant,
                content: "I will inspect it.\n[tool:run_terminal_command_v2]\n[tool_result:run_terminal_command_v2] {\"output\":\"huge\"}\nThe answer is here.".to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("<p>I will inspect it.</p>"));
        assert!(html.contains("2 tool executions"));
        assert!(html.contains("<summary>Tool call: run_terminal_command_v2</summary>"));
        assert!(html.contains("<summary>Tool result: run_terminal_command_v2</summary>"));
        assert!(html.contains("<p>The answer is here.</p>"));
        assert!(html.contains("class=\"tool-group\""));
    }

    #[test]
    fn html_renderer_uses_reading_layout_and_code_blocks() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::User,
                content: "Run this:\n```bash\nnpm test\n```".to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("--read-width: 700px"));
        assert!(html.contains("class=\"site-header\""));
        assert!(html.contains("class=\"user-block\""));
        assert!(html.contains("<pre class=\"code-block\">"));
        assert!(html.contains("npm test"));
    }

    #[test]
    fn html_renderer_preserves_unlabeled_code_fence_content() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::User,
                content: "Example:\n```\nhello\nworld\n```".to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("hello"));
        assert!(html.contains("world"));
    }

    #[test]
    fn preview_writes_html_file() {
        let dir =
            std::env::temp_dir().join(format!("recall-preview-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = session("preview-test");
        let messages = vec![Message {
            session_id: "local-id".to_string(),
            role: Role::User,
            content: "hello".to_string(),
            timestamp: None,
            seq: 0,
        }];
        let path = dir.join("preview.html");
        let html = render_html(&session, &messages);
        std::fs::write(&path, html).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("hello"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn html_renderer_collapses_citation_lines() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::Assistant,
                content:
                    "Here is the answer.\n<oai-mem-citation>path/to/file</oai-mem-citation>\nDone."
                        .to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("<summary>Citation</summary>"));
        assert!(html.contains("<p>Here is the answer.</p>"));
        assert!(html.contains("<p>Done.</p>"));
    }

    #[test]
    fn html_renderer_batches_grok_tool_messages() {
        let html = render_html(
            &session("s1"),
            &[
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "Answer incoming.".to_string(),
                    timestamp: None,
                    seq: 0,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "[Read] {\"path\":\"src/share.rs\"}".to_string(),
                    timestamp: None,
                    seq: 1,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "[Glob] {\"glob_pattern\":\"**/*\"}".to_string(),
                    timestamp: None,
                    seq: 2,
                },
            ],
        );
        assert!(html.contains("<p>Answer incoming.</p>"));
        assert!(html.contains("class=\"tool-run\""));
        assert!(html.contains("2 tool executions"));
        assert!(html.contains("<summary>Read</summary>"));
        assert!(html.contains("<summary>Glob</summary>"));
        assert_eq!(html.matches("class=\"turn assistant\"").count(), 1);
        assert!(html.contains("role-label\">Assistant"));
        assert!(html.contains("assistant-body\"><div class=\"prose\"><p>Answer incoming.</p>"));
    }

    #[test]
    fn html_renderer_groups_user_assistant_exchanges() {
        let html = render_html(
            &session("s1"),
            &[
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::User,
                    content: "First question".to_string(),
                    timestamp: None,
                    seq: 0,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "Working on it.".to_string(),
                    timestamp: None,
                    seq: 1,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "[Read] {\"path\":\"src/share.rs\"}".to_string(),
                    timestamp: None,
                    seq: 2,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "Here is the answer.".to_string(),
                    timestamp: None,
                    seq: 3,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::User,
                    content: "Second question".to_string(),
                    timestamp: None,
                    seq: 4,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "Second answer.".to_string(),
                    timestamp: None,
                    seq: 5,
                },
            ],
        );
        assert_eq!(html.matches("class=\"turn user\"").count(), 2);
        assert_eq!(html.matches("class=\"turn assistant\"").count(), 2);
        assert!(html.contains("class=\"turn user\" id=\"user-1\""));
        assert!(html.contains("turn assistant\"><span class=\"role-label\">Assistant</span>"));
        assert!(html.contains("class=\"user-block\""));
        let first_user = html.find("First question").unwrap();
        let first_answer = html.find("Here is the answer.").unwrap();
        let second_user = html.find("Second question").unwrap();
        let tool_run = html.find("class=\"tool-run\"").unwrap();
        assert!(first_user < tool_run);
        assert!(tool_run < first_answer);
        assert!(first_answer < second_user);
    }

    #[test]
    fn html_renderer_renders_user_toc_and_highlight_anchors() {
        let html = render_html(
            &session("s1"),
            &[
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::User,
                    content: "First question".to_string(),
                    timestamp: None,
                    seq: 0,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "First answer.".to_string(),
                    timestamp: None,
                    seq: 1,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::User,
                    content: "Second question".to_string(),
                    timestamp: None,
                    seq: 2,
                },
            ],
        );
        assert!(html.contains("class=\"user-toc\""));
        assert!(html.contains("href=\"#user-1\""));
        assert!(html.contains("href=\"#user-2\""));
        assert!(html.contains("id=\"user-1\""));
        assert!(html.contains("id=\"user-2\""));
        assert!(html.contains("--user-block-bg: #FFF3D6"));
        assert!(html.contains("1. First question"));
        assert!(html.contains("2. Second question"));
    }

    #[test]
    fn collect_display_meta_from_usage_and_codex_rollout() {
        let mut meta = SessionDisplayMeta::default();
        enrich_display_meta_from_usage(
            &mut meta,
            &[SessionUsageEventRecord {
                event_key: "e1".to_string(),
                event_seq: 0,
                message_seq: None,
                timestamp: 0,
                model: "gpt-5.5".to_string(),
                provider: "openai".to_string(),
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                token_source: "observed".to_string(),
                parser_version: 1,
                source_path: None,
                raw_usage_json: Some(r#"{"effort":"high"}"#.to_string()),
            }],
        );

        let dir = std::env::temp_dir().join(format!("recall-share-meta-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let rollout = dir.join("rollout.jsonl");
        fs::write(
            &rollout,
            r#"{"type":"turn_context","payload":{"model":"gpt-5-codex","effort":"medium"}}"#,
        )
        .unwrap();
        enrich_display_meta_from_codex_rollout(&rollout, &mut meta).unwrap();
        let _ = fs::remove_dir_all(dir);

        assert_eq!(meta.models, vec!["gpt-5.5", "gpt-5-codex"]);
        assert_eq!(meta.thinking_depths, vec!["high", "medium"]);
    }

    #[test]
    fn html_renderer_shows_model_and_thinking_chips() {
        let html = render_session_html(
            &session("s1"),
            &[],
            &SessionDisplayMeta {
                models: vec!["grok-composer-2.5-fast".to_string()],
                thinking_depths: vec!["high".to_string()],
            },
        );
        assert!(html.contains("<p class=\"meta\">"));
        assert!(html.contains("0 messages · Model: grok-composer-2.5-fast · Thinking: high</p>"));
    }

    #[test]
    fn html_renderer_wraps_box_drawing_tables_in_preformatted_block() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::User,
                content: "Author rejection cases:\n\n┌────────┬──────────────────────────┐\n│ Field  │ Reject when matched      │\n└────────┴──────────────────────────┘".to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("preformatted"));
        assert!(html.contains("┌────"));
        assert!(!html.contains("<p>┌"));
        assert!(html.contains("overflow-x:auto"));
        assert!(html.contains("overflow-wrap:anywhere"));
    }

    #[test]
    fn html_renderer_renders_markdown_and_keeps_inline_mentions() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::User,
                content: "**Bold title**\n\n### Section\n\n* first item\n* second item\n\nMention `<oai-mem-citation>` in prose.".to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("<strong>Bold title</strong>"));
        assert!(html.contains("<h3>Section</h3>"));
        assert!(html.contains("<li>first item</li>"));
        assert!(!html.contains("<summary>Citation</summary>"));
    }
}
