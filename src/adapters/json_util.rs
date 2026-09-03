use std::io;

use serde_json::Value;

pub(crate) fn rfc3339_ms(value: Option<&Value>) -> Option<i64> {
    let text = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|dt| dt.timestamp_millis())
}

pub(crate) fn json_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_f64().map(|value| value as i64))
}

pub(crate) fn jsonl_indexed(
    lines: impl IntoIterator<Item = io::Result<String>>,
) -> impl Iterator<Item = io::Result<(usize, Value)>> {
    lines.into_iter().enumerate().filter_map(|(index, line)| match line {
        Ok(line) => {
            let trimmed = line.trim().trim_start_matches('\u{feff}');
            if trimmed.is_empty() {
                None
            } else {
                serde_json::from_str::<Value>(trimmed).ok().map(|value| Ok((index, value)))
            }
        }
        Err(error) => Some(Err(error)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_indexed_strips_utf8_bom() {
        let lines = [Ok("\u{feff}{\"type\":\"user\"}".to_string())];
        let items: Vec<_> = jsonl_indexed(lines).collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1.get("type").and_then(Value::as_str), Some("user"));
    }
}
