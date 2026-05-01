// ── App-update checker ──────────────────────────────────────
//
// Lightweight GitHub-Releases-based update flow. We do not bundle the new
// binary in-app — installing requires a fresh download from the user's
// browser — but we do detect new versions and surface them in:
//
//   • the Help menu ("Check for Updates…") — interactive
//   • the Settings page Updates card — manual + status
//   • a startup dialog when `updates_auto_check` is on and we haven't
//     already announced this exact version
//
// Why GitHub Releases instead of `tauri-plugin-updater`? The latter needs
// a signing key + hosted manifest. This project's release pipeline builds
// unsigned `.dmg`/`.deb`/`.AppImage`/`.msi`/`.exe` artifacts already and
// publishes them on `v*` tags, so reusing that as the source-of-truth
// avoids new infrastructure and keeps OSS forks easy to ship.

use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::{save_config, SharedConfig};

const RELEASES_API: &str =
    "https://api.github.com/repos/girafic/penpot-desktop/releases/latest";
const USER_AGENT: &str = "PenpotDesktop-Updater";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub available: bool,
    pub release_url: String,
    pub release_notes: String,
    pub published_at: String,
    pub checked_at: i64,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

/// In-memory cache of the most recent check result. Settings/menu callers
/// can read this without re-hitting the network.
static LAST_RESULT: OnceLock<RwLock<Option<UpdateInfo>>> = OnceLock::new();

fn cache() -> &'static RwLock<Option<UpdateInfo>> {
    LAST_RESULT.get_or_init(|| RwLock::new(None))
}

pub async fn cached() -> Option<UpdateInfo> {
    cache().read().await.clone()
}

/// Strip a leading `v`/`V` from a tag (e.g. `v1.0.2` → `1.0.2`).
fn normalize_tag(raw: &str) -> &str {
    raw.trim_start_matches(|c: char| c == 'v' || c == 'V')
}

/// Numeric semver compare. Returns `Some(Ordering)` when both sides parse
/// as dot-separated integers; otherwise falls back to string compare so a
/// non-semver tag still produces a deterministic answer.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let l = normalize_tag(latest);
    let c = normalize_tag(current);

    let parse = |s: &str| -> Option<Vec<u64>> {
        s.split('.')
            .map(|part| {
                // Drop any `-rc1`, `+build`, etc. suffix on the component.
                let num: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
                num.parse::<u64>().ok()
            })
            .collect()
    };

    match (parse(l), parse(c)) {
        (Some(a), Some(b)) => {
            let max = a.len().max(b.len());
            for i in 0..max {
                let av = a.get(i).copied().unwrap_or(0);
                let bv = b.get(i).copied().unwrap_or(0);
                if av > bv {
                    return true;
                }
                if av < bv {
                    return false;
                }
            }
            false
        }
        _ => l > c,
    }
}

/// Fetch the latest release from GitHub and compare it to `current`.
/// Returns `Err` only on network/parse failures — a successful check that
/// finds no update still resolves to `Ok(UpdateInfo { available: false })`.
pub async fn check(current: &str) -> Result<UpdateInfo, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .get(RELEASES_API)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("GitHub API returned {}", resp.status()));
    }

    // reqwest is built without the `json` feature, so deserialize manually.
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    let release: GitHubRelease =
        serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    if release.draft || release.prerelease {
        // Don't announce drafts/prereleases as updates — but still surface
        // them through the cache so manual checks can show the result.
        let info = UpdateInfo {
            current_version: current.to_string(),
            latest_version: normalize_tag(&release.tag_name).to_string(),
            available: false,
            release_url: release.html_url,
            release_notes: release.body,
            published_at: release.published_at,
            checked_at: now_secs(),
        };
        *cache().write().await = Some(info.clone());
        return Ok(info);
    }

    let info = UpdateInfo {
        current_version: current.to_string(),
        latest_version: normalize_tag(&release.tag_name).to_string(),
        available: is_newer(&release.tag_name, current),
        release_url: release.html_url,
        release_notes: release.body,
        published_at: release.published_at,
        checked_at: now_secs(),
    };

    *cache().write().await = Some(info.clone());
    Ok(info)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Persist `updates_last_check` to disk.
pub async fn record_check(config: &SharedConfig) {
    let mut c = config.write().await;
    c.updates_last_check = now_secs();
    save_config(&c);
}

/// Persist the announced version so we don't re-prompt the user about it
/// on every launch.
pub async fn record_announced(config: &SharedConfig, version: &str) {
    let mut c = config.write().await;
    c.updates_last_announced = version.to_string();
    save_config(&c);
}

/// Truncate a release-notes body for use in a native dialog, which renders
/// poorly when given many lines of Markdown.
pub fn shorten_notes(notes: &str, max_chars: usize) -> String {
    let stripped: String = notes.lines().take(8).collect::<Vec<_>>().join("\n");
    if stripped.chars().count() <= max_chars {
        return stripped;
    }
    let truncated: String = stripped.chars().take(max_chars).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_detects_minor_bumps() {
        assert!(is_newer("v1.0.3", "1.0.2"));
        assert!(is_newer("1.1.0", "1.0.99"));
        assert!(is_newer("v2.0.0", "1.99.99"));
    }

    #[test]
    fn newer_handles_equal_and_older() {
        assert!(!is_newer("1.0.2", "1.0.2"));
        assert!(!is_newer("v1.0.2", "1.0.2"));
        assert!(!is_newer("1.0.1", "1.0.2"));
    }

    #[test]
    fn newer_tolerates_short_versions() {
        assert!(is_newer("1.1", "1.0.5"));
        assert!(!is_newer("1.0", "1.0.0"));
    }

    #[test]
    fn newer_strips_pre_release_suffix() {
        assert!(is_newer("1.0.3-rc1", "1.0.2"));
    }

    #[test]
    fn shorten_keeps_short_notes() {
        assert_eq!(shorten_notes("hello", 100), "hello");
    }

    #[test]
    fn shorten_truncates_long_notes() {
        let s = "a".repeat(500);
        let out = shorten_notes(&s, 200);
        assert!(out.chars().count() <= 201); // 200 + ellipsis
        assert!(out.ends_with('…'));
    }
}
