//! Rules the canvas and figure native project files share: the size limits and the persisted feature-key checks. Each
//! engine keeps its own envelope layout, its own key list, and its own error codes and text. The errors here are
//! neutral, and each engine turns them into its own.

/// Largest native project file an engine writes or reads (256 MiB).
pub const NATIVE_MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Largest total of embedded asset bytes in one native project file (128 MiB).
pub const NATIVE_MAX_EMBEDDED_BYTES: u64 = 128 * 1024 * 1024;

/// Why a declared `feature_keys` list is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureKeyError<'a> {
    /// The list is present on a format version without feature keys.
    NotAllowed,
    /// The feature-key format version has no list, or an empty one.
    Required,
    /// The same key twice in a row.
    Duplicate(&'a str),
    /// A key sorts before the one ahead of it.
    Unsorted,
    /// A key this build does not know.
    Unsupported(&'a str),
}

/// A `feature_keys` list belongs only on the feature-key format version, and that version requires one.
pub fn check_feature_key_presence(present: bool, keys_version: bool) -> Result<(), FeatureKeyError<'static>> {
    match (present, keys_version) {
        (true, false) => Err(FeatureKeyError::NotAllowed),
        (false, true) => Err(FeatureKeyError::Required),
        _ => Ok(()),
    }
}

/// A declared list on the feature-key version: not empty, sorted and unique (the first pair out of order decides), and
/// every key in `known`. Membership is checked after the order.
pub fn check_feature_key_list<'a>(keys: &'a [String], known: &[&str]) -> Result<(), FeatureKeyError<'a>> {
    if keys.is_empty() {
        return Err(FeatureKeyError::Required);
    }
    for pair in keys.windows(2) {
        if pair[0] == pair[1] {
            return Err(FeatureKeyError::Duplicate(&pair[0]));
        }
        if pair[0] > pair[1] {
            return Err(FeatureKeyError::Unsorted);
        }
    }
    match keys.iter().find(|key| !known.contains(&key.as_str())) {
        Some(unknown) => Err(FeatureKeyError::Unsupported(unknown)),
        None => Ok(()),
    }
}

/// Why the keys a document uses do not fit its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureUseError {
    /// Below the feature-key version, the document uses a key that version cannot carry.
    NeedsKeysVersion,
    /// On the feature-key version, the declared list is not exactly the keys the document uses.
    Mismatch,
}

/// The keys a document uses against its file. Below the feature-key version, the document may use only keys in
/// `allowed_below`. On that version, `declared` lists exactly `used`, in the same order.
pub fn check_feature_keys_used<D: AsRef<str>, U: AsRef<str>>(
    declared: &[D],
    used: &[U],
    keys_version: bool,
    allowed_below: &[&str],
) -> Result<(), FeatureUseError> {
    if !keys_version {
        if used.iter().any(|key| !allowed_below.contains(&key.as_ref())) {
            return Err(FeatureUseError::NeedsKeysVersion);
        }
        return Ok(());
    }
    let same = declared.len() == used.len() && declared.iter().zip(used).all(|(d, u)| d.as_ref() == u.as_ref());
    if same {
        Ok(())
    } else {
        Err(FeatureUseError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn limits_are_256_and_128_mib() {
        assert_eq!(NATIVE_MAX_FILE_BYTES, 268_435_456);
        assert_eq!(NATIVE_MAX_EMBEDDED_BYTES, 134_217_728);
    }

    #[test]
    fn presence_is_tied_to_the_feature_key_version() {
        assert_eq!(check_feature_key_presence(true, false), Err(FeatureKeyError::NotAllowed));
        assert_eq!(check_feature_key_presence(false, true), Err(FeatureKeyError::Required));
        assert_eq!(check_feature_key_presence(false, false), Ok(()));
        assert_eq!(check_feature_key_presence(true, true), Ok(()));
    }

    #[test]
    fn the_list_is_checked_for_emptiness_then_order_then_membership() {
        let known = ["a.v1", "b.v1", "c.v1"];
        assert_eq!(check_feature_key_list(&[], &known), Err(FeatureKeyError::Required));
        assert_eq!(check_feature_key_list(&keys(&["a.v1", "c.v1"]), &known), Ok(()));
        assert_eq!(check_feature_key_list(&keys(&["a.v1", "a.v1"]), &known), Err(FeatureKeyError::Duplicate("a.v1")));
        assert_eq!(check_feature_key_list(&keys(&["c.v1", "a.v1"]), &known), Err(FeatureKeyError::Unsorted));
        assert_eq!(check_feature_key_list(&keys(&["z.v1", "y.v1"]), &known), Err(FeatureKeyError::Unsorted), "order first");
        assert_eq!(check_feature_key_list(&keys(&["a.v1", "x.v1"]), &known), Err(FeatureKeyError::Unsupported("x.v1")));
    }

    #[test]
    fn used_keys_need_the_feature_key_version_and_an_exact_list() {
        let none: [&str; 0] = [];
        assert_eq!(check_feature_keys_used(&none, &["a.v1"], false, &[]), Err(FeatureUseError::NeedsKeysVersion));
        assert_eq!(check_feature_keys_used(&none, &["a.v1"], false, &["a.v1"]), Ok(()));
        assert_eq!(check_feature_keys_used(&none, &none, false, &[]), Ok(()));
        assert_eq!(check_feature_keys_used(&keys(&["a.v1"]), &["a.v1"], true, &[]), Ok(()));
        assert_eq!(check_feature_keys_used(&keys(&["a.v1"]), &["a.v1", "b.v1"], true, &[]), Err(FeatureUseError::Mismatch));
        assert_eq!(check_feature_keys_used(&keys(&["b.v1", "a.v1"]), &["a.v1", "b.v1"], true, &[]), Err(FeatureUseError::Mismatch));
    }
}
