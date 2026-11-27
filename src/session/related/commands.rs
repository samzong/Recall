fn command_basename_lower(token: &str) -> String {
    token.rsplit(['/', '\\']).next().unwrap_or(token).to_ascii_lowercase()
}

fn match_flag_value<'a>(
    tokens: &'a [String],
    cmd_idx: usize,
    flag: &str,
    equals_prefix: Option<&str>,
) -> Option<(&'a str, usize)> {
    let limit = (cmd_idx + 6).min(tokens.len());
    let mut j = cmd_idx + 1;
    while j < limit {
        let tok = &tokens[j];
        if tok == "&&" || tok == "||" || tok == "|" {
            break;
        }
        if let Some(prefix) = equals_prefix
            && let Some(id) = tok.strip_prefix(prefix)
        {
            return Some((id, j + 1 - cmd_idx));
        }
        if tok == flag {
            return tokens.get(j + 1).map(|id| (id.as_str(), j + 2 - cmd_idx));
        }
        j += 1;
    }
    None
}

pub(super) fn parse_resume_commands_in_text(text: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    for segment in text.split(['\n', ';', '|']) {
        let tokens = tokenize_command_text(segment);
        let mut i = 0usize;
        while i < tokens.len() {
            let cmd = command_basename_lower(&tokens[i]);
            let matched = match cmd.as_str() {
                "claude" => match_flag_value(&tokens, i, "--resume", Some("--resume="))
                    .map(|(id, step)| ("claude-code", id, step)),
                "codex" => match_flag_value(&tokens, i, "resume", None)
                    .map(|(id, step)| ("codex", id, step)),
                "agy" => match_flag_value(&tokens, i, "--conversation", Some("--conversation="))
                    .map(|(id, step)| ("antigravity-cli", id, step)),
                "copilot" => match_flag_value(&tokens, i, "--resume", Some("--resume="))
                    .map(|(id, step)| ("copilot-cli", id, step)),
                "kimi" => match_flag_value(&tokens, i, "--session", Some("--session="))
                    .map(|(id, step)| ("kimi-code", id, step)),
                "rx" | "rx-agent" => match_flag_value(&tokens, i, "--resume", Some("--resume="))
                    .map(|(id, step)| ("rx-agent", id, step)),
                "cursor-agent" | "agent" => {
                    match_flag_value(&tokens, i, "--resume", Some("--resume="))
                        .map(|(id, step)| ("cursor", id, step))
                }
                "grok" => match_flag_value(&tokens, i, "--resume", Some("--resume="))
                    .map(|(id, step)| ("grok", id, step)),
                "devin" => match_flag_value(&tokens, i, "--resume", Some("--resume="))
                    .map(|(id, step)| ("devin", id, step)),
                "opencode" => match_flag_value(&tokens, i, "--session", Some("--session="))
                    .map(|(id, step)| ("opencode", id, step)),
                _ => None,
            };
            if let Some((source, raw_id, step)) = matched {
                let cleaned = clean_session_id_token(raw_id);
                if is_valid_native_session_id(&cleaned) {
                    let pair = (source.to_string(), cleaned);
                    if !results.contains(&pair) {
                        results.push(pair);
                    }
                }
                i += step;
            } else {
                i += 1;
            }
        }
    }
    results
}

fn tokenize_command_text(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_whitespace()
            || matches!(ch, '"' | '\'' | '`' | ',' | ';' | '{' | '}' | '[' | ']' | '(' | ')')
        {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn clean_session_id_token(raw: &str) -> String {
    raw.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ',' | ';' | '\\' | ')' | ']' | '}'))
        .to_string()
}

fn is_valid_native_session_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.starts_with('<')
        && !id.ends_with('>')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}
