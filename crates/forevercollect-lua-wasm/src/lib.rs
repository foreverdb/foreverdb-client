//! The ForeverCollect SavedVariables parser (`forevercollect-lua`) for the browser: the
//! website converts `ForeverCollect.lua` with exactly the code the desktop uploader runs.
//!
//! Built with `pack.sh` (wasm-bindgen `--target web`) into the npm package
//! `@foreverdb/forevercollect-lua`, which every uploader release carries as an asset.

use wasm_bindgen::prelude::*;

/// The snapshot (`ForeverCollectDB`) of a SavedVariables file as JSON text, ready to be
/// gzipped and uploaded unchanged. Throws an `Error` with the parser's message for an
/// unreadable file or an unsupported `schemaVersion`.
#[wasm_bindgen]
pub fn parse(source: &[u8]) -> Result<String, JsError> {
    parse_json(source).map_err(|message| JsError::new(&message))
}

/// `parse` without the JS error type, which exists only on wasm32.
fn parse_json(source: &[u8]) -> Result<String, String> {
    let value = forevercollect_lua::parse_forever_collect(source)?;
    serde_json::to_string(&value).map_err(|error| format!("Could not encode the snapshot: {error}"))
}

#[cfg(test)]
mod tests {
    use super::parse_json;

    #[test]
    fn returns_the_snapshot_as_json() {
        let source = b"ForeverCollectDB = {\n[\"schemaVersion\"] = 9,\n[\"catalogs\"] = { [\"k\"] = { [\"projectID\"] = 2 } },\n}";
        let json: serde_json::Value = serde_json::from_str(&parse_json(source).unwrap()).unwrap();
        assert_eq!(json["schemaVersion"], 9);
        assert_eq!(json["catalogs"]["k"]["projectID"], 2);
    }

    #[test]
    fn rejects_an_unsupported_schema_version() {
        let source = b"ForeverCollectDB = {\n[\"schemaVersion\"] = 8,\n[\"catalogs\"] = { [\"k\"] = {} },\n}";
        assert!(parse_json(source).is_err());
    }
}
