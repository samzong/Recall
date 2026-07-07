use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};

use crate::types::{Message, Role, Session};
use crate::utils;

use super::assets::{CHEVRON_SVG, SESSION_PAGE_CSS, TOC_NAV_SCRIPT};
use super::meta::SessionDisplayMeta;

#[derive(Debug, Clone, Default)]
pub(crate) struct ShareRenderOptions {
    pub(crate) tldr_markdown: Option<String>,
}

pub(crate) fn share_id_for_session(session: &Session) -> String {
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
pub(crate) fn render_session_html(
    session: &Session,
    messages: &[Message],
    display_meta: &SessionDisplayMeta,
) -> String {
    render_session_html_with_tldr(session, messages, display_meta, None)
}

pub(crate) fn render_session_html_with_tldr(
    session: &Session,
    messages: &[Message],
    display_meta: &SessionDisplayMeta,
    tldr_markdown: Option<&str>,
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
    if let Some(tldr) = tldr_markdown.filter(|tldr| !tldr.trim().is_empty()) {
        render_tldr_html(&mut out, tldr);
    }
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
fn render_tldr_html(out: &mut String, markdown: &str) {
    out.push_str("<section class=\"tldr\" aria-labelledby=\"tldr-title\">");
    out.push_str("<h2 id=\"tldr-title\" class=\"tldr-title\">TL;DR</h2>");
    render_content(out, markdown);
    out.push_str("</section>");
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
    use super::super::meta::SessionDisplayMeta;
    use super::*;
    use crate::types::{Message, Role, Session};

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
    fn html_renderer_places_tldr_before_transcript_and_escapes_it() {
        let html = render_session_html_with_tldr(
            &session("s1"),
            &[Message {
                session_id: "local-id".to_string(),
                role: Role::User,
                content: "First question".to_string(),
                timestamp: None,
                seq: 0,
            }],
            &SessionDisplayMeta::default(),
            Some("**Query:** <script>alert('x')</script>"),
        );

        let tldr = html.find("class=\"tldr\"").unwrap();
        let first_turn = html.find("class=\"turn user\"").unwrap();
        assert!(tldr < first_turn);
        assert!(html.contains("TL;DR"));
        assert!(html.contains("&lt;script&gt;alert("));
        assert!(html.contains("&lt;/script&gt;"));
        assert!(!html.contains("<script>alert"));
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
