//! Saved-part comparison: each expected part is looked up in the saved output and its payload hash is compared.
//!
//! Engines choose which parts to check, which diagnostic codes and messages to emit, and how to count checked
//! parts. This module only reports, in input order, whether each part was kept, changed, or is missing.

use designllm_contracts::sha256;

use crate::Package;

/// What the saved output holds for one expected part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartState {
    /// The part is present and its payload hash matches.
    Kept,
    /// The part is present but its payload hash differs.
    Changed,
    /// No part with exactly this name is present.
    Missing,
}

/// Compares `expected` parts, each a name and its expected SHA-256 hex, with `actual`, which returns the saved
/// payload hash of a part or `None` when the saved output has no part with exactly that name. The result keeps
/// the order of `expected`.
pub fn compare_parts<'a, S: AsRef<str>>(
    expected: impl IntoIterator<Item = (&'a str, &'a str)>,
    mut actual: impl FnMut(&str) -> Option<S>,
) -> Vec<(&'a str, PartState)> {
    expected
        .into_iter()
        .map(|(name, sha)| {
            let state = match actual(name) {
                None => PartState::Missing,
                Some(found) if found.as_ref() == sha => PartState::Kept,
                Some(_) => PartState::Changed,
            };
            (name, state)
        })
        .collect()
}

/// SHA-256 hex of the decoded payload of the part named exactly `name`, or `None` when `package` has no such part.
/// The entry's compression method does not affect the hash.
pub fn payload_sha256(package: &Package, name: &str) -> Option<String> {
    package.part(name).ok().map(sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_follow_the_input_order() {
        let saved = [("b.xml", "2"), ("c.xml", "9")];
        let lookup = |name: &str| saved.iter().find(|(n, _)| *n == name).map(|(_, sha)| *sha);
        let result = compare_parts([("c.xml", "3"), ("a.xml", "1"), ("b.xml", "2")], lookup);
        assert_eq!(result, vec![("c.xml", PartState::Changed), ("a.xml", PartState::Missing), ("b.xml", PartState::Kept)]);
        assert!(compare_parts(std::iter::empty(), lookup).is_empty());
    }

    #[test]
    fn payload_hash_uses_the_exact_part_name() {
        let mut package = Package::new();
        package.set_part("word/a.xml", b"<a/>".to_vec()).expect("set");
        assert_eq!(payload_sha256(&package, "word/a.xml"), Some(sha256(b"<a/>")));
        assert_eq!(payload_sha256(&package, "word/A.xml"), None);
        assert_eq!(payload_sha256(&package, "word/b.xml"), None);
    }
}
