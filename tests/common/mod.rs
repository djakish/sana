//! Shared test helpers. Not compiled as its own test binary (it lives in a
//! subdirectory), so unused items here would warn per-including-crate; keep it
//! lean and `allow(dead_code)`.
#![allow(dead_code)]

use std::path::PathBuf;

/// Snapshot/golden assertion. On first run (fixture absent) it records the
/// bytes and passes; afterwards it asserts equality. Fixtures are committed to
/// git so accidental format drift is caught in review. Delete the fixture file
/// to intentionally regenerate.
pub fn assert_golden(name: &str, actual: &[u8]) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = dir.join(name);
    match std::fs::read(&path) {
        Ok(expected) => {
            assert_eq!(
                actual,
                expected.as_slice(),
                "golden mismatch for {name}; delete tests/fixtures/{name} to regenerate"
            );
        }
        Err(_) => {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, actual).unwrap();
            eprintln!("recorded new golden fixture: tests/fixtures/{name}");
        }
    }
}
