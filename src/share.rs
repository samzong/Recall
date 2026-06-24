use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};
use serde_json::Value;

use crate::config::{AppConfig, ShareConfig};
use crate::types::{Message, Role, Session, SessionUsageEventRecord};
use crate::utils;

const PROVIDER_CLOUDFLARE_PAGES: &str = "cloudflare-pages";
const PAGES_PROJECT_NAME_FIELD: &str = "Project Name";
const PAGES_PROJECT_DOMAINS_FIELD: &str = "Project Domains";
const MAX_PAGES_ASSET_BYTES: usize = 25 * 1024 * 1024;
const HEADERS: &str = "/*\n  X-Robots-Tag: noindex, nofollow\n  X-Frame-Options: DENY\n  X-Content-Type-Options: nosniff\n  Referrer-Policy: no-referrer\n  Cache-Control: no-store\n";
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
:root{
  --page-bg:#FAF9F6;--surface:#FFFFFF;--user-surface:#FFFFFF;
  --text-primary:#23211C;--text-secondary:#6C685F;--text-tertiary:#9A958A;
  --accent:#3C4FA0;--accent-soft:rgba(60,79,160,.09);
  --rule:rgba(35,33,28,.10);--rule-strong:rgba(35,33,28,.16);
  --code-bg:#F4F2EC;--tool-bg:rgba(35,33,28,.028);
  --read-width:716px;--layout-width:1100px;
  --font-serif:"Newsreader",Georgia,"Times New Roman","Songti SC","STSong","Source Han Serif SC","Noto Serif CJK SC",SimSun,serif;
  --font-sans:-apple-system,BlinkMacSystemFont,"Segoe UI","Helvetica Neue","PingFang SC","Hiragino Sans GB","Microsoft YaHei","Noto Sans CJK SC",sans-serif;
  --font-mono:"JetBrains Mono","SF Mono",Menlo,"PingFang SC","Microsoft YaHei",monospace;
}
*,*::before,*::after{box-sizing:border-box}
html{-webkit-text-size-adjust:100%;scroll-behavior:smooth}
body{margin:0;background:var(--page-bg);color:var(--text-primary);font:16px/1.6 var(--font-sans);-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility}

.site-header{position:sticky;top:0;z-index:10;backdrop-filter:saturate(140%) blur(16px);-webkit-backdrop-filter:saturate(140%) blur(16px);background:rgba(250,249,246,.82);border-bottom:1px solid var(--rule)}
.site-header-inner{max-width:var(--layout-width);margin:0 auto;padding:18px 32px 16px}
.site-header h1{margin:0;max-width:var(--read-width);font:600 29px/1.22 var(--font-serif);letter-spacing:-.01em;color:var(--text-primary)}
.meta{display:flex;flex-wrap:wrap;align-items:center;gap:8px 14px;margin:13px 0 2px;color:var(--text-secondary);font-size:13px}
.meta-item{display:inline-flex;align-items:center;gap:6px;white-space:nowrap}
.meta-sep{width:3px;height:3px;border-radius:50%;background:var(--text-tertiary);opacity:.7}
.meta-tags{display:inline-flex;flex-wrap:wrap;gap:6px}
.meta-tag{display:inline-flex;align-items:center;gap:5px;padding:2px 9px;border:1px solid var(--rule);border-radius:999px;background:var(--surface);font:500 11.5px/1.5 var(--font-mono);color:var(--text-secondary)}
.meta-tag b{font-weight:500;color:var(--text-primary)}

.layout{display:grid;grid-template-columns:minmax(0,var(--read-width)) 212px;grid-template-areas:"page toc";justify-content:center;gap:64px;max-width:var(--layout-width);margin:0 auto;padding:40px 32px 28px}
.page{grid-area:page;min-width:0}
.document{min-width:0}

.user-toc{grid-area:toc;position:sticky;top:50vh;transform:translateY(-50%);align-self:start;max-height:80vh;display:flex;flex-direction:column;align-items:flex-end;gap:4px;padding:4px 0;z-index:5}
.user-toc-title{margin:0 3px 4px 0;font:600 10px/1 var(--font-sans);letter-spacing:.12em;text-transform:uppercase;color:var(--text-tertiary);opacity:0;transform:translateX(4px);transition:opacity .16s ease,transform .16s ease}
.user-toc:hover .user-toc-title{opacity:1;transform:none}
.toc-nav-btn{display:flex;align-items:center;justify-content:center;width:26px;height:20px;margin-right:-3px;border:0;background:none;color:var(--text-tertiary);cursor:pointer;border-radius:6px;transition:color .15s,background .15s}
.toc-nav-btn:hover{color:var(--accent);background:var(--accent-soft)}
.toc-nav-btn svg{width:13px;height:13px}
.toc-ticks{display:flex;flex-direction:column;gap:3px;width:100%;align-items:flex-end}
.tick{display:flex;align-items:center;justify-content:flex-end;gap:9px;height:22px;padding:0 3px 0 8px;text-decoration:none;border-radius:7px;color:var(--text-secondary);transition:background .14s}
.tick-label{display:flex;align-items:baseline;justify-content:flex-end;gap:9px;max-width:0;opacity:0;overflow:hidden;white-space:nowrap;transition:max-width .22s ease,opacity .16s ease}
.user-toc:hover .tick-label{max-width:165px;opacity:1}
.tick-n{flex:none;font:500 10.5px/1.5 var(--font-mono);color:var(--text-tertiary)}
.tick-t{font-size:12px;line-height:1.35;color:inherit;overflow:hidden;text-overflow:ellipsis}
.tick-line{flex:none;width:18px;height:2px;border-radius:2px;background:var(--rule-strong);transition:width .2s ease,background .2s ease}
.tick.active .tick-line{width:30px;background:var(--accent)}
.user-toc:hover .tick.active .tick-line{width:24px}
.tick:hover{background:var(--accent-soft)}
.tick:hover .tick-t,.tick:hover .tick-n{color:var(--text-primary)}
.tick.active .tick-t{color:var(--text-primary);font-weight:500}
.tick.active .tick-n{color:var(--accent)}

.turn{margin:0}
.turn.user{scroll-margin-top:128px}
.turn.user+.turn.assistant,.turn.assistant{margin-top:20px}
.turn.user:not(:first-child){margin-top:44px}
.role-label{display:inline-flex;align-items:center;gap:7px;margin:0 0 11px;font:600 11px/1 var(--font-sans);letter-spacing:.09em;text-transform:uppercase}
.role-label::before{content:"";width:14px;height:1px;background:currentColor;opacity:.5}
.turn.user .role-label{color:var(--text-tertiary)}
.turn.assistant .role-label{color:var(--accent)}

.user-block{min-width:0;background:var(--user-surface);border:1px solid var(--rule);border-left:3px solid var(--rule-strong);border-radius:4px 10px 10px 4px;padding:15px 18px;box-shadow:0 1px 2px rgba(35,33,28,.04)}
.user-block .prose{font-family:var(--font-sans);font-size:15.5px;line-height:1.62;color:var(--text-primary)}

.assistant-body{min-width:0;max-width:100%;color:var(--text-primary)}
.assistant-body>.prose+.prose{margin-top:.7em}
.assistant-body .prose{font:18px/1.72 var(--font-serif)}
.assistant-body .tool-run{margin:1.25em 0}
.prose{min-width:0;max-width:100%;overflow-wrap:anywhere;word-break:break-word}
.prose p{margin:0 0 .8em;text-wrap:pretty}
.prose p:last-child{margin-bottom:0}
.prose h1,.prose h2,.prose h3,.prose h4{font-family:var(--font-serif);font-weight:600;line-height:1.3;letter-spacing:-.01em;color:var(--text-primary)}
.prose h1{margin:1.5em 0 .5em;font-size:25px}
.prose h2{margin:1.5em 0 .5em;font-size:23px}
.prose h3{margin:1.3em 0 .4em;font-size:19px}
.prose h4{margin:1.3em 0 .4em;font-size:16px}
.prose ul,.prose ol{margin:0 0 .9em;padding-left:1.25em}
.prose li{margin:0 0 .4em}
.prose li>p{margin:.35em 0}
.prose li::marker{color:var(--text-tertiary)}
.prose strong{font-weight:600}
.prose em{font-style:italic}
.prose del{color:var(--text-secondary)}
.prose a{color:var(--accent);text-decoration:none;border-bottom:1px solid var(--accent-soft)}
.prose a:hover{border-bottom-color:var(--accent)}
.prose blockquote{margin:1em 0;padding:.2em 0 .2em 14px;border-left:3px solid var(--rule-strong);color:var(--text-secondary)}
.prose blockquote p{margin:.45em 0}
.prose hr{border:0;border-top:1px solid var(--rule);margin:1.4em 0}
.prose table{width:100%;border-collapse:collapse;margin:1em 0;font-size:14px;line-height:1.45;display:block;overflow-x:auto}
.prose th,.prose td{border:1px solid var(--rule);padding:7px 9px;text-align:left;vertical-align:top}
.prose th{background:var(--code-bg);font-weight:600}
.prose input[type="checkbox"]{width:14px;height:14px;margin:0 .45em 0 0;vertical-align:-2px}
.prose code{font:.86em/1.5 var(--font-mono);background:var(--accent-soft);border-radius:5px;padding:1.5px 5px;color:#39406b}

pre.code-block,pre.preformatted,.prose pre{margin:1.1em 0;padding:15px 17px;border:1px solid var(--rule);border-radius:10px;font:13px/1.62 var(--font-mono);overflow-x:auto;white-space:pre;word-break:normal;max-width:100%;color:var(--text-primary)}
pre.code-block{background:var(--code-bg)}
pre.code-block code,.prose pre code{background:transparent;border-radius:0;padding:0;color:inherit;font:inherit}
pre.preformatted{background:var(--surface);color:var(--text-secondary)}

.tool-run{margin:1.25em 0;padding:6px 4px 6px 16px;border-left:2px solid var(--rule)}
.tool-run .tool-group{border-left:0;padding:0;margin:0}
.tool-group{margin:0;color:var(--text-secondary);font-size:13px;line-height:1.5}
.tool-group>summary{cursor:pointer;list-style:none;display:flex;align-items:center;gap:8px;padding:3px 0;font:500 12.5px/1.4 var(--font-sans);color:var(--text-secondary)}
.tool-group>summary::-webkit-details-marker{display:none}
.tool-group>summary .chev,.log summary .chev{flex:none;width:14px;height:14px;color:var(--text-tertiary);transition:transform .15s}
.tool-group[open]>summary .chev,.log[open] summary .chev{transform:rotate(90deg)}
.tool-group>summary .count{margin-left:2px;padding:1px 7px;border-radius:999px;background:var(--accent-soft);font:500 11px/1.5 var(--font-mono);color:var(--accent)}
.tool-group-items{margin:7px 0 0;padding:0 0 0 6px;display:flex;flex-direction:column;gap:2px}

.log{margin:.6em 0;color:var(--text-secondary);font-size:13px;line-height:1.5}
.tool-run .log,.tool-group-items .log{margin:0}
.log summary{cursor:pointer;list-style:none;display:flex;align-items:center;gap:8px;padding:5px 9px;border-radius:7px;font:500 12.5px/1.4 var(--font-sans);color:var(--text-secondary);transition:background .12s}
.log summary:hover{background:var(--tool-bg)}
.log summary::-webkit-details-marker{display:none}
.log summary .badge{flex:none;font:500 10px/1.5 var(--font-mono);letter-spacing:.04em;text-transform:uppercase;padding:1px 7px;border-radius:5px;background:var(--tool-bg);color:var(--text-tertiary)}
.log summary .lname{font-family:var(--font-mono);font-size:12px;color:var(--text-primary)}
.log[open] summary{color:var(--text-primary)}
.log pre{margin:6px 0 4px;padding:11px 13px;background:var(--tool-bg);border:1px solid var(--rule);border-radius:8px;color:var(--text-secondary);font:12px/1.55 var(--font-mono);white-space:pre-wrap;word-break:break-word;overflow-x:auto}

.empty{margin:0;color:var(--text-secondary);font:18px/1.65 var(--font-serif)}

.site-footer{max-width:var(--layout-width);margin:0 auto;padding:22px 32px 60px}
.site-footer-inner{max-width:var(--read-width);margin-left:auto;margin-right:auto;display:flex;align-items:center;justify-content:center;gap:10px;padding-top:22px;border-top:1px solid var(--rule);color:var(--text-tertiary);font-size:12.5px}
.site-footer .brand-dot{width:8px;height:8px;border-radius:50%;background:var(--accent);box-shadow:0 0 0 3px var(--accent-soft)}
.site-footer b{font-weight:600;color:var(--text-secondary)}

@media (max-width:980px){
  .site-header-inner{padding:16px 20px 14px}
  .layout{display:block;padding:28px 20px 24px}
  .user-toc{display:none}
  .site-footer{padding:18px 20px 56px}
  .assistant-body .prose{font-size:17px}
}
"#;

const CHEVRON_SVG: &str = "<svg class=\"chev\" viewBox=\"0 0 16 16\" fill=\"none\" aria-hidden=\"true\"><path d=\"M6 4l4 4-4 4\" stroke=\"currentColor\" stroke-width=\"1.5\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/></svg>";

const TOC_NAV_SCRIPT: &str = r#"<script>
(function(){
  function setup(){
    var sections = Array.prototype.slice.call(document.querySelectorAll('.turn.user'));
    var ticks = Array.prototype.slice.call(document.querySelectorAll('.tick'));
    if(!sections.length || !ticks.length) return;
    var tickFor = {};
    ticks.forEach(function(t){ var h=t.getAttribute('href'); if(h) tickFor[h.slice(1)]=t; });
    var activeId = sections[0].id, offset = 150;
    function compute(){
      var cur = sections[0].id;
      for(var i=0;i<sections.length;i++){
        if(sections[i].getBoundingClientRect().top - offset <= 0) cur = sections[i].id; else break;
      }
      activeId = cur;
      ticks.forEach(function(t){ t.classList.remove('active'); });
      if(tickFor[cur]) tickFor[cur].classList.add('active');
    }
    var ticking = false;
    window.addEventListener('scroll', function(){
      if(ticking) return; ticking = true;
      requestAnimationFrame(function(){ compute(); ticking = false; });
    }, { passive:true });
    compute();
    function go(dir){
      var idx = sections.map(function(s){return s.id;}).indexOf(activeId);
      var n = Math.max(0, Math.min(sections.length-1, idx+dir));
      var top = sections[n].getBoundingClientRect().top + window.scrollY - 120;
      window.scrollTo({ top: top, behavior: 'smooth' });
    }
    var up = document.querySelector('.toc-up'), down = document.querySelector('.toc-down');
    if(up) up.addEventListener('click', function(){ go(-1); });
    if(down) down.addEventListener('click', function(){ go(1); });
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', setup);
  else setup();
})();
</script>"#;

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
    out.push_str("<link rel=\"icon\" href=\"data:,\">");
    out.push_str("<link rel=\"preconnect\" href=\"https://fonts.googleapis.com\">");
    out.push_str("<link rel=\"preconnect\" href=\"https://fonts.gstatic.com\" crossorigin>");
    out.push_str("<link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2?family=Newsreader:opsz,wght@6..72,400;6..72,500;6..72,600&family=JetBrains+Mono:wght@400;500&display=swap\">");
    out.push_str("<title>");
    out.push_str(&escape_html(&display_title));
    out.push_str("</title><style>");
    out.push_str(SESSION_PAGE_CSS);
    out.push_str("</style></head><body>");
    out.push_str("<header class=\"site-header\"><div class=\"site-header-inner\"><h1>");
    out.push_str(&escape_html(&display_title));
    out.push_str("</h1><div class=\"meta\"><span class=\"meta-item\">");
    out.push_str(&escape_html(&format_source_label(&session.source)));
    out.push_str("</span><span class=\"meta-sep\"></span><span class=\"meta-item\">");
    out.push_str(&escape_html(&format_started_at(session.started_at)));
    out.push_str("</span><span class=\"meta-sep\"></span><span class=\"meta-item\">");
    out.push_str(&messages.len().to_string());
    out.push_str(" messages</span>");
    append_header_display_meta(&mut out, display_meta);
    out.push_str("</div></div></header>");
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
    out.push_str("</div>");
    out.push_str("<footer class=\"site-footer\"><div class=\"site-footer-inner\">");
    out.push_str("<span class=\"brand-dot\"></span><span>Published with <b>Recall</b></span>");
    out.push_str("</div></footer>");
    out.push_str(TOC_NAV_SCRIPT);
    out.push_str("</body></html>");
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
            Role::User if !pending_tools.is_empty() => {
                pending_tools.push(message.content.clone());
            }
            Role::User => {
                blocks.push(RenderBlock::User(message.content.clone()));
            }
            Role::Assistant if is_tool_message(&message.content) => {
                pending_tools.push(message.content.clone());
            }
            Role::Assistant => {
                attach_tools(&mut blocks, &mut pending_tools);
                if let Some(RenderBlock::Assistant(segments)) = blocks.last_mut() {
                    append_assistant_text_segment(segments, message.content.clone());
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

fn append_assistant_text_segment(segments: &mut Vec<AssistantSegment>, content: String) {
    if let Some(AssistantSegment::Text(previous)) = segments.last_mut() {
        let previous_core = assistant_text_core(previous);
        let content_core = assistant_text_core(&content);
        if !previous_core.is_empty() && previous_core == content_core {
            if content.contains("<oai-mem-citation>") && !previous.contains("<oai-mem-citation>") {
                *previous = content;
            }
            return;
        }
    }
    segments.push(AssistantSegment::Text(content));
}

fn assistant_text_core(text: &str) -> &str {
    text.split("<oai-mem-citation>").next().unwrap_or(text).trim()
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
    out.push_str(
        "<aside class=\"user-toc\" aria-label=\"Questions in this conversation\"><p class=\"user-toc-title\">Questions</p>",
    );
    out.push_str(
        "<button type=\"button\" class=\"toc-nav-btn toc-up\" aria-label=\"Previous question\"><svg viewBox=\"0 0 16 16\" fill=\"none\" aria-hidden=\"true\"><path d=\"M4 10l4-4 4 4\" stroke=\"currentColor\" stroke-width=\"1.6\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/></svg></button>",
    );
    out.push_str("<nav class=\"toc-ticks\">");
    for (index, label) in entries {
        out.push_str("<a class=\"tick\" href=\"#user-");
        out.push_str(&index.to_string());
        out.push_str("\"><span class=\"tick-label\"><span class=\"tick-n\">");
        out.push_str(&format!("{index:02}"));
        out.push_str("</span><span class=\"tick-t\">");
        out.push_str(&escape_html(label));
        out.push_str("</span></span><span class=\"tick-line\"></span></a>");
    }
    out.push_str("</nav>");
    out.push_str(
        "<button type=\"button\" class=\"toc-nav-btn toc-down\" aria-label=\"Next question\"><svg viewBox=\"0 0 16 16\" fill=\"none\" aria-hidden=\"true\"><path d=\"M4 6l4 4 4-4\" stroke=\"currentColor\" stroke-width=\"1.6\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/></svg></button>",
    );
    out.push_str("</aside>");
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

    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0usize;
    while index < lines.len() {
        let sanitized = utils::sanitize_line(lines[index]);
        if is_oai_mem_citation_start(&sanitized) {
            if !prose.trim().is_empty() {
                render_markdown_text(out, &prose);
                prose.clear();
            }
            let (citation, next_index) = collect_oai_mem_citation(&lines, index);
            pending_logs.push(citation);
            rendered = true;
            index = next_index;
            continue;
        }
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
        index += 1;
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

fn is_oai_mem_citation_start(line: &str) -> bool {
    line.trim_start().starts_with("<oai-mem-citation>")
}

fn collect_oai_mem_citation(lines: &[&str], start: usize) -> (String, usize) {
    let mut block = Vec::new();
    let mut index = start;
    while index < lines.len() {
        let sanitized = utils::sanitize_line(lines[index]);
        let is_end = sanitized.trim_end().ends_with("</oai-mem-citation>");
        block.push(sanitized);
        index += 1;
        if is_end {
            break;
        }
    }
    (block.join("\n"), index)
}

fn render_markdown_text(out: &mut String, text: &str) {
    for fragment in split_markdown_fragments(text.trim()) {
        match fragment {
            MarkdownFragment::Markdown(markdown) => render_markdown_blocks(out, &markdown),
            MarkdownFragment::Preformatted(lines) => render_preformatted_block(out, &lines),
        }
    }
}

fn render_markdown_blocks(out: &mut String, text: &str) {
    let mut events = Vec::new();
    let mut unsafe_link_depth = 0usize;
    let mut dropped_image_depth = 0usize;
    let mut code_block: Option<Option<String>> = None;
    let mut code = String::new();
    for event in Parser::new_ext(text, markdown_options()) {
        if let Some(language) = code_block.as_ref() {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    events.push(trusted_html_event(render_code_block_html(
                        &code,
                        language.as_deref(),
                    )));
                    code.clear();
                    code_block = None;
                }
                Event::Text(value)
                | Event::Code(value)
                | Event::Html(value)
                | Event::InlineHtml(value)
                | Event::InlineMath(value)
                | Event::DisplayMath(value) => code.push_str(&value),
                Event::SoftBreak | Event::HardBreak => code.push('\n'),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                code_block = Some(code_block_language(kind));
            }
            Event::Start(Tag::Link { dest_url, .. }) if !is_safe_markdown_link(&dest_url) => {
                unsafe_link_depth += 1;
            }
            Event::End(TagEnd::Link) if unsafe_link_depth > 0 => {
                unsafe_link_depth -= 1;
            }
            Event::Start(Tag::Image { .. }) => {
                dropped_image_depth += 1;
            }
            Event::End(TagEnd::Image) if dropped_image_depth > 0 => {
                dropped_image_depth -= 1;
            }
            Event::Html(raw) | Event::InlineHtml(raw) => {
                events.push(Event::Text(raw.into_static()));
            }
            other => events.push(other.into_static()),
        }
    }
    if let Some(language) = code_block.take() {
        events.push(trusted_html_event(render_code_block_html(&code, language.as_deref())));
    }
    html::push_html(out, events.into_iter());
}

fn markdown_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_GFM);
    options
}

fn trusted_html_event(value: String) -> Event<'static> {
    Event::Html(CowStr::Boxed(value.into_boxed_str()))
}

fn code_block_language(kind: CodeBlockKind<'_>) -> Option<String> {
    match kind {
        CodeBlockKind::Indented => None,
        CodeBlockKind::Fenced(info) => {
            let language = info.split_whitespace().next().unwrap_or("").trim();
            if is_fence_language_tag(language) { Some(language.to_string()) } else { None }
        }
    }
}

fn render_code_block_html(text: &str, language: Option<&str>) -> String {
    let code = dedent_code_block(text);
    let mut out = String::new();
    out.push_str("<pre class=\"code-block\"><code");
    if let Some(language) = language {
        out.push_str(" class=\"language-");
        out.push_str(&escape_html(language));
        out.push('"');
    }
    out.push('>');
    out.push_str(&escape_html(&code));
    out.push_str("</code></pre>");
    out
}

fn is_safe_markdown_link(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
    {
        return true;
    }
    let Some((scheme, _)) = trimmed.split_once(':') else {
        return true;
    };
    matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "mailto")
}

fn split_markdown_fragments(text: &str) -> Vec<MarkdownFragment> {
    let lines: Vec<&str> = text.lines().collect();
    let mut fragments = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        if is_preformatted_line(lines[index]) {
            let start = index;
            while index < lines.len() && is_preformatted_line(lines[index]) {
                index += 1;
            }
            fragments.push(MarkdownFragment::Preformatted(
                lines[start..index].iter().map(|line| (*line).to_string()).collect(),
            ));
            continue;
        }
        let start = index;
        while index < lines.len() && !is_preformatted_line(lines[index]) {
            index += 1;
        }
        let markdown = lines[start..index].join("\n");
        if !markdown.trim().is_empty() {
            fragments.push(MarkdownFragment::Markdown(markdown));
        }
    }
    if fragments.is_empty() && !text.is_empty() {
        fragments.push(MarkdownFragment::Markdown(text.to_string()));
    }
    fragments
}

enum MarkdownFragment {
    Markdown(String),
    Preformatted(Vec<String>),
}

fn render_preformatted_block(out: &mut String, lines: &[String]) {
    out.push_str("<pre class=\"preformatted\">");
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&escape_html(line.trim_end()));
    }
    out.push_str("</pre>");
}

fn is_preformatted_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed
        .chars()
        .any(|ch| matches!(ch, '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' | '│' | '─'))
}

fn dedent_code_block(text: &str) -> String {
    let lines: Vec<&str> = text.trim_matches('\n').lines().collect();
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().take_while(|ch| *ch == ' ' || *ch == '\t').count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| line.chars().skip(indent).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_fence_language_tag(line: &str) -> bool {
    !line.is_empty()
        && line.len() <= 32
        && line.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '+')
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
    out.push_str(CHEVRON_SVG);
    out.push_str(&escape_html(&format!("{} tool executions", logs.len())));
    out.push_str("<span class=\"count\">");
    out.push_str(&logs.len().to_string());
    out.push_str("</span></summary><div class=\"tool-group-items\">");
    for log in logs {
        render_log_segment(out, log);
    }
    out.push_str("</div></details>");
}

fn render_log_segment(out: &mut String, text: &str) {
    let (badge, name) = log_badge_and_name(text);
    out.push_str("<details class=\"log\"><summary>");
    out.push_str(CHEVRON_SVG);
    out.push_str("<span class=\"badge\">");
    out.push_str(&escape_html(badge));
    out.push_str("</span><span class=\"lname\">");
    out.push_str(&escape_html(&name));
    out.push_str("</span></summary><pre>");
    out.push_str(&escape_html(text));
    out.push_str("</pre></details>");
}

fn log_badge_and_name(text: &str) -> (&'static str, String) {
    let summary = log_summary(text);
    if let Some(rest) = summary.strip_prefix("Tool call: ") {
        return ("tool", rest.to_string());
    }
    if let Some(rest) = summary.strip_prefix("Tool result: ") {
        return ("result", rest.to_string());
    }
    if let Some(rest) = summary.strip_prefix("Tool use: ") {
        return ("tool", rest.to_string());
    }
    if let Some(name) = summary.strip_suffix(" result") {
        return ("result", name.to_string());
    }
    if summary == "Citation" {
        return ("citation", summary);
    }
    if summary == "System log" {
        return ("system", summary);
    }
    ("log", summary)
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
    if display_meta.models.is_empty() && display_meta.thinking_depths.is_empty() {
        return;
    }
    out.push_str("<span class=\"meta-tags\">");
    if !display_meta.models.is_empty() {
        out.push_str("<span class=\"meta-tag\">model <b>");
        out.push_str(&escape_html(&display_meta.models.join(", ")));
        out.push_str("</b></span>");
    }
    if !display_meta.thinking_depths.is_empty() {
        out.push_str("<span class=\"meta-tag\">thinking <b>");
        out.push_str(&escape_html(&display_meta.thinking_depths.join(", ")));
        out.push_str("</b></span>");
    }
    out.push_str("</span>");
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
        assert!(html.contains("<span class=\"badge\">tool</span>"));
        assert!(html.contains("<span class=\"badge\">result</span>"));
        assert_eq!(html.matches("<span class=\"lname\">run_terminal_command_v2</span>").count(), 2);
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
        assert!(html.contains("--read-width:716px"));
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
    fn html_renderer_dedents_fenced_code_blocks() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::Assistant,
                content: "Example:\n```yaml\n     skill:\n       root: skills/mosoo\n```"
                    .to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(
            html.contains(
                "<pre class=\"code-block\"><code class=\"language-yaml\">skill:\n  root: skills/mosoo</code></pre>"
            )
        );
    }

    #[test]
    fn html_renderer_uses_real_markdown_for_agent_replies() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::Assistant,
                content:
                    "## Result\n\n1. First\n   - nested **bold**\n   - task\n\n> quoted note\n\n| Name | Value |\n| --- | --- |\n| one | `two` |\n\n~~old~~"
                        .to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("<h2>Result</h2>"));
        assert!(html.contains("<ol>"));
        assert!(html.contains("<ul>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<blockquote>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("<td><code>two</code></td>"));
        assert!(html.contains("<del>old</del>"));
    }

    #[test]
    fn html_renderer_escapes_raw_html_and_filters_unsafe_links() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::Assistant,
                content:
                    "Inline <span>html</span> and [bad](javascript:alert(1)) plus [good](https://example.com).\n\n![alt](https://example.com/image.png)"
                        .to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert!(html.contains("&lt;span&gt;html&lt;/span&gt;"));
        assert!(!html.contains("javascript:"));
        assert!(html.contains("<a href=\"https://example.com\">good</a>"));
        assert!(!html.contains("<img"));
        assert!(html.contains("alt"));
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
        assert!(html.contains("<span class=\"badge\">citation</span>"));
        assert!(html.contains("<span class=\"lname\">Citation</span>"));
        assert!(html.contains("<p>Here is the answer.</p>"));
        assert!(html.contains("<p>Done.</p>"));
    }

    #[test]
    fn html_renderer_collapses_multiline_memory_citation() {
        let html = render_html(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::Assistant,
                content:
                    "Answer.\n<oai-mem-citation>\n<citation_entries>\nMEMORY.md:1-2|note=[used]\n</citation_entries>\n</oai-mem-citation>\nNext."
                        .to_string(),
                timestamp: None,
                seq: 0,
            }],
        );
        assert_eq!(html.matches("<span class=\"lname\">Citation</span>").count(), 1);
        assert!(!html.contains("2 tool executions"));
        assert!(html.contains("<p>Answer.</p>"));
        assert!(html.contains("<p>Next.</p>"));
    }

    #[test]
    fn html_renderer_replaces_duplicate_final_with_cited_version() {
        let html = render_html(
            &session("s1"),
            &[
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "Final answer.".to_string(),
                    timestamp: None,
                    seq: 0,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content:
                        "Final answer.\n<oai-mem-citation>\n<citation_entries>\nMEMORY.md:1-2|note=[used]\n</citation_entries>\n</oai-mem-citation>"
                            .to_string(),
                    timestamp: None,
                    seq: 1,
                },
            ],
        );
        assert_eq!(html.matches("Final answer.").count(), 1);
        assert!(html.contains("<span class=\"lname\">Citation</span>"));
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
        assert!(html.contains("<span class=\"lname\">Read</span>"));
        assert!(html.contains("<span class=\"lname\">Glob</span>"));
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
        assert!(html.contains("--accent:#3C4FA0"));
        assert!(html.contains("<span class=\"tick-t\">First question</span>"));
        assert!(html.contains("<span class=\"tick-t\">Second question</span>"));
    }

    #[test]
    fn html_renderer_treats_user_tool_results_as_logs_not_turns() {
        let html = render_html(
            &session("s1"),
            &[
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::User,
                    content: "Read the config file.".to_string(),
                    timestamp: None,
                    seq: 0,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "[Read] {\"path\":\"config.toml\"}".to_string(),
                    timestamp: None,
                    seq: 1,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::User,
                    content: "{\"method\":\"get_file\",\"content\":\"secret body\"}".to_string(),
                    timestamp: None,
                    seq: 2,
                },
                Message {
                    session_id: "local-id".to_string(),
                    role: Role::Assistant,
                    content: "Here is what the config says.".to_string(),
                    timestamp: None,
                    seq: 3,
                },
            ],
        );
        assert_eq!(html.matches("class=\"turn user\"").count(), 1);
        assert!(html.contains("<span class=\"tick-t\">Read the config file.</span>"));
        assert!(!html.contains("<span class=\"tick-t\">{&quot;method&quot;"));
        assert!(html.contains("2 tool executions"));
        assert!(html.contains("secret body"));
        assert!(html.contains("<p>Here is what the config says.</p>"));
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
        assert!(html.contains("<div class=\"meta\">"));
        assert!(html.contains("0 messages</span>"));
        assert!(
            html.contains("<span class=\"meta-tag\">model <b>grok-composer-2.5-fast</b></span>")
        );
        assert!(html.contains("<span class=\"meta-tag\">thinking <b>high</b></span>"));
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
