//! Lexical checks before a caller-supplied file name reaches the filesystem.
use std::path::Path;

/// Request key the desktop adds (`true`) to artifact.open, asset.import, artifact.save and artifact.export when the
/// path came from an agent's ui.command.run: the core then applies the agent path rules on the gui connection too.
/// The core removes it before parsing; it is not part of any request type.
pub const AGENT_PATH_PARAM: &str = "agent_path";

pub fn is_windows_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    let port = stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT"));
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || port.is_some_and(|n| matches!(n, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}"))
}

pub fn is_local_file_path(raw: &str) -> bool {
    let path = Path::new(raw);
    if !path.is_absolute() || path.file_name().is_none() || raw.contains('\0') {
        return false;
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut parts = path.components();
        let Some(Component::Prefix(prefix)) = parts.next() else { return false };
        if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
            return false;
        }
        // Verbatim paths use only backslash as separator. Keep ordinary canonicalized drive paths.
        if matches!(prefix.kind(), Prefix::VerbatimDisk(_)) && raw.contains('/') {
            return false;
        }
        parts.all(|part| match part {
            Component::RootDir => true,
            Component::Normal(name) => name.to_str().is_some_and(|s| !s.contains(':') && !s.ends_with(['.', ' ']) && !is_windows_device_name(s)),
            _ => false,
        })
    }
    #[cfg(not(windows))]
    {
        !raw.starts_with("//") && !path.components().any(|part| matches!(part, std::path::Component::ParentDir))
    }
}
