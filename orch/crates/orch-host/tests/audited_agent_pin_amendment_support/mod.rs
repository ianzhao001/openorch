//! Self-contained historical reader fixtures; no registry amendment producer.
// Other exports are exercised by b289_fixture_round_isolation; this reader needs only construction.
#[allow(dead_code)]
#[path = "../b289_fixture_round_isolation_support/mod.rs"]
mod fixture;

/// Build independent historical inputs for the retained plan reader.
pub fn fixture_root(label: &str) -> std::path::PathBuf {
    fixture::synthesize_signed_round(label)
}
