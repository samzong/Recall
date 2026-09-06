use std::path::Path;

use serde_json::Value;

use crate::types::{CommandEvidenceStatus, FileEvidence, FileEvidenceKind, FileOperation};

const MAX_BYTES: usize = 1_048_576;
const MAX_TOKENS: usize = 32_768;
const MAX_FILES: usize = 256;

pub(crate) fn command_file_evidence(
    name: &str,
    args: Option<&Value>,
    cwd: Option<&str>,
) -> (Vec<FileEvidence>, CommandEvidenceStatus) {
    let mut scan = Scan { files: Vec::new(), status: CommandEvidenceStatus::Complete };
    match name {
        "exec" | "functions.exec" => match args.and_then(Value::as_str) {
            Some(script) => scan.javascript(script),
            None => scan.unsupported(),
        },
        "exec_command" | "functions.exec_command" => match args {
            Some(args) => scan.command_args(args, cwd),
            None => scan.unsupported(),
        },
        _ => scan.unsupported(),
    }
    (scan.files, scan.status)
}

pub(crate) fn shell_file_evidence(
    command: &str,
    cwd: Option<&str>,
) -> (Vec<FileEvidence>, CommandEvidenceStatus) {
    let mut scan = Scan { files: Vec::new(), status: CommandEvidenceStatus::Complete };
    scan.shell(command, cwd);
    (scan.files, scan.status)
}

struct Scan {
    files: Vec<FileEvidence>,
    status: CommandEvidenceStatus,
}

impl Scan {
    fn unsupported(&mut self) {
        if self.status != CommandEvidenceStatus::LimitExceeded {
            self.status = CommandEvidenceStatus::Unsupported;
        }
    }

    fn add(&mut self, mut files: Vec<FileEvidence>, cwd: Option<&str>) {
        for file in &mut files {
            file.kind = FileEvidenceKind::Command;
            file.cwd = cwd.map(str::to_string);
            if cwd.is_none() && !Path::new(&file.path).is_absolute() {
                self.unsupported();
            }
        }
        let remaining = MAX_FILES.saturating_sub(self.files.len());
        if files.len() > remaining {
            files.truncate(remaining);
            self.status = CommandEvidenceStatus::LimitExceeded;
        }
        self.files.extend(files);
    }

    fn path(&mut self, path: &str, operation: FileOperation, cwd: Option<&str>) {
        if path.trim().is_empty()
            || path.starts_with('-')
            || path.contains(['$', '*', '?', '[', '`'])
        {
            self.unsupported();
            return;
        }
        self.add(
            vec![FileEvidence {
                path: path.into(),
                operation,
                kind: FileEvidenceKind::Command,
                cwd: None,
                target: None,
            }],
            cwd,
        );
    }

    fn javascript(&mut self, script: &str) {
        let tokens = match js_tokens(script) {
            Ok(tokens) => tokens,
            Err(status) => {
                self.status = status;
                return;
            }
        };
        self.unsupported();
        for (i, token) in tokens.iter().enumerate() {
            if let Some(value) = token.literal() {
                let files = super::patch_file_evidence(value);
                if !files.is_empty() {
                    self.add(files, None);
                }
            }
            if token.word() == Some("tools")
                && tokens.get(i + 1) == Some(&Token::Punct('.'))
                && tokens.get(i + 2).and_then(Token::word) == Some("exec_command")
                && tokens.get(i + 3) == Some(&Token::Punct('('))
            {
                match static_object(&tokens[i + 4..]) {
                    Some(args) => self.command_args(&args, None),
                    None => self.unsupported(),
                }
            }
        }
    }

    fn command_args(&mut self, args: &Value, default_cwd: Option<&str>) {
        let Some(command) = args.get("cmd").and_then(Value::as_str) else {
            self.unsupported();
            return;
        };
        let cwd = match args.get("workdir") {
            Some(value) => value.as_str(),
            None => default_cwd,
        };
        let cwd = cwd.filter(|path| Path::new(path).is_absolute());
        if args.get("workdir").is_some() && cwd.is_none() {
            self.unsupported();
        }
        self.shell(command, cwd);
    }

    fn shell(&mut self, command: &str, cwd: Option<&str>) {
        if command.len() > MAX_BYTES {
            self.status = CommandEvidenceStatus::LimitExceeded;
            return;
        }
        let mut cwd = cwd.map(str::to_string);
        let mut lines = command.lines();
        let mut remaining = MAX_TOKENS;
        let mut conditional_cwd = false;
        while let Some(line) = lines.next() {
            let tokens = match shell_words(line) {
                Some(tokens) => tokens,
                None => {
                    self.unsupported();
                    cwd = None;
                    continue;
                }
            };
            if tokens.len() > remaining {
                self.status = CommandEvidenceStatus::LimitExceeded;
                return;
            }
            remaining -= tokens.len();
            let mut parts =
                tokens.split_inclusive(|word| matches!(word.as_str(), ";" | "&&" | "||" | "|"));
            for part in &mut parts {
                let separator = part
                    .last()
                    .map(String::as_str)
                    .filter(|word| matches!(*word, ";" | "&&" | "||" | "|"));
                let words = if separator.is_some() { &part[..part.len() - 1] } else { part };
                if words.is_empty() {
                    continue;
                }
                if let Some(index) = words.iter().position(|word| word == "<<") {
                    let Some(delimiter) = words.get(index + 1) else {
                        self.unsupported();
                        return;
                    };
                    let mut body = String::new();
                    let mut terminated = false;
                    for line in lines.by_ref() {
                        if line == delimiter {
                            terminated = true;
                            break;
                        }
                        body.push_str(line);
                        body.push('\n');
                    }
                    if !terminated || separator.is_some() {
                        self.unsupported();
                    } else if words[0] == "apply_patch" && index == 1 && words.len() == 3 {
                        if body.contains(['$', '`'])
                            && !line.rsplit_once("<<").is_some_and(|(_, delimiter)| {
                                delimiter.trim_start().starts_with(['\'', '"'])
                            })
                        {
                            self.unsupported();
                        } else {
                            let files = super::patch_file_evidence(&body);
                            if files.is_empty() {
                                self.unsupported();
                            }
                            self.add(files, cwd.as_deref());
                        }
                    }
                    if words[0] == "apply_patch" && (index != 1 || words.len() != 3) {
                        self.unsupported();
                    }
                    if !matches!(words[0].as_str(), "apply_patch" | "cat") {
                        self.unsupported();
                        cwd = None;
                    }
                    if words[0] == "cat" && terminated && separator.is_none() {
                        let target = match words {
                            [_, redirect, target, heredoc, _]
                                if matches!(redirect.as_str(), ">" | ">>") && heredoc == "<<" =>
                            {
                                Some(target)
                            }
                            [_, heredoc, _, redirect, target]
                                if heredoc == "<<" && matches!(redirect.as_str(), ">" | ">>") =>
                            {
                                Some(target)
                            }
                            _ => None,
                        };
                        if let Some(target) = target {
                            self.path(target, FileOperation::Write, cwd.as_deref());
                        } else if words.len() != 3 || index != 1 {
                            self.unsupported();
                        }
                        if body.contains(['$', '`'])
                            && !line
                                .rsplit_once("<<")
                                .is_some_and(|(_, tail)| tail.trim_start().starts_with(['\'', '"']))
                        {
                            self.unsupported();
                        }
                    }
                    if conditional_cwd {
                        cwd = None;
                    }
                    continue;
                }
                if words.iter().any(|word| matches!(word.as_str(), ">" | ">>")) {
                    self.unsupported();
                    continue;
                }
                match words[0].as_str() {
                    "cd" => {
                        conditional_cwd = true;
                        cwd = words
                            .get(1)
                            .filter(|path| {
                                words.len() == 2
                                    && separator == Some("&&")
                                    && Path::new(path).is_absolute()
                            })
                            .cloned();
                        if cwd.is_none() {
                            self.unsupported();
                        }
                    }
                    "git" => self.git(&words[1..], cwd.as_deref()),
                    "mv" => {
                        let operands =
                            words[1..].strip_prefix(&["--".to_string()]).unwrap_or(&words[1..]);
                        if operands.len() == 2 {
                            self.path(&operands[0], FileOperation::MoveFrom, cwd.as_deref());
                            self.path(&operands[1], FileOperation::MoveTo, cwd.as_deref());
                        } else {
                            self.unsupported();
                        }
                    }
                    "echo" | "printf" | "true" | "false" => {}
                    _ => {
                        self.unsupported();
                        cwd = None;
                    }
                }
                if conditional_cwd && separator != Some("&&") {
                    cwd = None;
                }
                if separator == Some("|") {
                    cwd = None;
                    self.unsupported();
                }
            }
        }
    }

    fn git(&mut self, mut words: &[String], cwd: Option<&str>) {
        let mut cwd = cwd;
        if words.first().map(String::as_str) == Some("-C") {
            cwd = words.get(1).map(String::as_str).filter(|path| Path::new(path).is_absolute());
            if cwd.is_none() || words.len() < 3 {
                self.unsupported();
                return;
            }
            words = &words[2..];
        }
        let Some(operation) = words.first().map(String::as_str) else {
            self.unsupported();
            return;
        };
        if !matches!(operation, "restore" | "checkout") {
            self.unsupported();
            return;
        }
        let Some(index) = words.iter().position(|word| word == "--") else {
            self.unsupported();
            return;
        };
        let options = &words[1..index];
        if options.iter().any(|word| matches!(word.as_str(), "--staged" | "-S"))
            && !options.iter().any(|word| matches!(word.as_str(), "--worktree" | "-W"))
        {
            return;
        }
        if options.iter().any(|word| {
            word.starts_with('-')
                && !matches!(word.as_str(), "--staged" | "-S" | "--worktree" | "-W")
        }) {
            self.unsupported();
        }
        if index + 1 == words.len() {
            self.unsupported();
        }
        for path in &words[index + 1..] {
            if path.starts_with(":(") || matches!(path.as_str(), "." | "..") || path.ends_with('/')
            {
                self.unsupported();
            } else {
                self.path(path, FileOperation::Write, cwd);
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Literal(String),
    Punct(char),
}

impl Token {
    fn word(&self) -> Option<&str> {
        match self {
            Self::Word(value) => Some(value),
            _ => None,
        }
    }

    fn literal(&self) -> Option<&str> {
        match self {
            Self::Literal(value) => Some(value),
            _ => None,
        }
    }
}

fn js_tokens(script: &str) -> Result<Vec<Token>, CommandEvidenceStatus> {
    if script.len() > MAX_BYTES {
        return Err(CommandEvidenceStatus::LimitExceeded);
    }
    let mut chars = script.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        let token = if matches!(ch, '\'' | '"' | '`') {
            let mut literal = String::new();
            let mut terminated = false;
            while let Some(next) = chars.next() {
                if next == ch {
                    terminated = true;
                    break;
                }
                if ch == '`' && next == '$' && chars.peek() == Some(&'{') {
                    return Err(CommandEvidenceStatus::Unsupported);
                }
                if next != '\\' {
                    literal.push(next);
                    continue;
                }
                match chars.next() {
                    Some('n') => literal.push('\n'),
                    Some('r') => literal.push('\r'),
                    Some('t') => literal.push('\t'),
                    Some('\\') => literal.push('\\'),
                    Some('\'') => literal.push('\''),
                    Some('"') => literal.push('"'),
                    Some('`') => literal.push('`'),
                    Some('$') => literal.push('$'),
                    Some('\n') => {}
                    _ => return Err(CommandEvidenceStatus::Unsupported),
                }
            }
            if !terminated {
                return Err(CommandEvidenceStatus::Unsupported);
            }
            Token::Literal(literal)
        } else if ch == '/' {
            match chars.next() {
                Some('/') => {
                    for next in chars.by_ref() {
                        if next == '\n' {
                            break;
                        }
                    }
                }
                Some('*') => {
                    let mut terminated = false;
                    while let Some(next) = chars.next() {
                        if next == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            terminated = true;
                            break;
                        }
                    }
                    if !terminated {
                        return Err(CommandEvidenceStatus::Unsupported);
                    }
                }
                _ => return Err(CommandEvidenceStatus::Unsupported),
            }
            continue;
        } else if ch.is_alphanumeric() || matches!(ch, '_' | '$') {
            let mut word = String::from(ch);
            while chars.peek().is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '$')) {
                word.push(chars.next().unwrap());
            }
            Token::Word(word)
        } else {
            Token::Punct(ch)
        };
        if tokens.len() == MAX_TOKENS {
            return Err(CommandEvidenceStatus::LimitExceeded);
        }
        tokens.push(token);
    }
    Ok(tokens)
}

fn static_object(tokens: &[Token]) -> Option<Value> {
    if tokens.first() != Some(&Token::Punct('{')) {
        return None;
    }
    let mut object = serde_json::Map::new();
    let mut position = 1;
    loop {
        if tokens.get(position) == Some(&Token::Punct('}')) {
            return (tokens.get(position + 1) == Some(&Token::Punct(')')))
                .then_some(Value::Object(object));
        }
        let key = tokens.get(position)?.word().or_else(|| tokens[position].literal())?;
        if tokens.get(position + 1) != Some(&Token::Punct(':')) {
            return None;
        }
        let token = tokens.get(position + 2)?;
        let value = token.literal().map(|value| Value::String(value.into())).unwrap_or(Value::Null);
        if object.insert(key.into(), value).is_some() {
            return None;
        }
        position += 3;
        match tokens.get(position) {
            Some(Token::Punct(',')) => position += 1,
            Some(Token::Punct('}')) => {}
            _ => return None,
        }
    }
}

fn shell_words(line: &str) -> Option<Vec<String>> {
    let mut chars = line.chars().peekable();
    let mut words = Vec::new();
    let mut word = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '#' if word.is_empty() => break,
            '\'' | '"' => {
                let mut terminated = false;
                for next in chars.by_ref() {
                    if next == ch {
                        terminated = true;
                        break;
                    }
                    if ch == '"' && matches!(next, '$' | '`' | '\\') {
                        return None;
                    }
                    word.push(next);
                }
                if !terminated {
                    return None;
                }
            }
            '\\' => word.push(chars.next()?),
            '$' | '`' | '(' | ')' => return None,
            ';' | '&' | '|' | '<' | '>' => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                let mut operator = String::from(ch);
                if chars.peek() == Some(&ch) {
                    operator.push(chars.next()?);
                }
                if matches!(operator.as_str(), "&" | "<") {
                    return None;
                }
                words.push(operator);
            }
            ch if ch.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(ch),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    Some(words)
}
