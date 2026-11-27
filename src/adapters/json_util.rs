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

pub(crate) fn usage_json_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(value) = value.as_i64() {
        return Some(value);
    }
    if let Some(value) = value.as_u64() {
        return i64::try_from(value).ok();
    }
    let value = value.as_f64()?;
    (value.is_finite()
        && value.fract() == 0.0
        && value > i64::MIN as f64
        && value < -(i64::MIN as f64))
        .then_some(value as i64)
}

#[cfg(test)]
mod usage_number_tests {
    use super::*;

    #[test]
    fn usage_integers_reject_fractional_and_out_of_range_numbers() {
        for text in ["1.5", "1e100", "9223372036854775808", "-9223372036854775809"] {
            let value: Value = serde_json::from_str(text).unwrap();
            assert_eq!(usage_json_i64(Some(&value)), None, "{text}");
        }
        for value in [i64::MIN, 0, i64::MAX] {
            assert_eq!(usage_json_i64(Some(&serde_json::json!(value))), Some(value));
        }
    }
}
