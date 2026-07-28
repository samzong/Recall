use std::io::Write as _;
use std::process::{Command, Stdio};

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

pub(crate) fn clipboard_candidates() -> &'static [(&'static str, &'static [&'static str])] {
    if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "windows") {
        &[("clip.exe", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    }
}

fn try_clipboard_candidates(candidates: &[(&str, &[&str])], text: &str) -> anyhow::Result<()> {
    let mut last_error = None;
    for (program, args) in candidates {
        let mut child = match Command::new(program).args(*args).stdin(Stdio::piped()).spawn() {
            Ok(child) => child,
            Err(error) => {
                last_error = Some(error.into());
                continue;
            }
        };
        let write_error = match child.stdin.take() {
            Some(mut stdin) => stdin.write_all(text.as_bytes()).err().map(anyhow::Error::from),
            None => Some(anyhow::anyhow!("{program} stdin unavailable")),
        };
        let status = child.wait();
        if let Some(error) = write_error {
            last_error = Some(anyhow::anyhow!("{program} stdin write failed: {error}"));
            continue;
        }
        match status {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                last_error = Some(anyhow::anyhow!("{program} exited with status {status}"));
            }
            Err(error) => {
                last_error = Some(anyhow::anyhow!("{program} wait failed: {error}"));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no clipboard utility found")))
}

pub(crate) fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    try_clipboard_candidates(clipboard_candidates(), text).map_err(|error| {
        anyhow::anyhow!(
            "Failed to copy to clipboard (install wl-clipboard, xclip, or xsel): {error}"
        )
    })
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

pub(crate) fn sanitize_line(line: &str) -> String {
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
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn clipboard_candidates_fall_back_wl_copy_then_xclip_then_xsel_on_linux() {
        let candidates = clipboard_candidates();

        assert_eq!(
            candidates,
            &[
                ("wl-copy", &[][..]),
                ("xclip", &["-selection", "clipboard"][..]),
                ("xsel", &["--clipboard", "--input"][..]),
            ]
        );
    }

    #[test]
    #[cfg(unix)]
    fn clipboard_candidates_fall_through_after_spawn_failure() {
        let candidates: &[(&str, &[&str])] = &[
            ("recall-test-nonexistent-clipboard-tool-xyz", &[]),
            ("sh", &["-c", "cat >/dev/null; exit 0"]),
        ];

        assert!(try_clipboard_candidates(candidates, "hello").is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn clipboard_candidates_fall_through_after_nonzero_exit() {
        let candidates: &[(&str, &[&str])] =
            &[("sh", &["-c", "cat >/dev/null; exit 1"]), ("sh", &["-c", "cat >/dev/null; exit 0"])];

        assert!(try_clipboard_candidates(candidates, "hello").is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn clipboard_candidates_reject_partial_stdin_write() {
        let candidates: &[(&str, &[&str])] = &[
            ("sh", &["-c", "dd bs=1 count=1 of=/dev/null 2>/dev/null"]),
            ("sh", &["-c", "cat >/dev/null; exit 1"]),
        ];
        let text = "x".repeat(1024 * 1024);

        assert!(try_clipboard_candidates(candidates, &text).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn clipboard_candidates_error_when_all_fail() {
        let candidates: &[(&str, &[&str])] = &[("sh", &["-c", "cat >/dev/null; exit 1"])];

        assert!(try_clipboard_candidates(candidates, "hello").is_err());
    }

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
