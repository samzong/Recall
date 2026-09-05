use std::collections::{BTreeMap, HashMap, HashSet};

use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd, html};

use crate::types::{EvidenceVisibility, Message, Role, Session, SessionEventRecord};
use crate::utils;

use super::assets::{CHEVRON_SVG, SESSION_PAGE_CSS, SESSION_PAGE_SCRIPT};
use super::meta::SessionDisplayMeta;

const STRUCTURED_EVENT_LIMIT: usize = 256;
const STRUCTURED_FIELD_CHAR_LIMIT: usize = 240;
const STRUCTURED_SUMMARY_CHAR_LIMIT: usize = 240;
const STRUCTURED_TEXT_CHAR_LIMIT: usize = 32_000;

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
#[cfg(any(test, feature = "bench"))]
pub(crate) fn render_session_html(
    session: &Session,
    messages: &[Message],
    events: &[SessionEventRecord],
    display_meta: &SessionDisplayMeta,
) -> String {
    render_session_html_inner(session, messages, events, display_meta, None, true)
}

pub(crate) fn render_session_preview_html(
    session: &Session,
    messages: &[Message],
    events: &[SessionEventRecord],
    display_meta: &SessionDisplayMeta,
) -> String {
    render_session_html_inner(session, messages, events, display_meta, None, false)
}

pub(crate) fn render_session_html_with_tldr(
    session: &Session,
    messages: &[Message],
    events: &[SessionEventRecord],
    display_meta: &SessionDisplayMeta,
    tldr_markdown: Option<&str>,
) -> String {
    render_session_html_inner(session, messages, events, display_meta, tldr_markdown, true)
}

fn render_session_html_inner(
    session: &Session,
    messages: &[Message],
    events: &[SessionEventRecord],
    display_meta: &SessionDisplayMeta,
    tldr_markdown: Option<&str>,
    load_external_fonts: bool,
) -> String {
    let title = session.custom_title.as_deref().unwrap_or(&session.title);
    let display_title = display_title(title);
    let blocks = prepare_render_blocks(messages, events, session.source == "claude-code");
    let user_toc = collect_user_toc(&blocks);
    let mut out = String::new();
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    out.push_str("<meta name=\"robots\" content=\"noindex,nofollow\">");
    out.push_str("<link rel=\"icon\" href=\"data:,\">");
    if load_external_fonts {
        out.push_str("<link rel=\"preconnect\" href=\"https://fonts.googleapis.com\">");
        out.push_str("<link rel=\"preconnect\" href=\"https://fonts.gstatic.com\" crossorigin>");
        out.push_str("<link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2?family=Newsreader:opsz,wght@6..72,400;6..72,500;6..72,600&family=JetBrains+Mono:wght@400;500&display=swap\">");
    }
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
    out.push_str("<button type=\"button\" class=\"details-toggle\" aria-expanded=\"false\" hidden>Expand all</button>");
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
    out.push_str(SESSION_PAGE_SCRIPT);
    out.push_str("</body></html>");
    out
}
fn render_tldr_html(out: &mut String, markdown: &str) {
    out.push_str("<section class=\"tldr\" aria-labelledby=\"tldr-title\">");
    out.push_str("<h2 id=\"tldr-title\" class=\"tldr-title\">TL;DR</h2>");
    render_content(out, markdown, false);
    out.push_str("</section>");
}

enum RenderBlock {
    User(String),
    Assistant(Vec<AssistantSegment>),
}

enum AssistantSegment {
    Text { content: String, suppress_legacy_logs: bool },
    LegacyTools(Vec<String>),
    StructuredTools { executions: Vec<StructuredToolExecution>, truncated: bool },
}

struct StructuredToolTimeline {
    by_message: BTreeMap<u32, Vec<StructuredToolExecution>>,
    covered_messages: HashSet<u32>,
    result_anchors: HashSet<u32>,
    truncated_at: Option<u32>,
}

struct StructuredToolExecution {
    paired: bool,
    events: Vec<PublicToolEvent>,
}

struct PublicToolEvent {
    event_seq: u32,
    message_seq: u32,
    kind: String,
    name: Option<String>,
    status: Option<String>,
    target: Option<String>,
    summary: Option<String>,
    tool_call_id: Option<String>,
}

fn prepare_render_blocks(
    messages: &[Message],
    events: &[SessionEventRecord],
    legacy_user_tool_results: bool,
) -> Vec<RenderBlock> {
    let mut blocks = Vec::new();
    let mut pending_tools = Vec::new();
    let mut structured_coverage_active = false;
    let mut structured_result_active = false;
    let mut suppressed_tool_pending = false;
    let mut structured = prepare_structured_tool_timeline(messages, events);

    let attach_tools = |blocks: &mut Vec<RenderBlock>, pending: &mut Vec<String>| {
        if pending.is_empty() {
            return;
        }
        let tools = std::mem::take(pending);
        if let Some(RenderBlock::Assistant(segments)) = blocks.last_mut() {
            segments.push(AssistantSegment::LegacyTools(tools));
        } else {
            blocks.push(RenderBlock::Assistant(vec![AssistantSegment::LegacyTools(tools)]));
        }
    };

    let attach_structured = |blocks: &mut Vec<RenderBlock>,
                             executions: Vec<StructuredToolExecution>,
                             truncated: bool| {
        let segment = AssistantSegment::StructuredTools { executions, truncated };
        if let Some(RenderBlock::Assistant(segments)) = blocks.last_mut() {
            segments.push(segment);
        } else {
            blocks.push(RenderBlock::Assistant(vec![segment]));
        }
    };

    for message in messages {
        let (anchored, result_anchored) = structured.as_ref().map_or((false, false), |timeline| {
            (
                timeline.covered_messages.contains(&message.seq),
                timeline.result_anchors.contains(&message.seq),
            )
        });
        if anchored {
            structured_coverage_active = true;
            structured_result_active = result_anchored;
        } else if !legacy_user_tool_results {
            structured_coverage_active = false;
            structured_result_active = false;
        }
        let tool_shaped = is_tool_message(&message.content);
        match message.role {
            Role::User if suppressed_tool_pending => {
                suppressed_tool_pending = false;
            }
            Role::User if !pending_tools.is_empty() => {
                pending_tools.push(message.content.clone());
            }
            Role::User => {
                structured_coverage_active = anchored;
                structured_result_active = result_anchored;
                blocks.push(RenderBlock::User(message.content.clone()));
            }
            Role::Assistant if structured_coverage_active && tool_shaped => {
                attach_tools(&mut blocks, &mut pending_tools);
                suppressed_tool_pending = legacy_user_tool_results && structured_result_active;
                if let Some(RenderBlock::Assistant(segments)) = blocks.last_mut() {
                    append_assistant_text_segment(segments, message.content.clone(), true);
                } else {
                    blocks.push(RenderBlock::Assistant(vec![AssistantSegment::Text {
                        content: message.content.clone(),
                        suppress_legacy_logs: true,
                    }]));
                }
            }
            Role::Assistant if tool_shaped => {
                pending_tools.push(message.content.clone());
            }
            Role::Assistant => {
                attach_tools(&mut blocks, &mut pending_tools);
                suppressed_tool_pending = false;
                structured_coverage_active = anchored;
                structured_result_active = result_anchored;
                if let Some(RenderBlock::Assistant(segments)) = blocks.last_mut() {
                    append_assistant_text_segment(segments, message.content.clone(), anchored);
                } else {
                    blocks.push(RenderBlock::Assistant(vec![AssistantSegment::Text {
                        content: message.content.clone(),
                        suppress_legacy_logs: anchored,
                    }]));
                }
            }
        }
        if let Some(timeline) = structured.as_mut()
            && let Some(executions) = timeline.by_message.remove(&message.seq)
        {
            let truncated = timeline.truncated_at == Some(message.seq);
            attach_structured(&mut blocks, executions, truncated);
        }
    }
    attach_tools(&mut blocks, &mut pending_tools);
    blocks
}

fn prepare_structured_tool_timeline(
    messages: &[Message],
    events: &[SessionEventRecord],
) -> Option<StructuredToolTimeline> {
    let message_sequences = messages.iter().map(|message| message.seq).collect::<HashSet<_>>();
    let mut candidates = events
        .iter()
        .filter(|event| is_public_tool_event(event, &message_sequences))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|event| (event.message_seq.unwrap_or_default(), event.event_seq));
    if candidates.is_empty() {
        return None;
    }

    let mut public_events = Vec::new();
    let mut text_chars = 0usize;
    let mut truncated = false;
    let mut group_start = 0usize;
    while group_start < candidates.len() {
        let message_seq = candidates[group_start].message_seq;
        let group_end = candidates[group_start..]
            .iter()
            .position(|event| event.message_seq != message_seq)
            .map_or(candidates.len(), |offset| group_start + offset);
        if public_events.len() + group_end - group_start > STRUCTURED_EVENT_LIMIT {
            truncated = true;
            break;
        }
        let group = candidates[group_start..group_end]
            .iter()
            .map(|event| public_tool_event(event))
            .collect::<Vec<_>>();
        let group_chars = group.iter().map(public_event_text_chars).sum::<usize>();
        if text_chars + group_chars > STRUCTURED_TEXT_CHAR_LIMIT {
            truncated = true;
            break;
        }
        text_chars += group_chars;
        public_events.extend(group);
        group_start = group_end;
    }
    if public_events.is_empty() {
        return None;
    }

    let covered_messages =
        public_events.iter().map(|event| event.message_seq).collect::<HashSet<_>>();
    let result_anchors = public_events
        .iter()
        .filter(|event| event.kind == "tool_result")
        .map(|event| event.message_seq)
        .collect::<HashSet<_>>();
    let mut relation_shapes: HashMap<String, (bool, bool)> = HashMap::new();
    for event in &public_events {
        let Some(tool_call_id) = event.tool_call_id.as_ref() else {
            continue;
        };
        let shape = relation_shapes.entry(tool_call_id.clone()).or_default();
        if event.kind == "tool_result" {
            shape.1 = true;
        } else {
            shape.0 = true;
        }
    }
    let pairable = relation_shapes
        .into_iter()
        .filter_map(|(tool_call_id, (has_call, has_result))| {
            (has_call && has_result).then_some(tool_call_id)
        })
        .collect::<HashSet<_>>();

    let mut executions = Vec::<StructuredToolExecution>::new();
    let mut paired_indexes = HashMap::<String, usize>::new();
    for event in public_events {
        let pair_id = event
            .tool_call_id
            .as_ref()
            .filter(|tool_call_id| pairable.contains(*tool_call_id))
            .cloned();
        if let Some(pair_id) = pair_id {
            if let Some(index) = paired_indexes.get(&pair_id).copied() {
                executions[index].events.push(event);
            } else {
                let index = executions.len();
                paired_indexes.insert(pair_id, index);
                executions.push(StructuredToolExecution { paired: true, events: vec![event] });
            }
        } else {
            executions.push(StructuredToolExecution { paired: false, events: vec![event] });
        }
    }

    let mut by_message = BTreeMap::<u32, Vec<StructuredToolExecution>>::new();
    for mut execution in executions {
        execution.events.sort_by_key(|event| event.event_seq);
        let message_seq = execution.events[0].message_seq;
        by_message.entry(message_seq).or_default().push(execution);
    }
    let truncated_at = truncated.then(|| *by_message.keys().next_back().unwrap());
    Some(StructuredToolTimeline { by_message, covered_messages, result_anchors, truncated_at })
}

fn is_public_tool_event(event: &SessionEventRecord, message_sequences: &HashSet<u32>) -> bool {
    let positioned = event.message_seq.is_some_and(|seq| message_sequences.contains(&seq));
    positioned
        && event.is_meta != Some(true)
        && !matches!(
            event.visibility,
            Some(EvidenceVisibility::Hidden | EvidenceVisibility::Inactive)
        )
        && matches!(
            event.kind.as_str(),
            "tool_call" | "tool_result" | "command" | "search" | "file_read" | "file_write"
        )
}

fn public_tool_event(event: &SessionEventRecord) -> PublicToolEvent {
    PublicToolEvent {
        event_seq: event.event_seq,
        message_seq: event.message_seq.unwrap_or_default(),
        kind: truncate_display_text(&event.kind, STRUCTURED_FIELD_CHAR_LIMIT),
        name: public_field(event.name.as_deref(), STRUCTURED_FIELD_CHAR_LIMIT),
        status: public_field(event.status.as_deref(), STRUCTURED_FIELD_CHAR_LIMIT),
        target: public_field(event.target.as_deref(), STRUCTURED_FIELD_CHAR_LIMIT),
        summary: public_event_summary(event),
        tool_call_id: event
            .tool_call_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|value| value.chars().count() <= STRUCTURED_FIELD_CHAR_LIMIT)
            .map(String::from),
    }
}

fn public_event_summary(event: &SessionEventRecord) -> Option<String> {
    if event.kind == "tool_result" || event.name.is_some() || event.target.is_some() {
        return None;
    }
    let summary = event.summary.as_deref()?.trim();
    if summary.is_empty()
        || summary.contains(['\n', '\r', '{', '}', '[', ']'])
        || summary.starts_with("<oai-")
    {
        return None;
    }
    Some(truncate_display_text(summary, STRUCTURED_SUMMARY_CHAR_LIMIT))
}

fn public_field(value: Option<&str>, max_chars: usize) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncate_display_text(value, max_chars))
}

fn public_event_text_chars(event: &PublicToolEvent) -> usize {
    event.kind.chars().count()
        + event.name.as_deref().map_or(0, |value| value.chars().count())
        + event.status.as_deref().map_or(0, |value| value.chars().count())
        + event.target.as_deref().map_or(0, |value| value.chars().count())
        + event.summary.as_deref().map_or(0, |value| value.chars().count())
}

fn append_assistant_text_segment(
    segments: &mut Vec<AssistantSegment>,
    content: String,
    suppress_legacy_logs: bool,
) {
    if let Some(AssistantSegment::Text {
        content: previous,
        suppress_legacy_logs: previous_suppression,
    }) = segments.last_mut()
    {
        let previous_core = assistant_text_core(previous);
        let content_core = assistant_text_core(&content);
        if !previous_core.is_empty() && previous_core == content_core {
            if content.contains("<oai-mem-citation>") && !previous.contains("<oai-mem-citation>") {
                *previous = content;
            }
            *previous_suppression |= suppress_legacy_logs;
            return;
        }
    }
    segments.push(AssistantSegment::Text { content, suppress_legacy_logs });
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
            render_content(out, &content, false);
            out.push_str("</div></section>");
        }
        RenderBlock::Assistant(segments) => {
            out.push_str(
                "<section class=\"turn assistant\"><span class=\"role-label\">Assistant</span><div class=\"assistant-body\">",
            );
            for segment in segments {
                match segment {
                    AssistantSegment::Text { content, suppress_legacy_logs } => {
                        render_content(out, &content, suppress_legacy_logs);
                    }
                    AssistantSegment::LegacyTools(logs) => {
                        out.push_str("<div class=\"tool-run\">");
                        render_tool_group(out, &logs);
                        out.push_str("</div>");
                    }
                    AssistantSegment::StructuredTools { executions, truncated } => {
                        out.push_str("<div class=\"tool-run\">");
                        render_structured_tool_group(out, &executions);
                        if truncated {
                            out.push_str(
                                "<p class=\"tool-timeline-note\">Tool timeline truncated.</p>",
                            );
                        }
                        out.push_str("</div>");
                    }
                }
            }
            out.push_str("</div></section>");
        }
    }
}

fn render_content(out: &mut String, text: &str, suppress_legacy_logs: bool) {
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
            if suppress_legacy_logs {
                rendered = true;
            } else {
                pending_logs.push(sanitized);
                rendered = true;
            }
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

fn render_structured_tool_group(out: &mut String, executions: &[StructuredToolExecution]) {
    if executions.is_empty() {
        return;
    }
    if executions.len() == 1 {
        render_structured_execution(out, &executions[0]);
        return;
    }
    out.push_str("<details class=\"tool-group\"><summary>");
    out.push_str(CHEVRON_SVG);
    out.push_str(&escape_html(&format!("{} tool executions", executions.len())));
    out.push_str("<span class=\"count\">");
    out.push_str(&executions.len().to_string());
    out.push_str("</span></summary><div class=\"tool-group-items\">");
    for execution in executions {
        render_structured_execution(out, execution);
    }
    out.push_str("</div></details>");
}

fn render_structured_execution(out: &mut String, execution: &StructuredToolExecution) {
    let primary = execution
        .events
        .iter()
        .find(|event| event.kind != "tool_result")
        .unwrap_or(&execution.events[0]);
    let name = primary.name.as_deref().unwrap_or_else(|| structured_kind_label(&primary.kind));
    out.push_str("<details class=\"log\"><summary>");
    out.push_str(CHEVRON_SVG);
    out.push_str("<span class=\"badge\">");
    out.push_str(structured_kind_badge(&primary.kind));
    out.push_str("</span><span class=\"lname\">");
    out.push_str(&escape_html(name));
    out.push_str("</span>");
    if execution.paired {
        out.push_str("<span class=\"relation\">call + result</span>");
    }
    out.push_str("</summary><pre>");
    for (index, event) in execution.events.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        render_structured_event_text(out, event);
    }
    out.push_str("</pre></details>");
}

fn render_structured_event_text(out: &mut String, event: &PublicToolEvent) {
    out.push_str(if event.kind == "tool_result" { "Result" } else { "Call" });
    out.push_str(": ");
    out.push_str(&escape_html(structured_kind_label(&event.kind)));
    if let Some(name) = event.name.as_deref() {
        out.push_str(" · ");
        out.push_str(&escape_html(name));
    }
    if let Some(target) = event.target.as_deref() {
        out.push_str("\nTarget: ");
        out.push_str(&escape_html(target));
    }
    if let Some(status) = event.status.as_deref() {
        out.push_str("\nStatus: ");
        out.push_str(&escape_html(status));
    }
    if let Some(summary) = event.summary.as_deref() {
        out.push_str("\nSummary: ");
        out.push_str(&escape_html(summary));
    }
}

fn structured_kind_badge(kind: &str) -> &'static str {
    match kind {
        "command" => "command",
        "search" => "search",
        "file_read" => "read",
        "file_write" => "write",
        "tool_result" => "result",
        _ => "tool",
    }
}

fn structured_kind_label(kind: &str) -> &'static str {
    match kind {
        "command" => "Command",
        "search" => "Search",
        "file_read" => "File read",
        "file_write" => "File write",
        "tool_result" => "Tool result",
        _ => "Tool call",
    }
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
        render_session_html(session, messages, &[], &SessionDisplayMeta::default())
    }

    fn render_html_with_events(
        session: &Session,
        messages: &[Message],
        events: &[SessionEventRecord],
    ) -> String {
        render_session_html(session, messages, events, &SessionDisplayMeta::default())
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

    fn message(role: Role, content: &str, seq: u32) -> Message {
        Message {
            session_id: "local-id".to_string(),
            role,
            content: content.to_string(),
            timestamp: None,
            seq,
        }
    }

    fn event(
        event_seq: u32,
        kind: &str,
        message_seq: Option<u32>,
        tool_call_id: Option<&str>,
    ) -> SessionEventRecord {
        SessionEventRecord {
            files: Vec::new(),
            event_seq,
            timestamp: None,
            kind: kind.to_string(),
            actor: if kind == "tool_result" { "tool" } else { "assistant" }.to_string(),
            name: None,
            status: None,
            target: None,
            message_seq,
            summary: None,
            source_path: Some("/private/transcript.jsonl".to_string()),
            source_event_id: Some(format!("source-{event_seq}")),
            tool_call_id: tool_call_id.map(String::from),
            is_meta: None,
            visibility: None,
            attrs_json: Some("{\"secret\":true}".to_string()),
            parser_version: 1,
        }
    }

    #[test]
    fn structured_timeline_pairs_only_source_relations_and_orders_by_event_sequence() {
        let messages = [
            message(Role::User, "Question", 0),
            message(Role::Assistant, "Before tools", 1),
            message(Role::Assistant, "After tools", 2),
        ];
        let mut events = vec![
            event(4, "tool_result", Some(2), Some("call-1")),
            event(1, "tool_call", Some(1), None),
            event(5, "tool_result", Some(2), Some("call-1")),
            event(0, "tool_call", Some(1), Some("call-1")),
            event(2, "tool_result", Some(1), None),
        ];
        events[0].name = Some("Read".to_string());
        events[1].name = Some("Independent".to_string());
        events[2].status = Some("complete".to_string());
        events[3].name = Some("Read".to_string());
        events[3].target = Some("src/lib.rs".to_string());
        events[4].name = Some("Independent".to_string());

        let html = render_html_with_events(&session("s1"), &messages, &events);

        assert!(html.contains("3 tool executions"));
        assert_eq!(html.matches("call + result").count(), 1);
        assert_eq!(html.matches("Result: Tool result").count(), 3);
        assert_eq!(html.matches("class=\"lname\">Independent</span>").count(), 2);
        let before = html.find("Before tools").unwrap();
        let call = html.find("Target: src/lib.rs").unwrap();
        let first_result = html.find("Result: Tool result").unwrap();
        let after = html.find("After tools").unwrap();
        assert!(before < call);
        assert!(call < first_result);
        assert!(first_result < after);
    }

    #[test]
    fn structured_timeline_filters_unpositioned_meta_and_explicitly_hidden_events() {
        let messages = [message(Role::Assistant, "Visible answer", 10)];
        let mut events = vec![
            event(0, "tool_call", Some(10), None),
            event(1, "tool_call", Some(10), None),
            event(2, "tool_call", Some(10), None),
            event(3, "tool_call", Some(10), None),
            event(4, "tool_call", None, None),
            event(5, "tool_call", Some(99), None),
        ];
        for (event, name) in events.iter_mut().zip([
            "unknown-visibility",
            "hidden",
            "inactive",
            "meta",
            "null-anchor",
            "invalid-anchor",
        ]) {
            event.name = Some(name.to_string());
        }
        events[1].visibility = Some(EvidenceVisibility::Hidden);
        events[2].visibility = Some(EvidenceVisibility::Inactive);
        events[3].is_meta = Some(true);

        let html = render_html_with_events(&session("s1"), &messages, &events);

        assert!(html.contains("unknown-visibility"));
        for excluded in ["hidden", "inactive", "meta", "null-anchor", "invalid-anchor"] {
            assert!(!html.contains(&format!(">{excluded}<")));
        }
    }

    #[test]
    fn structured_timeline_deduplicates_claude_tool_only_messages_anchored_to_prior_text() {
        let mut claude_session = session("s1");
        claude_session.source = "claude-code".to_string();
        let messages = [
            message(Role::User, "Question", 0),
            message(Role::Assistant, "I will inspect it.", 1),
            message(Role::Assistant, "[Read] {\"path\":\"src/lib.rs\"}", 2),
            message(Role::User, "file body", 3),
        ];
        let mut call = event(0, "tool_call", Some(1), Some("tool-1"));
        call.name = Some("Read".to_string());
        call.target = Some("src/lib.rs".to_string());
        let result = event(1, "tool_result", Some(1), Some("tool-1"));

        let html = render_html_with_events(&claude_session, &messages, &[call, result]);

        assert_eq!(html.matches("class=\"log\"").count(), 1);
        assert_eq!(html.matches("class=\"turn user\"").count(), 1);
        assert!(!html.contains("file body"));

        let unpaired = render_html_with_events(
            &claude_session,
            &messages[..3]
                .iter()
                .cloned()
                .chain([message(Role::User, "real follow-up", 3)])
                .collect::<Vec<_>>(),
            &[event(0, "tool_call", Some(1), Some("tool-1"))],
        );
        assert_eq!(unpaired.matches("class=\"turn user\"").count(), 2);
        assert!(unpaired.contains("real follow-up"));
    }

    #[test]
    fn structured_timeline_keeps_prose_in_claude_tool_first_messages() {
        let mut claude_session = session("s1");
        claude_session.source = "claude-code".to_string();
        let messages = [
            message(Role::User, "Question", 0),
            message(
                Role::Assistant,
                "[Read] {\"path\":\"Cargo.toml\"}\nI found the manifest.\n[Read] {\"path\":\"src/lib.rs\"}",
                1,
            ),
            message(Role::User, "library body\nmanifest body", 2),
            message(Role::Assistant, "Continue.", 3),
        ];
        let mut first_call = event(0, "tool_call", Some(0), Some("tool-before"));
        first_call.name = Some("Read".to_string());
        first_call.target = Some("Cargo.toml".to_string());
        let mut second_call = event(1, "tool_call", Some(1), Some("tool-after"));
        second_call.name = Some("Read".to_string());
        second_call.target = Some("src/lib.rs".to_string());
        let second_result = event(2, "tool_result", Some(1), Some("tool-after"));
        let first_result = event(3, "tool_result", Some(1), Some("tool-before"));

        let html = render_html_with_events(
            &claude_session,
            &messages,
            &[first_call, second_call, second_result, first_result],
        );

        assert_eq!(html.matches("class=\"log\"").count(), 2);
        assert_eq!(html.matches("class=\"turn user\"").count(), 1);
        assert!(html.contains("I found the manifest."));
        assert!(html.contains("Continue."));
        assert!(!html.contains("library body"));
        assert!(!html.contains("manifest body"));
    }

    #[test]
    fn structured_timeline_preserves_user_prompt_after_cursor_inline_result() {
        let mut cursor_session = session("s1");
        cursor_session.source = "cursor".to_string();
        let messages = [
            message(
                Role::Assistant,
                "[tool:grep] {\"query\":\"needle\"}\n[tool_result:grep] found",
                0,
            ),
            message(Role::User, "Now update the file", 1),
        ];
        let mut call = event(0, "tool_call", Some(0), Some("bubble-1"));
        call.name = Some("grep".to_string());
        let result = event(1, "tool_result", Some(0), Some("bubble-1"));

        let html = render_html_with_events(&cursor_session, &messages, &[call, result]);

        assert_eq!(html.matches("class=\"log\"").count(), 1);
        assert_eq!(html.matches("class=\"turn user\"").count(), 1);
        assert!(html.contains("Now update the file"));
    }

    #[test]
    fn structured_timeline_falls_back_without_positioned_events_and_deduplicates_when_present() {
        let messages = [
            message(Role::Assistant, "Before\n[tool:Legacy] {\"path\":\"secret\"}\nAfter", 0),
            message(Role::User, "{\"ordinary\":\"json\"}", 1),
            message(Role::Assistant, "[tool:Uncovered] {\"path\":\"keep\"}", 2),
        ];
        let mut unpositioned = event(0, "tool_call", None, Some("call-1"));
        unpositioned.name = Some("Structured".to_string());
        let fallback = render_html_with_events(&session("s1"), &messages, &[unpositioned]);
        assert!(fallback.contains("Legacy"));
        assert!(fallback.contains("secret"));

        let mut positioned = event(0, "tool_call", Some(0), Some("call-1"));
        positioned.name = Some("Structured".to_string());
        positioned.target = Some("src/lib.rs".to_string());
        let structured = render_html_with_events(&session("s1"), &messages, &[positioned]);
        assert!(structured.contains("Before"));
        assert!(structured.contains("After"));
        assert!(structured.contains("Structured"));
        assert!(structured.contains("src/lib.rs"));
        assert!(!structured.contains("Legacy"));
        assert!(!structured.contains("secret"));
        assert!(structured.contains("ordinary"));
        assert!(structured.contains("Uncovered"));
        assert!(structured.contains("keep"));
        assert_eq!(structured.matches("class=\"log\"").count(), 2);
    }

    #[test]
    fn structured_timeline_escapes_public_fields_and_omits_private_payloads() {
        let messages = [message(Role::Assistant, "Answer", 0)];
        let mut call = event(0, "tool_call", Some(0), Some("hostile"));
        call.name = Some("<img src=x onerror=alert(1)>".to_string());
        call.target = Some("**[click](javascript:alert(1))</pre><script>boom</script>".to_string());
        call.status = Some("ok</pre><script>status</script>".to_string());
        call.summary = Some("[tool] raw-argument-secret".to_string());
        call.attrs_json = Some("raw-argument-secret".to_string());
        call.source_path = Some("source-path-secret".to_string());
        let mut result = event(1, "tool_result", Some(0), Some("hostile"));
        result.summary = Some("source-file-content-secret".to_string());

        let html = render_html_with_events(&session("s1"), &messages, &[call, result]);

        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(html.contains("&lt;script&gt;boom&lt;/script&gt;"));
        assert!(html.contains("&lt;script&gt;status&lt;/script&gt;"));
        assert!(!html.contains("<script>boom"));
        assert!(!html.contains("raw-argument-secret"));
        assert!(!html.contains("source-file-content-secret"));
        assert!(!html.contains("source-path-secret"));
        assert!(!html.contains("hostile"));
    }

    #[test]
    fn structured_timeline_applies_event_and_summary_budgets() {
        let messages =
            (0..300).map(|seq| message(Role::Assistant, "Answer", seq)).collect::<Vec<_>>();
        let mut events = (0..300)
            .map(|event_seq| {
                let mut event = event(event_seq, "tool_call", Some(event_seq), None);
                event.name = Some(format!("tool-{event_seq}"));
                event
            })
            .collect::<Vec<_>>();
        events[0].name = None;
        events[0].summary = Some("a".repeat(STRUCTURED_SUMMARY_CHAR_LIMIT + 20));

        let html = render_html_with_events(&session("s1"), &messages, &events);

        assert_eq!(html.matches("class=\"log\"").count(), STRUCTURED_EVENT_LIMIT);
        assert!(html.contains("Tool timeline truncated."));
        assert!(
            html.contains(&format!("Summary: {}…", "a".repeat(STRUCTURED_SUMMARY_CHAR_LIMIT - 1)))
        );
        assert!(!html.contains("tool-299"));

        let messages =
            [message(Role::Assistant, "First", 0), message(Role::Assistant, "Second", 1)];
        let mut grouped_events = (0..255)
            .map(|event_seq| event(event_seq, "tool_call", Some(0), None))
            .collect::<Vec<_>>();
        let mut excluded_call = event(255, "tool_call", Some(1), Some("pair"));
        excluded_call.name = Some("excluded-call".to_string());
        grouped_events.push(excluded_call);
        grouped_events.push(event(256, "tool_result", Some(1), Some("pair")));
        let grouped = render_html_with_events(&session("s1"), &messages, &grouped_events);
        assert_eq!(grouped.matches("class=\"log\"").count(), 255);
        assert!(!grouped.contains("excluded-call"));
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
    fn local_preview_has_no_render_blocking_network_resources() {
        let html =
            render_session_preview_html(&session("s1"), &[], &[], &SessionDisplayMeta::default());

        assert!(!html.contains("fonts.googleapis.com"));
        assert!(!html.contains("fonts.gstatic.com"));
        assert!(render_html(&session("s1"), &[]).contains("fonts.googleapis.com"));
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
            &[],
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
