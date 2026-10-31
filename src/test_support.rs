//! Helpers shared by unit tests.

use std::path::Path;

/// Loads `tests/fixtures/{name}.json` as a JSON value.
pub fn fx(name: &str) -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.json"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("invalid fixture {name}: {e}"))
}

/// The frozen compatibility list used by tests that assert specific model
/// data. `compat.json` at the repo root is regenerated weekly, so tests must
/// not depend on its contents.
pub fn compat_seed() -> crate::compat::CompatList {
    crate::compat::parse_compat(include_str!("../tests/fixtures/compat_seed.json"))
        .expect("tests/fixtures/compat_seed.json is a valid compat list")
}
