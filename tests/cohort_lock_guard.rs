//! `SPEC.md` §0.5 clause 2a — the enforcement the puzzle-hash guard (`tests/puzzle_hash_guard.rs`)
//! CANNOT provide, because that guard is a test in THIS crate and never runs in a consumer's
//! build. This test instead reads this repo's own committed `Cargo.lock` and asserts every §0.5
//! cohort member resolves to the exact version the SPEC's table records.
//!
//! It is what caught the drift #3272 found: the lock resolved `clvmr 0.16.4` while the SPEC named
//! `0.16.2`, and nothing failed, because `.github/workflows/ci.yml` ran the coverage job without
//! `--locked` and the resolver was free to pick anything satisfying the caret range.
//!
//! # Proving this test is red for the right reason
//!
//! `include_str!` binds the literal bytes of `Cargo.lock` AND `Cargo.toml` at compile time, but
//! editing a version in either file to prove the point does not work: cargo re-resolves before it
//! builds, so it either rewrites the lock or aborts with a resolution error and the test never
//! runs. Edit an EXPECTED constant in this file instead --- e.g. `clvmr` to `0.16.3` in
//! [`EXPECTED_COHORT_VERSIONS`], or `chia-sdk-driver` to `=0.36.1` in
//! [`EXPECTED_MANIFEST_REQUIREMENTS`] --- run `cargo test --locked --test cohort_lock_guard`,
//! see it fail naming that crate, and revert. That exercises the same comparison from the other
//! side and needs no resolver cooperation.

use std::collections::HashMap;

// `Cargo.lock` can legitimately list the SAME crate name at several majors when two unrelated
// dependents each pin their own (`chia-bls` and `clvm-traits` do here). An earlier version of this
// guard therefore accepted the pinned version as "one of" the resolved ones — which let a DRIFTED
// version pass unnoticed as long as some other resolution still matched. So the table below names
// the COMPLETE expected set per crate and the assertion compares the whole set: a new duplicate
// appearing, or an existing one moving, is drift and fails here.

/// The complete set of versions `Cargo.lock` resolves for each §0.5 cohort member, after #3272.
///
/// The three byte-bearing crates are pinned `=` in `Cargo.toml`
/// ([`EXPECTED_MANIFEST_REQUIREMENTS`]), so their lock version can only ever equal this. The four
/// dev-only names are caret — for those this table is descriptive of THIS repo's lock, not a
/// normative requirement on a consumer (§0.5 clause 1).
///
/// The extra `chia-bls` and `clvm-traits` majors are transitive and unrelated to the cohort's
/// puzzle bytes; they are listed so that the comparison can be an exact one.
const EXPECTED_COHORT_VERSIONS: &[(&str, &[&str])] = &[
    ("chia-sdk-driver", &["0.36.0"]),
    ("chia-sdk-types", &["0.36.0"]),
    ("chia-puzzle-types", &["0.36.1"]),
    ("chia-protocol", &["0.36.1"]),
    ("chia-bls", &["0.28.2", "0.36.1", "0.42.1"]),
    ("chia-consensus", &["0.36.1"]),
    ("chia-puzzles", &["0.20.3"]),
    ("clvm-traits", &["0.28.1", "0.36.1"]),
    ("clvm-utils", &["0.36.1"]),
    ("clvmr", &["0.16.4"]),
];

/// The exact `[dependencies]` requirements §0.5 clause 1 makes normative for a consumer.
///
/// The lock is what THIS repo builds; a requirement is what every consumer inherits, and it is the
/// half #3272 is about — a `^` here lets a consumer resolve moved puzzle bytes however green this
/// repo's lock is. Asserted against `Cargo.toml`'s literal bytes.
const EXPECTED_MANIFEST_REQUIREMENTS: &[(&str, &str)] = &[
    ("chia-sdk-driver", "=0.36.0"),
    ("chia-sdk-types", "=0.36.0"),
    ("chia-puzzle-types", "=0.36.1"),
];

/// Parses `Cargo.lock`'s `[[package]]` tables into `name -> [versions resolved for that name]`.
fn locked_versions(lock: &str) -> HashMap<&str, Vec<&str>> {
    let mut versions: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut current_name: Option<&str> = None;

    for line in lock.lines() {
        let line = line.trim();
        if let Some(name) = line
            .strip_prefix("name = \"")
            .and_then(|s| s.strip_suffix('"'))
        {
            current_name = Some(name);
            continue;
        }
        if let Some(version) = line
            .strip_prefix("version = \"")
            .and_then(|s| s.strip_suffix('"'))
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
            Some(actual) => {
                let mut actual = actual.clone();
                actual.sort_unstable();
                let mut expected: Vec<&str> = expected.to_vec();
                expected.sort_unstable();
                if actual != expected {
                    mismatches.push(format!(
                        "{name}: Cargo.lock resolves {actual:?}, SPEC.md §0.5 expects exactly {expected:?}"
                    ));
                }
            }
            None => mismatches.push(format!("{name}: not found in Cargo.lock at all")),
        }
    }

    assert!(
        mismatches.is_empty(),
        "cohort drift from SPEC.md §0.5's table:\n{}",
        mismatches.join("\n")
    );
}

/// The half a consumer actually inherits: the `=` requirements in `Cargo.toml` itself.
///
/// The lock guard above cannot see this — a lock is not published — so without this assertion a
/// requirement could silently relax back to `^` while every version check stayed green.
#[test]
fn the_byte_bearing_crates_are_pinned_exactly_in_the_manifest() {
    let manifest = include_str!("../Cargo.toml");

    for (name, requirement) in EXPECTED_MANIFEST_REQUIREMENTS {
        let line = manifest
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with(&format!("{name} = ")))
            .unwrap_or_else(|| panic!("{name} is not a [dependencies] entry in Cargo.toml at all"));

        // Either spelling: a bare `name = "=x.y.z"` or a table with `version = "=x.y.z"`.
        let spelled_bare = line.starts_with(&format!("{name} = \"{requirement}\""));
        let spelled_in_table = line.contains(&format!("version = \"{requirement}\""));
        assert!(
            spelled_bare || spelled_in_table,
            "SPEC.md §0.5 clause 1 requires {name} = {requirement:?} as an EXACT requirement, \
             which is what a consumer inherits; Cargo.toml says: {line}"
        );
    }
}
