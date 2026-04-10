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
