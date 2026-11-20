//! Codex CLI installed with `npm install -g @openai/codex`.
//!
//! npm puts shims (`codex.cmd`, `codex.ps1`, `codex`) in the global prefix. Each shim only runs
//! `node <prefix>\node_modules\@openai\codex\bin\codex.js`, and `codex.js` spawns the native binary it
//! finds this way (`findCodexExecutable` in `bin/codex.js`, package 0.170.0):
//!
//! 1. `require.resolve("@openai/codex-win32-<arch>/package.json")` from `bin/codex.js`; the vendor root is
//!    `vendor` next to that `package.json`. npm nests the platform package under the package's own
//!    `node_modules` or hoists it to the prefix's `node_modules`; both are checked, nested first, as Node
//!    resolution orders them.
//! 2. Otherwise `<package>\vendor`.
//! 3. The binary is `<vendor root>\<target triple>\bin\codex.exe`.
//!
//! `codex.js` starts that binary with `CODEX_MANAGED_PACKAGE_ROOT=<package root>` and, for an npm install,
//! `CODEX_MANAGED_BY_NPM=1`, after removing the other `CODEX_MANAGED_BY_*` markers; [`managed_env`] repeats
//! that. This module returns the native `codex.exe` and that environment, so the runner starts it directly.
//! The shim is never run: `cmd /c` would re-parse the arguments with batch quoting rules. Nothing is written.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::EnvChange;

const PACKAGE: &str = "@openai/codex";

/// A `package.json` larger than this is not read.
const MAX_MANIFEST_BYTES: u64 = 1 << 20;

/// `(platform package, target triple)` as `PLATFORM_PACKAGE_BY_TARGET` in `bin/codex.js` names them.
fn platform() -> Option<(&'static str, &'static str)> {
    if cfg!(target_arch = "x86_64") {
        Some(("codex-win32-x64", "x86_64-pc-windows-msvc"))
    } else if cfg!(target_arch = "aarch64") {
        Some(("codex-win32-arm64", "aarch64-pc-windows-msvc"))
    } else {
        None
    }
}

/// The native `codex.exe` behind the npm shim in `prefix` and the environment to start it with, or `None`
/// when `prefix` holds no complete npm install of `@openai/codex`.
pub(super) fn codex_native(prefix: &Path) -> Option<(PathBuf, Vec<EnvChange>)> {
    if !prefix.join("codex.cmd").is_file() {
        return None;
    }
    let package = prefix.join(r"node_modules\@openai\codex");
    if !is_codex_manifest(&package.join("package.json")) {
        return None;
    }
    let (platform_package, triple) = platform()?;
    let vendor = [package.join(r"node_modules\@openai"), prefix.join(r"node_modules\@openai")]
        .into_iter()
        .map(|scope| scope.join(platform_package))
        .find(|root| root.join("package.json").is_file())
        .map_or_else(|| package.join("vendor"), |root| root.join("vendor"));
    let exe = vendor.join(triple).join(r"bin\codex.exe");
    exe.is_file().then(|| (exe, managed_env(package)))
}

/// True when `path` is a JSON object of at most [`MAX_MANIFEST_BYTES`] whose `name` is `@openai/codex`.
/// Anything else (unreadable, too large, invalid JSON, a byte-order mark, not an object) is false.
fn is_codex_manifest(path: &Path) -> bool {
    let mut bytes = Vec::new();
    let read = fs::File::open(path).and_then(|file| file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes));
    if read.is_err() || bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|manifest| manifest.get("name")?.as_str().map(|name| name == PACKAGE))
        .unwrap_or(false)
}

/// The environment `bin/codex.js` (lines 226-234 in 0.170.0) gives the native binary of an npm install.
fn managed_env(package: PathBuf) -> Vec<EnvChange> {
    vec![
        ("CODEX_MANAGED_PACKAGE_ROOT", Some(package.into_os_string())),
        ("CODEX_MANAGED_BY_NPM", Some("1".into())),
        ("CODEX_MANAGED_BY_BUN", None),
        ("CODEX_MANAGED_BY_PNPM", None),
        ("CODEX_MANAGED_BY_VITE_PLUS", None),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_manifest_is_not_codex_and_never_panics() {
        let dir = crate::detect::tests::TempDir::new("npm-manifest");
        let path = dir.join("package.json");
        let good = br#"{"name":"@openai/codex"}"#;
        let mut oversized = br#"{"name":"@openai/codex","pad":""#.to_vec();
        oversized.resize(MAX_MANIFEST_BYTES as usize + 16, b' ');
        oversized.extend_from_slice(b"\"}");
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend_from_slice(good);
        let cases: [(&str, &[u8]); 8] = [
            ("invalid json", b"{\"name\": \"@openai/codex\""),
            ("byte-order mark", &bom),
            ("name missing", br#"{"bin":{"codex":"bin/codex.js"}}"#),
            ("name not a string", br#"{"name":["@openai/codex"]}"#),
            ("array", br#"["@openai/codex"]"#),
            ("string", br#""@openai/codex""#),
            ("empty", b""),
            ("over the size cap", &oversized),
        ];
        for (label, bytes) in cases {
            fs::write(&path, bytes).unwrap();
            assert!(!is_codex_manifest(&path), "{label} must not count as the codex package");
        }
        assert!(!is_codex_manifest(&dir.join("missing.json")));
        fs::write(&path, good).unwrap();
        assert!(is_codex_manifest(&path));
    }
}
