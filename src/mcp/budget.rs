use crate::db::message_store::{json_escaped_char_bytes, json_escaped_str_bytes};

pub(super) const MCP_INLINE_MAX_BYTES: usize = 8_800;

pub(super) fn sanitize_lossy_text(input: &str) -> String {
    if !input.contains('\x1b') && !input.contains('\r') {
        return input.to_string();
    }
    let stripped = if input.contains('\x1b') {
        std::borrow::Cow::Owned(strip_ansi(input))
    } else {
        std::borrow::Cow::Borrowed(input)
    };
    if !stripped.contains('\r') {
        return stripped.into_owned();
    }
    let normalized = stripped.replace("\r\n", "\n");
    if !normalized.contains('\r') {
        return normalized;
    }
    normalized
        .split('\n')
        .map(|line| {
            if line.contains('\r') {
                line.rsplit('\r').find(|frame| !frame.trim().is_empty()).unwrap_or("")
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                while let Some(&next) = chars.peek() {
                    if matches!(next as u32, 0x20..=0x3f) {
                        chars.next();
                    } else {
                        if matches!(next as u32, 0x40..=0x7e) {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            Some(']' | 'P' | 'X' | '^' | '_') => {
                chars.next();
                while let Some(&next) = chars.peek() {
                    if next == '\x07' {
                        chars.next();
                        break;
                    }
                    if next == '\n' {
                        break;
                    }
                    if next == '\x1b' {
                        chars.next();
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    chars.next();
                }
            }
            Some(next) if matches!(next as u32, 0x20..=0x2f) => {
                chars.next();
                while let Some(&inter) = chars.peek() {
                    if matches!(inter as u32, 0x20..=0x2f) {
                        chars.next();
                    } else {
                        if matches!(inter as u32, 0x30..=0x7e) {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            Some(next) if matches!(next as u32, 0x30..=0x7e) => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

pub(super) fn truncate_json_bytes(text: &str, max_bytes: usize) -> String {
    if json_escaped_str_bytes(text) <= max_bytes {
        return text.to_string();
    }
    if max_bytes < 3 {
        return String::new();
    }
    let content_budget = max_bytes - 3;
    let mut used = 0usize;
    let mut kept_utf8 = 0usize;
    for ch in text.chars() {
        let cost = json_escaped_char_bytes(ch);
        if used + cost > content_budget {
            break;
        }
        used += cost;
        kept_utf8 += ch.len_utf8();
    }
    let mut out = text[..kept_utf8].trim_end_matches('…').to_string();
    out.push('…');
    out
}

pub(super) fn water_fill_strings(items: &mut [String], available_bytes: usize) {
    if items.is_empty() {
        return;
    }
    let mut entries: Vec<(usize, usize)> = items
        .iter()
        .enumerate()
        .map(|(index, item)| (index, json_escaped_str_bytes(item)))
        .collect();
    let total: usize = entries.iter().map(|(_, cost)| *cost).sum();
    if total <= available_bytes {
        return;
    }
    entries.sort_by_key(|&(index, cost)| (cost, index));
    let count = entries.len();
    let mut remaining = available_bytes;
    for (pos, &(index, cost)) in entries.iter().enumerate() {
        let left = count - pos;
        let share = remaining / left;
        if cost <= share {
            remaining -= cost;
        } else {
            let truncated = truncate_json_bytes(&items[index], share);
            let used = json_escaped_str_bytes(&truncated);
            items[index] = truncated;
            remaining = remaining.saturating_sub(used);
        }
    }
}

pub(super) fn water_fill_optional(items: &mut [Option<String>], available_bytes: usize) {
    let mut indices = Vec::new();
    let mut values = Vec::new();
    for (index, item) in items.iter_mut().enumerate() {
        if let Some(value) = item.take() {
            indices.push(index);
            values.push(value);
        }
    }
    water_fill_strings(&mut values, available_bytes);
    for (index, value) in indices.into_iter().zip(values) {
        items[index] = Some(value);
    }
}

pub(super) fn inline_page_budget(limit: Option<u32>, default_limit: u32, max_limit: u32) -> usize {
    match limit {
        None => MCP_INLINE_MAX_BYTES,
        Some(requested) if requested <= default_limit => MCP_INLINE_MAX_BYTES,
        Some(requested) => {
            let clamped = requested.clamp(1, max_limit) as usize;
            MCP_INLINE_MAX_BYTES.saturating_mul(clamped) / (default_limit as usize)
        }
    }
}
