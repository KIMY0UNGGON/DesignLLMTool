//! The check for a newer DesignLLMTool release, started by `--mcp` on a background thread. It never delays the MCP
//! handshake or a tool call, asks GitHub at most once per 24 hours (the answer is cached in
//! `<local app data>/DesignLLMTool/update-check.json`), and only fills an [`UpdateSlot`]: the MCP server tells the agent
//! once per connection (`designllm_bridge::update_notice`). Nothing is downloaded or replaced. Every failure is silent
//! apart from one debug line on stderr. `DESIGNLLM_NO_UPDATE_CHECK` (any non-empty value) turns it off.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use designllm_bridge::update_notice::{UpdateInfo, UpdateSlot};
use designllm_contracts::local_path;
use serde::{Deserialize, Serialize};

/// The GitHub API answer for the newest published (non-draft, non-prerelease) release.
pub const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/KIMY0UNGGON/DesignLLMTool/releases/latest";
/// Shown when a release names no usable page.
pub const RELEASES_PAGE: &str = "https://github.com/KIMY0UNGGON/DesignLLMTool/releases/latest";
/// Set to any non-empty value to turn the check off.
pub const OPT_OUT_ENV: &str = "DESIGNLLM_NO_UPDATE_CHECK";
/// GitHub is asked at most once in this many seconds.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// The whole request, connection to last byte.
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// A release answer is a few kilobytes; a larger one is refused.
const BODY_LIMIT: u64 = 2 * 1024 * 1024;
const CACHE_FILE: &str = "update-check.json";

/// What `update-check.json` holds: when GitHub was last asked and the latest release it named (empty when that check
/// failed and nothing was known before).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedCheck {
    /// Seconds since the Unix epoch.
    pub checked_at: u64,
    pub latest_tag: String,
    pub html_url: String,
}

/// The latest release as GitHub reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub html_url: String,
}

/// The result of one check: the newer release, if any, and a debug line for stderr when something failed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub update: Option<UpdateInfo>,
    pub note: Option<String>,
}

/// Whether the opt-out variable (its value, if set) turns the check off: any non-empty value does.
pub fn opted_out(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// The version in `tag` (a leading `v` or `V` allowed) when it is strictly newer than `current` and not a pre-release.
/// Build metadata is ignored. A tag that is not a semantic version gives `None`.
pub fn newer_version(current: &str, tag: &str) -> Option<String> {
    let tag = tag.trim();
    let tag = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    let mut latest = semver::Version::parse(tag).ok()?;
    let mut current = semver::Version::parse(current.trim()).ok()?;
    if !latest.pre.is_empty() {
        return None;
    }
    latest.build = semver::BuildMetadata::EMPTY;
    current.build = semver::BuildMetadata::EMPTY;
    (latest > current).then(|| latest.to_string())
}

/// Whether a cached check is recent enough that GitHub is not asked again. A time in the future (a clock set back) is not.
pub fn is_fresh(cached: &CachedCheck, now: u64) -> bool {
    now >= cached.checked_at && now - cached.checked_at < CHECK_INTERVAL_SECS
}

/// `<local app data>/DesignLLMTool/update-check.json`, or `None` when the environment names no such folder.
pub fn cache_path() -> Option<PathBuf> {
    local_path::local_app_data_dir().map(|base| base.join("DesignLLMTool").join(CACHE_FILE))
}

/// Reads a GitHub release answer: its `tag_name` and `html_url`.
pub fn parse_release(text: &str) -> Result<Release, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("the release answer is not JSON: {error}"))?;
    let tag = value.get("tag_name").and_then(serde_json::Value::as_str).ok_or("the release answer has no tag_name")?;
    let html_url = value.get("html_url").and_then(serde_json::Value::as_str).unwrap_or_default();
    Ok(Release { tag: tag.to_string(), html_url: html_url.to_string() })
}

/// A release page link that is safe to show: a short `https://github.com/` URL without spaces or control characters.
fn page_link(url: &str) -> String {
    let usable = url.starts_with("https://github.com/") && url.len() <= 300 && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    if usable { url.to_string() } else { RELEASES_PAGE.to_string() }
}

fn read_cache(path: &Path) -> Option<CachedCheck> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Writes the cache through a temporary file and a rename, so a reader never sees half a file.
fn write_cache(path: &Path, cached: &CachedCheck) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(cached).map_err(std::io::Error::other)?)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// One check against the cache at `cache`: a check younger than 24 hours is used as it is; otherwise `fetch` asks GitHub
/// and the answer is cached. A failed fetch is cached too (with the release known before, if any), so GitHub is asked at
/// most once per 24 hours whatever happens.
pub fn check(cache: &Path, now: u64, current: &str, fetch: impl FnOnce() -> Result<Release, String>) -> Outcome {
    let cached = read_cache(cache);
    let mut note = None;
    let (tag, html_url) = match cached {
        Some(cached) if is_fresh(&cached, now) => (cached.latest_tag, cached.html_url),
        earlier => {
            let (tag, html_url) = match fetch() {
                Ok(release) => (release.tag, release.html_url),
                Err(error) => {
                    note = Some(error);
                    earlier.map(|earlier| (earlier.latest_tag, earlier.html_url)).unwrap_or_default()
                }
            };
            let record = CachedCheck { checked_at: now, latest_tag: tag.clone(), html_url: html_url.clone() };
            if let Err(error) = write_cache(cache, &record) {
                let written = format!("could not write {}: {error}", cache.display());
                note = Some(match note { Some(first) => format!("{first}; {written}"), None => written });
            }
            (tag, html_url)
        }
    };
    let update = newer_version(current, &tag).map(|latest| UpdateInfo { latest, current: current.to_string(), html_url: page_link(&html_url) });
    Outcome { update, note }
}

/// Asks GitHub for the latest release: one GET with a 3-second limit and `User-Agent: DesignLLMTool/<version>`, nothing else
/// that identifies the user.
fn fetch_latest() -> Result<Release, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .user_agent(format!("DesignLLMTool/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut response = agent
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|error| format!("GET {LATEST_RELEASE_URL} failed: {error}"))?;
    let text = response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|error| format!("reading the release answer failed: {error}"))?;
    parse_release(&text)
}

/// Starts the check on a background thread that fills `slot` when a newer release is known. Does nothing when
/// `DESIGNLLM_NO_UPDATE_CHECK` is set, in a UI-test launch, in unit tests, or when there is no per-user data folder.
pub fn spawn(slot: UpdateSlot) {
    if cfg!(test) || crate::ui_test_launch() || opted_out(std::env::var_os(OPT_OUT_ENV).as_deref()) {
        return;
    }
    let Some(cache) = cache_path() else { return };
    let started = std::thread::Builder::new().name("designllm-update-check".into()).spawn(move || {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
        let outcome = check(&cache, now, env!("CARGO_PKG_VERSION"), fetch_latest);
        if let Some(note) = outcome.note {
            eprintln!("designllm: update check: {note}");
        }
        if let Some(update) = outcome.update {
            let _ = slot.set(update);
        }
    });
    if let Err(error) = started {
        eprintln!("designllm: update check: could not start: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const NOW: u64 = 1_800_000_000;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("designllm-update-check-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("DesignLLMTool").join(CACHE_FILE)
    }

    fn release(tag: &str) -> Result<Release, String> {
        Ok(Release { tag: tag.to_string(), html_url: format!("https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/{tag}") })
    }

    #[test]
    fn only_a_strictly_newer_final_release_counts() {
        assert_eq!(newer_version("0.1.1", "v0.1.2").as_deref(), Some("0.1.2"));
        assert_eq!(newer_version("0.1.1", "0.2.0").as_deref(), Some("0.2.0"));
        assert_eq!(newer_version("0.1.1", "V1.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(newer_version("0.1.9", "v0.1.10").as_deref(), Some("0.1.10"), "numeric, not text, order");
        assert_eq!(newer_version("0.1.1", "v0.1.1"), None, "the same version");
        assert_eq!(newer_version("0.1.1", "v0.1.0"), None, "an older version");
        assert_eq!(newer_version("0.1.1", "v0.2.0-beta.1"), None, "a pre-release");
        assert_eq!(newer_version("0.1.1", "v0.1.1+build.7"), None, "build metadata alone is not newer");
        assert_eq!(newer_version("0.2.0-dev", "v0.2.0").as_deref(), Some("0.2.0"), "the release after a pre-release build");
        assert_eq!(newer_version("0.1.1", "latest"), None);
        assert_eq!(newer_version("0.1.1", ""), None);
        assert_eq!(newer_version("0.1.1", "v0.2"), None, "not a full semantic version");
    }

    #[test]
    fn a_check_is_fresh_for_24_hours() {
        let cached = CachedCheck { checked_at: NOW, latest_tag: "v0.1.2".into(), html_url: String::new() };
        assert!(is_fresh(&cached, NOW));
        assert!(is_fresh(&cached, NOW + CHECK_INTERVAL_SECS - 1));
        assert!(!is_fresh(&cached, NOW + CHECK_INTERVAL_SECS));
        assert!(!is_fresh(&cached, NOW - 1), "a check stamped in the future is not trusted");
    }

    #[test]
    fn any_non_empty_value_turns_the_check_off() {
        assert!(!opted_out(None));
        assert!(!opted_out(Some(OsStr::new(""))));
        assert!(opted_out(Some(OsStr::new("1"))));
        assert!(opted_out(Some(OsStr::new("0"))), "any non-empty value, as documented");
        assert!(opted_out(Some(OsStr::new("yes"))));
    }

    #[test]
    fn spawn_does_nothing_in_unit_tests() {
        let slot = UpdateSlot::default();
        spawn(slot.clone());
        assert!(slot.get().is_none());
    }

    #[test]
    fn the_first_check_asks_github_and_caches_the_answer() {
        let cache = scratch("first");
        let outcome = check(&cache, NOW, "0.1.1", || release("v0.2.0"));
        assert_eq!(outcome.note, None);
        let update = outcome.update.expect("a newer release");
        assert_eq!((update.latest.as_str(), update.current.as_str()), ("0.2.0", "0.1.1"));
        assert_eq!(update.html_url, "https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.2.0");
        let stored = read_cache(&cache).expect("cached");
        assert_eq!(stored, CachedCheck { checked_at: NOW, latest_tag: "v0.2.0".into(), html_url: update.html_url.clone() });
        let text = std::fs::read_to_string(&cache).unwrap();
        assert!(text.contains("\"checked_at\"") && text.contains("\"latest_tag\"") && text.contains("\"html_url\""), "{text}");
    }

    #[test]
    fn a_fresh_cache_is_used_without_asking_and_a_stale_one_asks_again() {
        let cache = scratch("fresh");
        check(&cache, NOW, "0.1.1", || release("v0.1.2"));
        let asked = Cell::new(false);
        let outcome = check(&cache, NOW + 60, "0.1.1", || { asked.set(true); release("v9.0.0") });
        assert!(!asked.get(), "within 24 hours GitHub is not asked");
        assert_eq!(outcome.update.unwrap().latest, "0.1.2", "the cached answer");
        let updated = check(&cache, NOW + 60, "0.1.2", || unreachable!());
        assert_eq!(updated.update, None, "after updating to the cached version nothing is newer");

        let outcome = check(&cache, NOW + CHECK_INTERVAL_SECS, "0.1.1", || { asked.set(true); release("v0.1.3") });
        assert!(asked.get(), "after 24 hours GitHub is asked again");
        assert_eq!(outcome.update.unwrap().latest, "0.1.3");
        assert_eq!(read_cache(&cache).unwrap().checked_at, NOW + CHECK_INTERVAL_SECS);
    }

    #[test]
    fn a_failed_fetch_is_cached_keeps_the_known_release_and_reports_a_note() {
        let cache = scratch("failed");
        let outcome = check(&cache, NOW, "0.1.1", || Err("offline".into()));
        assert_eq!(outcome, Outcome { update: None, note: Some("offline".into()) });
        assert_eq!(read_cache(&cache).unwrap(), CachedCheck { checked_at: NOW, latest_tag: String::new(), html_url: String::new() });
        let asked = Cell::new(false);
        check(&cache, NOW + 1, "0.1.1", || { asked.set(true); Err("offline".into()) });
        assert!(!asked.get(), "a failed check also counts for the 24 hours");

        let later = NOW + CHECK_INTERVAL_SECS;
        check(&cache, later, "0.1.1", || release("v0.1.5"));
        let outcome = check(&cache, later + CHECK_INTERVAL_SECS, "0.1.1", || Err("timed out".into()));
        assert_eq!(outcome.note.as_deref(), Some("timed out"));
        assert_eq!(outcome.update.unwrap().latest, "0.1.5", "the release known before is still told");
        assert_eq!(read_cache(&cache).unwrap().latest_tag, "v0.1.5");
    }

    #[test]
    fn an_unreadable_cache_is_replaced() {
        let cache = scratch("corrupt");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, b"{not json").unwrap();
        let outcome = check(&cache, NOW, "0.1.1", || release("v0.1.2"));
        assert_eq!(outcome.update.unwrap().latest, "0.1.2");
        assert_eq!(read_cache(&cache).unwrap().latest_tag, "v0.1.2");
    }

    #[test]
    fn release_answers_are_read_and_odd_links_are_replaced() {
        let release = parse_release(r#"{"tag_name":"v0.1.2","html_url":"https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.1.2","assets":[]}"#).unwrap();
        assert_eq!(release.tag, "v0.1.2");
        assert!(parse_release(r#"{"message":"API rate limit exceeded"}"#).is_err());
        assert!(parse_release("<html>").is_err());
        assert_eq!(page_link("https://example.com/x"), RELEASES_PAGE);
        assert_eq!(page_link("https://github.com/a b"), RELEASES_PAGE);
        assert_eq!(page_link(""), RELEASES_PAGE);
        let cache = scratch("link");
        let outcome = check(&cache, NOW, "0.1.1", || Ok(Release { tag: "v0.2.0".into(), html_url: "javascript:alert(1)".into() }));
        assert_eq!(outcome.update.unwrap().html_url, RELEASES_PAGE);
    }
}
