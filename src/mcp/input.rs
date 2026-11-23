use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_json::{Map, Value};

use crate::event_evidence::EvidencePart;

const SOURCE_CORRUPTION_ERR: &str = "invalid source parameter: nested call_mcp_tool argument serialization corrupted this value. Do NOT retry the same payload unchanged; pass a comma-separated string such as \"codex,claude-code\".";

const OPAQUE_TOKEN_ERR: &str = "invalid opaque parameter: nested call_mcp_tool argument serialization corrupted cursor or event_ref. Copy the exact opaque JSON string from the previous response or run the recall CLI instead. Do NOT retry the same payload unchanged.";

pub(super) fn deserialize_opt_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::Number(num) => {
            if let Some(unsigned) = num.as_u64() {
                u32::try_from(unsigned).map(Some).map_err(|_| {
                    de::Error::custom(
                        "integer exceeds 32-bit range. Do NOT retry the same payload unchanged. Pass a valid non-negative integer.",
                    )
                })
            } else if num.as_i64().is_some_and(|signed| signed < 0) {
                Err(de::Error::custom(
                    "negative integer is not valid for an unsigned integer parameter. Do NOT retry the same payload unchanged. Pass a non-negative integer.",
                ))
            } else {
                Err(de::Error::custom(
                    "floating-point number is not valid for an integer parameter. Do NOT retry the same payload unchanged. Pass a whole non-negative integer.",
                ))
            }
        }
        Value::String(text) => parse_opt_u32_str(&text).map_err(de::Error::custom),
        Value::Bool(_) | Value::Array(_) | Value::Object(_) => Err(de::Error::custom(
            "invalid integer parameter: nested call_mcp_tool argument serialization corrupted this field, or the wrong type was passed. Do NOT retry the same payload unchanged. Pass a non-negative integer.",
        )),
    }
}

fn parse_opt_u32_str(value: &str) -> Result<Option<u32>, &'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.starts_with('-') {
        return Err(
            "negative integer is not valid for an unsigned integer parameter. Do NOT retry the same payload unchanged. Pass a non-negative integer.",
        );
    }
    if trimmed.contains('.') {
        return Err(
            "floating-point number is not valid for an integer parameter. Do NOT retry the same payload unchanged. Pass a whole non-negative integer.",
        );
    }
    if !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(
            "invalid integer string. Do NOT retry the same payload unchanged. Pass a non-negative integer.",
        );
    }
    trimmed.parse::<u32>().map(Some).map_err(|_| {
        "integer exceeds 32-bit range. Do NOT retry the same payload unchanged. Pass a valid non-negative integer."
    })
}

pub(super) fn deserialize_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(deserialize_opt_bool(deserializer)?.unwrap_or(false))
}

pub(super) fn deserialize_opt_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::Bool(flag) => Ok(Some(flag)),
        Value::String(text) => match text.trim() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(de::Error::custom(
                "invalid boolean parameter: expected true or false. Do NOT retry the same payload unchanged. Pass true or false.",
            )),
        },
        Value::Number(_) | Value::Array(_) | Value::Object(_) => Err(de::Error::custom(
            "invalid boolean parameter: nested call_mcp_tool argument serialization corrupted this field, or the wrong type was passed. Do NOT retry the same payload unchanged. Pass true or false.",
        )),
    }
}

pub(super) fn deserialize_required_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(text) => Ok(text),
        _ => Err(de::Error::custom(
            "invalid required string parameter: nested call_mcp_tool argument serialization corrupted this field, or a non-string was passed. Do NOT retry the same payload unchanged. Pass a plain string or run the recall CLI instead.",
        )),
    }
}

pub(super) fn deserialize_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text)),
        _ => Err(de::Error::custom(
            "invalid optional string parameter: nested call_mcp_tool argument serialization corrupted this field. Do NOT retry the same payload unchanged. Pass a plain string or omit the parameter.",
        )),
    }
}

pub(super) fn deserialize_opt_source<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(text) => parse_source_string(&text).map_err(de::Error::custom),
        Value::Array(items) => parse_source_array(&items).map(Some).map_err(de::Error::custom),
        Value::Bool(_) | Value::Number(_) | Value::Object(_) => {
            Err(de::Error::custom(SOURCE_CORRUPTION_ERR))
        }
    }
}

fn parse_source_string(text: &str) -> Result<Option<String>, &'static str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if matches!(trimmed, "true" | "false" | "null") || trimmed.starts_with('{') {
        return Err(SOURCE_CORRUPTION_ERR);
    }
    if trimmed.starts_with('[') {
        let items: Vec<Value> = serde_json::from_str(trimmed).map_err(|_| SOURCE_CORRUPTION_ERR)?;
        return parse_source_array(&items).map(Some);
    }
    Ok(Some(text.to_string()))
}

fn parse_source_array(items: &[Value]) -> Result<String, &'static str> {
    if items.is_empty() {
        return Err(SOURCE_CORRUPTION_ERR);
    }
    let mut joined = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(text) = item else {
            return Err(SOURCE_CORRUPTION_ERR);
        };
        let trimmed = text.trim();
        if trimmed.is_empty() || matches!(trimmed, "true" | "false" | "null" | "[]" | "{}") {
            return Err(SOURCE_CORRUPTION_ERR);
        }
        joined.push(trimmed);
    }
    Ok(joined.join(","))
}

pub(super) fn deserialize_opt_cursor<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_opt_opaque_json(deserializer)
}

pub(super) fn deserialize_opt_event_ref<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_opt_opaque_json(deserializer)
}

fn deserialize_opt_opaque_json<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::Object(map) => serialize_opaque_object(map).map(Some).map_err(de::Error::custom),
        Value::String(text) => validate_opaque_string(text).map(Some).map_err(de::Error::custom),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) => {
            Err(de::Error::custom(OPAQUE_TOKEN_ERR))
        }
    }
}

fn serialize_opaque_object(map: Map<String, Value>) -> Result<String, &'static str> {
    if map.is_empty() {
        return Err(OPAQUE_TOKEN_ERR);
    }
    serde_json::to_string(&Value::Object(map)).map_err(|_| OPAQUE_TOKEN_ERR)
}

fn validate_opaque_string(text: String) -> Result<String, &'static str> {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || matches!(trimmed, "true" | "false" | "null" | "[]" | "{}")
        || trimmed.parse::<f64>().is_ok()
    {
        return Err(OPAQUE_TOKEN_ERR);
    }
    Ok(text)
}

pub(super) fn deserialize_opt_evidence_part<'de, D>(
    deserializer: D,
) -> Result<Option<EvidencePart>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(text) => match text.trim() {
            "payload" => Ok(Some(EvidencePart::Payload)),
            "before" => Ok(Some(EvidencePart::Before)),
            "after" => Ok(Some(EvidencePart::After)),
            _ => Err(de::Error::custom(
                "invalid evidence_part parameter: expected \"payload\", \"before\", or \"after\". Do NOT retry the same payload unchanged.",
            )),
        },
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            Err(de::Error::custom(
                "invalid evidence_part parameter: nested call_mcp_tool argument serialization corrupted this field. Expected \"payload\", \"before\", or \"after\". Do NOT retry the same payload unchanged.",
            ))
        }
    }
}
