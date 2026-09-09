//! `SPEC.md` §0.5 clause 2a — the enforcement the puzzle-hash guard (`tests/puzzle_hash_guard.rs`)
//! CANNOT provide, because that guard is a test in THIS crate and never runs in a consumer's
//! build. This test instead reads this repo's own committed `Cargo.lock` and asserts every §0.5
//! cohort member resolves to the exact version the SPEC's table records.
//!
//! It is what caught the drift #3272 found: the lock resolved `clvmr 0.16.4` while the SPEC named
//! `0.16.2`, and nothing failed, because `.github/workflows/ci.yml` ran the coverage job without
//! `--locked` and the resolver was free to pick anything satisfying the caret range.
//!
//! `include_str!` binds this test to the literal bytes of `Cargo.lock` at compile time, so editing
//! one version and re-running (without `cargo update`) is enough to prove the test is red for the
//! right reason.

use std::collections::HashMap;

// NOTE: `Cargo.lock` can legitimately list the SAME crate name at several major versions when two
// unrelated dependents each pin their own major (e.g. a different sub-dependency wanting an older
// `chia-bls`). So this test does not require a name resolve to EXACTLY one version — it requires
// the SPEC-pinned version to be ONE of the versions actually resolved, which is what catches a
// cohort member drifting away from the pinned version entirely (the #3272 `clvmr` case).

/// The exact version §0.5's table records for each cohort member, after #3272.
///
/// The three byte-bearing crates are pinned `=` in `Cargo.toml`, so their lock version can only
/// ever equal this. The four dev-only names are caret in `Cargo.toml` — this table is descriptive
/// of THIS repo's lock, not a normative requirement on a consumer (§0.5 clause 1).
const EXPECTED_COHORT_VERSIONS: &[(&str, &str)] = &[
    ("chia-sdk-driver", "0.36.0"),
    ("chia-sdk-types", "0.36.0"),
    ("chia-puzzle-types", "0.36.1"),
    ("chia-protocol", "0.36.1"),
    ("chia-bls", "0.36.1"),
    ("chia-consensus", "0.36.1"),
    ("chia-puzzles", "0.20.3"),
    ("clvm-traits", "0.36.1"),
    ("clvm-utils", "0.36.1"),
    ("clvmr", "0.16.4"),
];

/// Parses `Cargo.lock`'s `[[package]]` tables into `name -> [versions resolved for that name]`.
fn locked_versions(lock: &str) -> HashMap<&str, Vec<&str>> {
    let mut versions: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut current_name: Option<&str> = None;

    for line in lock.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("name = \"").and_then(|s| s.strip_suffix('"')) {
            current_name = Some(name);
            continue;
        }
        if let Some(version) = line.strip_prefix("version = \"").and_then(|s| s.strip_suffix('"'))
        {
            if let Some(name) = current_name.take() {
                versions.entry(name).or_default().push(version);
            }
        }
    }

    versions
}

#[test]
fn cohort_members_resolve_to_the_spec_pinned_versions() {
    let lock = include_str!("../Cargo.lock");
    let versions = locked_versions(lock);

    let mut mismatches = Vec::new();
    for (name, expected) in EXPECTED_COHORT_VERSIONS {
        match versions.get(*name) {
            Some(actual) if actual.iter().any(|v| v == expected) => {}
            Some(actual) => mismatches.push(format!(
                "{name}: Cargo.lock resolves {actual:?}, SPEC.md §0.5 expects {expected} among them"
            )),
            None => mismatches.push(format!("{name}: not found in Cargo.lock at all")),
        }
    }

    assert!(
        mismatches.is_empty(),
        "cohort drift from SPEC.md §0.5's table:\n{}",
        mismatches.join("\n")
    );
}
