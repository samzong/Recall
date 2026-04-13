pub fn format_age(started_at: i64) -> String {
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

pub fn parse_since(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('d') {
        (n, 24 * 3600 * 1000i64)
    } else if let Some(n) = s.strip_suffix('w') {
        (n, 7 * 24 * 3600 * 1000i64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 30 * 24 * 3600 * 1000i64)
    } else {
        return None;
    };
    let n: i64 = num_str.parse().ok()?;
    let now = chrono::Utc::now().timestamp_millis();
    Some(now - n * multiplier)
}

pub fn sanitize_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
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

pub fn format_message_time(ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return String::new();
    };
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

pub fn f32_slice_to_bytes(data: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for &f in data {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

const TITLE_MAX_CHARS: usize = 80;
const TITLE_TRUNCATE_TAIL: usize = 77;

pub fn title_from_user_messages(user_contents: &[&str]) -> String {
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
}
