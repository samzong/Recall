use std::fs;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::{AppConfig, ShareConfig};

use super::publish::{
    configured_project_domain, deploy_pages, ensure_readable_publish_dir, expand_path,
    require_share_config, share_page_url,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ShareFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SharePage {
    pub(crate) share_id: String,
    pub(crate) url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    pub(crate) file_path: PathBuf,
    pub(crate) html_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShareInventory {
    pub(crate) provider: String,
    pub(crate) project_name: String,
    pub(crate) project_domain: String,
    pub(crate) publish_dir: PathBuf,
    pub(crate) url_base: String,
    pub(crate) shares: Vec<SharePage>,
}

pub(crate) fn run_list(format: ShareFormat) -> Result<()> {
    let inventory = list_share_pages(&AppConfig::load()?)?;
    match format {
        ShareFormat::Text => print_inventory_text(&inventory),
        ShareFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&inventory)?);
        }
    }
    Ok(())
}

pub(crate) fn run_unpublish(id: &str, dry_run: bool, yes: bool, format: ShareFormat) -> Result<()> {
    let config = AppConfig::load()?;
    let share = require_share_config(&config)?;
    let page = find_share_page(share, id)?;
    if dry_run {
        print_unpublish_result(&page, true, format)?;
        return Ok(());
    }
    confirm_unpublish(&page, yes)?;
    eprintln!("Unpublishing {}...", page.share_id);
    let unpublished = unpublish_share_page(share, &page.share_id)?;
    print_unpublish_result(&unpublished, false, format)
}

pub(crate) fn list_share_pages(config: &AppConfig) -> Result<ShareInventory> {
    let share = require_share_config(config)?;
    let project_domain = configured_project_domain(share)?;
    let publish_dir = expand_path(&share.publish_dir);
    let shares = if ensure_readable_publish_dir(&publish_dir)? {
        collect_share_pages(&publish_dir, &project_domain)?
    } else {
        Vec::new()
    };
    Ok(ShareInventory {
        provider: share.provider.clone(),
        project_name: share.project_name.clone(),
        project_domain: project_domain.clone(),
        publish_dir,
        url_base: format!("https://{project_domain}"),
        shares,
    })
}

pub(crate) fn find_share_page(share: &ShareConfig, id: &str) -> Result<SharePage> {
    let project_domain = configured_project_domain(share)?;
    let share_id = resolve_share_id(id, &project_domain)?;
    let publish_dir = expand_path(&share.publish_dir);
    if !ensure_readable_publish_dir(&publish_dir)? {
        bail!("share page not found: {share_id}");
    }
    load_share_page(&publish_dir, &project_domain, &share_id)?
        .ok_or_else(|| anyhow!("share page not found: {share_id}"))
}

pub(crate) fn unpublish_share_page(share: &ShareConfig, id: &str) -> Result<SharePage> {
    unpublish_share_page_with(share, id, deploy_pages)
}

fn unpublish_share_page_with<F>(share: &ShareConfig, id: &str, deploy: F) -> Result<SharePage>
where
    F: FnOnce(&Path, &str) -> Result<()>,
{
    let page = find_share_page(share, id)?;
    let html = fs::read(&page.file_path)
        .with_context(|| format!("failed to read {}", page.file_path.display()))?;
    fs::remove_file(&page.file_path)
        .with_context(|| format!("failed to remove {}", page.file_path.display()))?;
    if let Err(error) = deploy(&expand_path(&share.publish_dir), &share.project_name) {
        fs::write(&page.file_path, html).with_context(|| {
            format!("failed to restore {} after deploy error: {error}", page.file_path.display())
        })?;
        return Err(error);
    }
    Ok(page)
}

fn collect_share_pages(publish_dir: &Path, project_domain: &str) -> Result<Vec<SharePage>> {
    let mut ranked = Vec::new();
    for entry in fs::read_dir(publish_dir)
        .with_context(|| format!("failed to read {}", publish_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(share_id) = name.strip_suffix(".html") else {
            continue;
        };
        if normalize_share_id(share_id).is_err() {
            continue;
        }
        let Some(page) = load_share_page(publish_dir, project_domain, share_id)? else {
            continue;
        };
        let modified = fs::metadata(&page.file_path).and_then(|meta| meta.modified()).ok();
        ranked.push((modified, page));
    }
    ranked.sort_by(|a, b| match (a.0, b.0) {
        (Some(left), Some(right)) => right.cmp(&left).then_with(|| a.1.share_id.cmp(&b.1.share_id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.share_id.cmp(&b.1.share_id),
    });
    Ok(ranked.into_iter().map(|(_, page)| page).collect())
}

fn load_share_page(
    publish_dir: &Path,
    project_domain: &str,
    share_id: &str,
) -> Result<Option<SharePage>> {
    let file_path = publish_dir.join(format!("{share_id}.html"));
    if !file_path.is_file() {
        return Ok(None);
    }
    let html = fs::read_to_string(&file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;
    let html_bytes = html.len();
    let (title, source) = parse_share_page_meta(&html);
    Ok(Some(SharePage {
        share_id: share_id.to_string(),
        url: share_page_url(project_domain, share_id),
        title,
        source,
        file_path,
        html_bytes,
    }))
}

fn parse_share_page_meta(html: &str) -> (Option<String>, Option<String>) {
    let title = extract_between(html, "<title>", "</title>").map(unescape_html);
    let source = source_from_meta_items(html).or_else(|| source_from_legacy_meta(html));
    (title.filter(|value| !value.is_empty()), source)
}

fn source_from_meta_items(html: &str) -> Option<String> {
    extract_between(html, "<span class=\"meta-item\">", "</span>")
        .map(unescape_html)
        .filter(|value| !value.is_empty())
}

fn source_from_legacy_meta(html: &str) -> Option<String> {
    let raw = extract_between(html, "<p class=\"meta\">", "</p>").map(unescape_html)?;
    let source = raw.split('·').next().map(str::trim).unwrap_or("");
    if source.is_empty() { None } else { Some(source.to_string()) }
}

fn extract_between(haystack: &str, start: &str, end: &str) -> Option<String> {
    let from = haystack.find(start)? + start.len();
    let rest = haystack.get(from..)?;
    let to = rest.find(end)?;
    Some(rest[..to].to_string())
}

fn unescape_html(input: String) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn resolve_share_id(input: &str, project_domain: &str) -> Result<String> {
    let trimmed = input.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return normalize_share_id(trimmed);
    };
    if !scheme.eq_ignore_ascii_case("https") {
        bail!("share URL must be https://{project_domain}/<share-id>");
    }
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, path),
        None => (rest, ""),
    };
    if !host.eq_ignore_ascii_case(project_domain) {
        bail!("share URL must be https://{project_domain}/<share-id>");
    }
    let path = path.trim_matches('/');
    if path.is_empty() || path.contains('/') {
        bail!("share URL must be https://{project_domain}/<share-id>");
    }
    normalize_share_id(path)
}

fn normalize_share_id(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("missing share id");
    }
    let id = trimmed.strip_suffix(".html").unwrap_or(trimmed);
    let valid = !id.is_empty()
        && id != "."
        && id != ".."
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid { Ok(id.to_string()) } else { bail!("invalid share id '{input}'") }
}

fn print_inventory_text(inventory: &ShareInventory) {
    print!("{}", inventory_text(inventory));
}

fn inventory_text(inventory: &ShareInventory) -> String {
    inventory_text_with_width(inventory, output_columns())
}

const TITLE_COL_MAX: usize = 32;

fn inventory_text_with_width(inventory: &ShareInventory, columns: usize) -> String {
    if inventory.shares.is_empty() {
        return "No published shares.\nPublish one with `recall session share --id <session-id>`.\n"
            .to_string();
    }
    let source_width = inventory
        .shares
        .iter()
        .map(|page| display_width(page.source.as_deref().unwrap_or("-")))
        .chain(std::iter::once(display_width("SOURCE")))
        .max()
        .unwrap_or(display_width("SOURCE"));
    let url_width = inventory
        .shares
        .iter()
        .map(|page| display_width(&page.url))
        .chain(std::iter::once(display_width("URL")))
        .max()
        .unwrap_or(display_width("URL"));
    let title_width = inventory
        .shares
        .iter()
        .map(|page| display_width(&truncate_width(&share_title(page), TITLE_COL_MAX)))
        .chain(std::iter::once(display_width("TITLE")))
        .max()
        .unwrap_or(display_width("TITLE"))
        .min(TITLE_COL_MAX)
        .min(
            columns.saturating_sub(source_width.saturating_add(url_width).saturating_add(4)).max(5),
        );
    let mut out = String::new();
    out.push_str(&format!(
        "{}  {}  {}\n",
        pad_width("TITLE", title_width),
        pad_width("SOURCE", source_width),
        pad_width("URL", url_width)
    ));
    for page in &inventory.shares {
        out.push_str(&format!(
            "{}  {}  {}\n",
            pad_width(&share_title(page), title_width),
            pad_width(page.source.as_deref().unwrap_or("-"), source_width),
            pad_width(&page.url, url_width)
        ));
    }
    let count = inventory.shares.len();
    out.push_str(&format!(
        "\n{count} share{}  {}\n",
        if count == 1 { "" } else { "s" },
        inventory.url_base
    ));
    out
}

fn output_columns() -> usize {
    if !io::stdout().is_terminal() {
        return 120;
    }
    crossterm::terminal::size()
        .ok()
        .map(|(width, _)| usize::from(width))
        .filter(|width| *width >= 40)
        .unwrap_or(80)
}

fn share_title(page: &SharePage) -> String {
    let cleaned = page.title.as_deref().map(clean_share_title).unwrap_or_default();
    if cleaned.is_empty() { page.share_id.clone() } else { cleaned }
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn truncate_width(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(text) <= max {
        return text.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let budget = max - 1;
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > budget {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push('…');
    out
}

fn pad_width(text: &str, width: usize) -> String {
    let truncated = truncate_width(text, width);
    let pad = width.saturating_sub(display_width(&truncated));
    format!("{truncated}{}", " ".repeat(pad))
}

fn clean_share_title(raw: &str) -> String {
    let unwrapped = unwrap_markdown_links(raw);
    let stripped = unwrapped.trim().trim_start_matches('#').trim();
    let mut out = String::new();
    for word in stripped.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn unwrap_markdown_links(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(mid) = after.find("](") {
            let label = &after[..mid];
            if !label.contains('[') {
                out.push_str(label);
                let after_mid = &after[mid + 2..];
                rest = after_mid.find(')').map(|end| &after_mid[end + 1..]).unwrap_or("");
                continue;
            }
        }
        out.push('[');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn print_unpublish_result(page: &SharePage, dry_run: bool, format: ShareFormat) -> Result<()> {
    match format {
        ShareFormat::Text => {
            if dry_run {
                println!("Dry run OK");
            }
            println!("{}", page.url);
        }
        ShareFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "share": page,
                    "dry_run": dry_run
                }))?
            );
        }
    }
    Ok(())
}

fn confirm_unpublish(page: &SharePage, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!("pass --yes to unpublish without confirmation");
    }
    eprint!("Unpublish {}? [y/N] ", page.url);
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();
    if trimmed == "y" || trimmed == "yes" {
        Ok(())
    } else {
        bail!("unpublish cancelled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::meta::collect_session_display_meta;
    use crate::share::publish::init_publish_dir;
    use crate::share::render::render_session_html;
    use crate::types::Session;

    #[test]
    fn normalize_share_id_accepts_html_suffix() {
        assert_eq!(normalize_share_id("foo-bar.html").unwrap(), "foo-bar");
        assert!(normalize_share_id("../secret").is_err());
        assert!(normalize_share_id(".recall-share").is_err());
    }

    #[test]
    fn resolve_share_id_requires_configured_https_origin() {
        let domain = "recall-share-abc.pages.dev";
        assert_eq!(
            resolve_share_id(
                "https://recall-share-abc.pages.dev/019e6d8d-588b-7fd2-a326-c525469ed120",
                domain
            )
            .unwrap(),
            "019e6d8d-588b-7fd2-a326-c525469ed120"
        );
        assert_eq!(
            resolve_share_id("HTTPS://Recall-Share-ABC.pages.dev/abc-123.html?x=1#top", domain)
                .unwrap(),
            "abc-123"
        );
        assert_eq!(resolve_share_id("abc-123", domain).unwrap(), "abc-123");
        assert!(resolve_share_id("https://example.com/abc-123", domain).is_err());
        assert!(resolve_share_id("http://recall-share-abc.pages.dev/abc-123", domain).is_err());
        assert!(resolve_share_id("https://recall-share-abc.pages.dev/foo/bar", domain).is_err());
        assert!(resolve_share_id("https://recall-share-abc.pages.dev/", domain).is_err());
    }

    #[test]
    fn parse_share_page_meta_reads_legacy_paragraph_source() {
        let current = concat!(
            "<title>Now</title>",
            "<span class=\"meta-item\">Goose</span>",
            "<span class=\"meta-item\">2026-09-01 13:57</span>",
        );
        assert_eq!(
            parse_share_page_meta(current),
            (Some("Now".to_string()), Some("Goose".to_string()))
        );
        let legacy = concat!(
            "<title>Old</title>",
            "<p class=\"meta\">Cursor · 2026-06-16 18:03 · 104 messages · Model: composer-2.5</p>",
        );
        assert_eq!(
            parse_share_page_meta(legacy),
            (Some("Old".to_string()), Some("Cursor".to_string()))
        );
    }

    #[test]
    fn inventory_text_is_one_line_table_with_footer() {
        let inventory = ShareInventory {
            provider: "cloudflare-pages".to_string(),
            project_name: "recall-share-test".to_string(),
            project_domain: "recall-share-test.pages.dev".to_string(),
            publish_dir: PathBuf::from("/tmp/share"),
            url_base: "https://recall-share-test.pages.dev".to_string(),
            shares: vec![
                SharePage {
                    share_id: "abc".to_string(),
                    url: "https://recall-share-test.pages.dev/abc".to_string(),
                    title: Some("[.local/note.md](/Users/x/git/note.md) leftover".to_string()),
                    source: Some("Grok".to_string()),
                    file_path: PathBuf::from("/tmp/share/abc.html"),
                    html_bytes: 12,
                },
                SharePage {
                    share_id: "def".to_string(),
                    url: "https://recall-share-test.pages.dev/def".to_string(),
                    title: Some("[.local/note.md](/Users/x/git/sam...".to_string()),
                    source: None,
                    file_path: PathBuf::from("/tmp/share/def.html"),
                    html_bytes: 8,
                },
            ],
        };
        let text = inventory_text_with_width(&inventory, 120);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), ["TITLE", "SOURCE", "URL"]);
        assert!(lines[1].contains(".local/note.md leftover"));
        assert!(lines[1].contains("Grok"));
        assert!(lines[1].contains("https://recall-share-test.pages.dev/abc"));
        assert!(!lines[1].contains('\n'));
        assert!(lines[2].contains(".local/note.md"));
        assert!(lines[2].contains("https://recall-share-test.pages.dev/def"));
        assert_eq!(lines[3], "");
        assert_eq!(lines[4], "2 shares  https://recall-share-test.pages.dev");
        let header_title_width = lines[0].find("SOURCE").unwrap();
        assert!(header_title_width <= TITLE_COL_MAX + 2);
    }

    #[test]
    fn list_reads_managed_html_and_skips_control_files() {
        let dir = tempfile::tempdir().unwrap();
        init_publish_dir(dir.path()).unwrap();
        let session = session("019e6d8d-588b-7fd2-a326-c525469ed120");
        let meta = collect_session_display_meta(&session, &[]);
        fs::write(
            dir.path().join("019e6d8d-588b-7fd2-a326-c525469ed120.html"),
            render_session_html(&session, &[], &[], &meta),
        )
        .unwrap();

        let config = config_for(dir.path());
        let inventory = list_share_pages(&config).unwrap();
        assert_eq!(inventory.shares.len(), 1);
        assert_eq!(inventory.shares[0].share_id, "019e6d8d-588b-7fd2-a326-c525469ed120");
        assert_eq!(
            inventory.shares[0].url,
            "https://recall-share-test.pages.dev/019e6d8d-588b-7fd2-a326-c525469ed120"
        );
        assert_eq!(inventory.shares[0].title.as_deref(), Some("Fix <bug>"));
        assert_eq!(inventory.shares[0].source.as_deref(), Some("Codex"));
    }

    #[test]
    fn list_missing_publish_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let inventory = list_share_pages(&config_for(&missing)).unwrap();
        assert!(inventory.shares.is_empty());
    }

    #[test]
    fn list_rejects_unmanaged_publish_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "not a share page").unwrap();
        let error = list_share_pages(&config_for(dir.path())).unwrap_err();
        assert!(error.to_string().contains("not managed by Recall"));
    }

    #[test]
    fn unpublish_removes_file_and_restores_on_deploy_failure() {
        let dir = tempfile::tempdir().unwrap();
        init_publish_dir(dir.path()).unwrap();
        let session = session("keep-me");
        let meta = collect_session_display_meta(&session, &[]);
        let html = render_session_html(&session, &[], &[], &meta);
        let path = dir.path().join("keep-me.html");
        fs::write(&path, &html).unwrap();
        let share = share_config(dir.path());

        let error =
            unpublish_share_page_with(&share, "keep-me", |_, _| bail!("deploy boom")).unwrap_err();
        assert!(error.to_string().contains("deploy boom"));
        assert_eq!(fs::read_to_string(&path).unwrap(), html);

        let page = unpublish_share_page_with(&share, "keep-me", |_, _| Ok(())).unwrap();
        assert_eq!(page.share_id, "keep-me");
        assert!(!path.exists());
    }

    #[test]
    fn unpublish_rejects_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        init_publish_dir(dir.path()).unwrap();
        let share = share_config(dir.path());
        let error = find_share_page(&share, "missing").unwrap_err();
        assert!(error.to_string().contains("share page not found"));
    }

    fn config_for(publish_dir: &Path) -> AppConfig {
        let mut config = AppConfig::default();
        config.share = Some(share_config(publish_dir));
        config
    }

    fn share_config(publish_dir: &Path) -> ShareConfig {
        ShareConfig {
            provider: "cloudflare-pages".to_string(),
            project_name: "recall-share-test".to_string(),
            project_domain: "recall-share-test.pages.dev".to_string(),
            publish_dir: publish_dir.to_string_lossy().to_string(),
        }
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
