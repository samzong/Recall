use std::process::Command;

use crate::types::Role;

pub(crate) fn open_url_in_default_browser(url: &str) -> anyhow::Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let status = Command::new(program).args(args).status()?;
    if !status.success() {
        anyhow::bail!("{program} exited with status {status}");
    }
    Ok(())
}

pub(crate) fn format_age(started_at: i64) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let diff_hours = (now - started_at) / (1000 * 3600);
    if diff_hours < 1 {
        "<1h".to_string()
    } else if diff_hours < 24 {
        format!("{diff_hours}h")
    } else {
        let days = diff_hours / 24;
        if days < 30 {
            format!("{days}d")
        } else {
            let months = days / 30;
            format!("{months}mo")
        }
    }
}

pub(crate) fn parse_since(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('d') {
        (n, 24 * 3600 * 1000i64)
    } else if let Some(n) = s.strip_suffix('w') {
        (n, 7 * 24 * 3600 * 1000i64)
    } else {
        let n = s.strip_suffix('m')?;
        (n, 30 * 24 * 3600 * 1000i64)
    };
    let n: i64 = num_str.parse().ok()?;
    let now = chrono::Utc::now().timestamp_millis();
    Some(now - n * multiplier)
}

/// Consumes a CSI parameter/intermediate tail through its final byte
/// (0x40..=0x7e). Returns whether it was terminated before EOF. Shared by
/// the `ESC [` and 8-bit `U+009B` (CSI) introducers.
fn consume_csi_tail(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    for b in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&b) {
            return true;
        }
    }
    false
}

/// Consumes an OSC string tail, terminated by BEL (0x07), ST (`ESC \`), or
/// the 8-bit C1 ST (`U+009C`). Returns whether it was terminated before EOF.
/// Shared by the `ESC ]` and 8-bit `U+009D` (OSC) introducers.
fn consume_osc_tail(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    while let Some(b) = chars.next() {
        if b == '\u{07}' || b == '\u{9c}' {
            return true;
        }
        if b == '\u{1b}' {
            // possible ST: consume the following '\'
            if chars.peek() == Some(&'\\') {
                chars.next();
            }
            return true;
        }
    }
    false
}

/// Consumes a DCS/SOS/PM/APC string tail, terminated by ST (`ESC \`) or the
/// 8-bit C1 ST (`U+009C`). Returns whether it was terminated before EOF.
/// Shared by the `ESC P`/`X`/`^`/`_` and 8-bit `U+0090`/`U+0098`/`U+009E`/
/// `U+009F` introducers.
fn consume_string_tail(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    while let Some(b) = chars.next() {
        if b == '\u{9c}' {
            return true;
        }
        if b == '\u{1b}' {
            if chars.peek() == Some(&'\\') {
                chars.next();
            }
            return true;
        }
    }
    false
}

pub(crate) fn strip_ansi_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // Saw ESC. Dispatch on the introducer byte.
                let Some(&intro) = chars.peek() else {
                    break; // lone trailing ESC -> drop (D6)
                };
                match intro {
                    '[' => {
                        // CSI: ESC [ params/intermediates final(0x40..=0x7e)
                        chars.next();
                        if !consume_csi_tail(&mut chars) {
                            break; // unterminated CSI at EOF -> drop remainder (D6)
                        }
                    }
                    ']' => {
                        // OSC: ESC ] ... terminated by BEL (0x07) or ST (ESC \)
                        chars.next();
                        if !consume_osc_tail(&mut chars) {
                            break; // unterminated OSC -> drop remainder (D6)
                        }
                    }
                    'P' | 'X' | '^' | '_' => {
                        // DCS/SOS/PM/APC string: terminated by ST (ESC \)
                        chars.next();
                        if !consume_string_tail(&mut chars) {
                            break; // unterminated string sequence -> drop remainder (D6)
                        }
                    }
                    _ => {
                        // 2-byte escape (ESC + single byte), e.g. ESC c, ESC 7, SS2/SS3 (D12)
                        chars.next();
                    }
                }
            }
            '\u{9b}' => {
                // 8-bit CSI introducer, equivalent to ESC [
                if !consume_csi_tail(&mut chars) {
                    break; // unterminated CSI at EOF -> drop remainder (D6)
                }
            }
            '\u{9d}' => {
                // 8-bit OSC introducer, equivalent to ESC ]
                if !consume_osc_tail(&mut chars) {
                    break; // unterminated OSC -> drop remainder (D6)
                }
            }
            '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => {
                // 8-bit DCS/SOS/PM/APC introducers, equivalent to ESC P/X/^/_
                if !consume_string_tail(&mut chars) {
                    break; // unterminated string sequence -> drop remainder (D6)
                }
            }
            _ => out.push(c),
        }
    }
    out
}

pub(crate) fn sanitize_line(line: &str) -> String {
    let stripped = strip_ansi_sequences(line);
    let mut out = String::with_capacity(stripped.len());
    for c in stripped.chars() {
        if c == '\t' {
            out.push_str("    ");
        } else if c.is_control() {
            continue;
        } else {
            out.push(c);
        }
    }
    out
}

pub(crate) fn role_label(source: &str, role: &Role) -> &'static str {
    match role {
        Role::User => "You",
        Role::Assistant => match source {
            "claude-code" => "Claude",
            "opencode" => "OpenCode",
            "codex" => "Codex",
            "pi" => "Pi",
            "antigravity-cli" => "Antigravity",
            "gemini-cli" => "Gemini",
            "grok" => "Grok",
            "kiro-cli" => "Kiro",
            "copilot-cli" => "Copilot",
            "cursor" => "Cursor",
            "cline" => "Cline",
            _ => "Asst",
        },
    }
}

pub(crate) fn project_label(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    match parts.len() {
        0 => path.to_string(),
        1 => parts[0].to_string(),
        len => format!("{}/{}", parts[len - 2], parts[len - 1]),
    }
}

pub(crate) fn format_started_calendar(started_at: i64) -> String {
    chrono::DateTime::from_timestamp_millis(started_at)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

pub(crate) fn format_message_time(ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return String::new();
    };
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

pub(crate) fn f32_slice_to_bytes(data: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for &f in data {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

const TITLE_MAX_CHARS: usize = 80;
const TITLE_TRUNCATE_TAIL: usize = 77;

pub(crate) fn title_from_user_messages(user_contents: &[&str]) -> String {
    let chosen = user_contents
        .iter()
        .copied()
        .find(|c| !is_noise_first_message(c))
        .or_else(|| user_contents.first().copied())
        .unwrap_or("");

    let trimmed = chosen.trim();
    if trimmed.is_empty() {
        return "Untitled".to_string();
    }
    if trimmed.chars().count() > TITLE_MAX_CHARS {
        let truncated: String = trimmed.chars().take(TITLE_TRUNCATE_TAIL).collect();
        format!("{truncated}...")
    } else {
        trimmed.to_string()
    }
}

fn is_noise_first_message(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with("<command-message>")
        || trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("# New session -")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_empty_input_returns_untitled() {
        assert_eq!(title_from_user_messages(&[]), "Untitled");
    }

    #[test]
    fn title_single_plain_message_returned_verbatim() {
        assert_eq!(title_from_user_messages(&["fix the parser bug"]), "fix the parser bug");
    }

    #[test]
    fn title_trims_whitespace() {
        assert_eq!(title_from_user_messages(&["  hello world  "]), "hello world");
    }

    #[test]
    fn title_long_message_is_truncated_with_ellipsis() {
        let long = "a".repeat(200);
        let result = title_from_user_messages(&[&long]);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 80);
    }

    #[test]
    fn title_skips_command_message_noise() {
        let msgs = [
            "<command-message>ship</command-message>\n<command-name>/ship</command-name>",
            "actually implement the feature",
        ];
        assert_eq!(title_from_user_messages(&msgs), "actually implement the feature");
    }

    #[test]
    fn title_skips_local_command_caveat_noise() {
        let msgs = [
            "<local-command-caveat>Caveat: ignore this wrapper</local-command-caveat>",
            "real intent here",
        ];
        assert_eq!(title_from_user_messages(&msgs), "real intent here");
    }

    #[test]
    fn title_skips_opencode_new_session_header() {
        let msgs = [
            "# New session - 2026-04-08T03:29:50.987Z\n\n**Session ID:** ses_abc",
            "debug the sync pipeline",
        ];
        assert_eq!(title_from_user_messages(&msgs), "debug the sync pipeline");
    }

    #[test]
    fn title_skips_multiple_noise_messages_in_a_row() {
        let msgs = [
            "<command-message>ship</command-message>",
            "<command-message>review</command-message>",
            "explain the regression",
        ];
        assert_eq!(title_from_user_messages(&msgs), "explain the regression");
    }

    #[test]
    fn title_falls_back_to_first_when_all_are_noise() {
        let msgs = [
            "<command-message>ship</command-message>",
            "<command-message>review</command-message>",
        ];
        assert_eq!(title_from_user_messages(&msgs), "<command-message>ship</command-message>");
    }

    #[test]
    fn title_does_not_misclassify_plain_markdown_heading() {
        let msgs = ["# Design notes\nthinking about the search pipeline"];
        let result = title_from_user_messages(&msgs);
        assert!(result.starts_with("# Design notes"));
    }

    #[test]
    fn title_detects_noise_with_leading_whitespace() {
        let msgs = ["   <command-message>ship</command-message>", "real content"];
        assert_eq!(title_from_user_messages(&msgs), "real content");
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc_leaving_text() {
        assert_eq!(strip_ansi_sequences("\x1b[31mred\x1b[0m\x1b]0;title\x07"), "red");
    }

    #[test]
    fn strip_ansi_preserves_plain_text_tabs_and_newlines() {
        assert_eq!(strip_ansi_sequences("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn strip_ansi_handles_osc_st_terminator() {
        // OSC terminated by ST (ESC \) instead of BEL
        assert_eq!(strip_ansi_sequences("x\x1b]0;title\x1b\\y"), "xy");
    }

    #[test]
    fn strip_ansi_drops_unterminated_csi_remainder() {
        assert_eq!(strip_ansi_sequences("ok\x1b[31"), "ok");
    }

    #[test]
    fn strip_ansi_drops_unterminated_osc_remainder() {
        assert_eq!(strip_ansi_sequences("ok\x1b]0;never"), "ok");
    }

    #[test]
    fn strip_ansi_drops_lone_trailing_esc() {
        assert_eq!(strip_ansi_sequences("ok\x1b"), "ok");
    }

    #[test]
    fn strip_ansi_drops_two_byte_escape() {
        // ESC c (RIS reset) is a 2-byte escape; both bytes go, rest stays
        assert_eq!(strip_ansi_sequences("a\x1bcb"), "ab");
    }

    #[test]
    fn strip_ansi_drops_dcs_string_through_st() {
        assert_eq!(strip_ansi_sequences("\x1bPq;data\x1b\\end"), "end");
    }

    #[test]
    fn strip_ansi_handles_c1_csi_introducer() {
        // U+009B is the 8-bit CSI introducer, equivalent to ESC [
        assert_eq!(strip_ansi_sequences("\u{9b}31mred\u{9b}0m"), "red");
    }

    #[test]
    fn strip_ansi_handles_c1_osc_introducer_bel_terminated() {
        // U+009D is the 8-bit OSC introducer, equivalent to ESC ]
        assert_eq!(strip_ansi_sequences("a\u{9d}0;title\u{07}b"), "ab");
    }

    #[test]
    fn strip_ansi_handles_c1_osc_introducer_c1_st_terminated() {
        // U+009C is the 8-bit ST, valid terminator alongside ESC \
        assert_eq!(strip_ansi_sequences("a\u{9d}0;title\u{9c}b"), "ab");
    }

    #[test]
    fn strip_ansi_handles_c1_dcs_introducer_esc_st_terminated() {
        // U+0090 is the 8-bit DCS introducer, equivalent to ESC P
        assert_eq!(strip_ansi_sequences("\u{90}payload\u{1b}\\x"), "x");
    }

    #[test]
    fn strip_ansi_drops_unterminated_c1_csi_remainder() {
        assert_eq!(strip_ansi_sequences("a\u{9b}31"), "a");
    }

    #[test]
    fn sanitize_line_strips_escape_residue_and_expands_tabs() {
        assert_eq!(sanitize_line("\x1b[31mred\x1b[0m\tX"), "red    X");
    }

    #[test]
    fn role_label_user_is_you_for_any_source() {
        assert_eq!(role_label("claude-code", &Role::User), "You");
        assert_eq!(role_label("totally-unknown", &Role::User), "You");
    }

    #[test]
    fn role_label_maps_each_assistant_source() {
        let cases = [
            ("claude-code", "Claude"),
            ("opencode", "OpenCode"),
            ("codex", "Codex"),
            ("pi", "Pi"),
            ("antigravity-cli", "Antigravity"),
            ("gemini-cli", "Gemini"),
            ("grok", "Grok"),
            ("kiro-cli", "Kiro"),
            ("copilot-cli", "Copilot"),
            ("cursor", "Cursor"),
            ("cline", "Cline"),
        ];
        for (src, expected) in cases {
            assert_eq!(role_label(src, &Role::Assistant), expected, "source {src}");
        }
    }

    #[test]
    fn role_label_unknown_assistant_falls_back_to_asst() {
        assert_eq!(role_label("mystery-cli", &Role::Assistant), "Asst");
    }

    #[test]
    fn project_label_returns_last_two_components() {
        assert_eq!(project_label("/home/u/dev/recall"), "dev/recall");
        assert_eq!(project_label("recall"), "recall");
        assert_eq!(project_label(""), "");
    }

    #[test]
    fn format_started_calendar_returns_iso_date_shape() {
        let out = format_started_calendar(1_700_000_000_000);
        assert!(!out.is_empty());
        assert!(out.contains('-'));
    }
}
