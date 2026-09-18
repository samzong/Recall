use serde_json::{Value, json};

pub const DATASET_SCHEMA_NAME: &str = "recall-session-dataset";
pub const DATASET_SCHEMA_VERSION: u32 = 1;
pub const MANIFEST_SCHEMA_NAME: &str = "recall-session-dataset-manifest";
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const SOURCE_SCHEMA_VERSION: u32 = 7;
pub const PROTOCOL_VERSION: u32 = 2;
pub const MIN_RECALL: &str = "0.6.0";

pub fn manifest_json() -> Value {
    json!({
        "name": "publish",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL_VERSION,
        "min_recall": MIN_RECALL
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_extension_contract() {
        let manifest = manifest_json();
        assert_eq!(manifest["name"], "publish");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["protocol"], 2);
        assert_eq!(manifest["min_recall"], MIN_RECALL);
    }
}
