//! Check the crates.io sparse index for newer releases using a bounded request and local cache.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};

const CRATE_NAME: &str = "ferrus";
const CACHE_TTL_HOURS: i64 = 24;
const REQUEST_TIMEOUT_SECS: u64 = 2;

#[derive(Debug, Deserialize)]
struct IndexEntry {
    vers: String,
    #[serde(default)]
    yanked: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCache {
    checked_at: DateTime<Utc>,
    latest_version: String,
}

pub async fn notification_message() -> Option<String> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    let cached = load_cache().await.ok().flatten();

    if let Some(cache) = cached.as_ref()
        && !cache_is_stale(cache)
    {
        return build_message(&current, &cache.latest_version);
    }

    match fetch_latest_version().await {
        Ok(latest_version) => {
            let _ = save_cache(&UpdateCache {
                checked_at: Utc::now(),
                latest_version: latest_version.clone(),
            })
            .await;
            build_message(&current, &latest_version)
        }
        Err(err) => {
            tracing::debug!(error = ?err, "failed to check for ferrus updates");
            cached
                .as_ref()
                .and_then(|cache| build_message(&current, &cache.latest_version))
        }
    }
}

async fn fetch_latest_version() -> Result<String> {
    let url = sparse_index_url(CRATE_NAME);
    let user_agent = format!("ferrus/{}", env!("CARGO_PKG_VERSION"));

    let body = tokio::task::spawn_blocking(move || -> Result<String> {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(REQUEST_TIMEOUT_SECS)))
            .user_agent(user_agent)
            .build();
        let agent: ureq::Agent = config.into();

        let mut response = agent
            .get(&url)
            .call()
            .context("failed to fetch crates.io sparse index")?;

        response
            .body_mut()
            .read_to_string()
            .context("failed to read crates.io sparse index response")
    })
    .await
    .context("update check task failed to join")??;

    newest_non_yanked_version(&body).context("no non-yanked version found in crates.io index")
}

fn newest_non_yanked_version(body: &str) -> Result<String> {
    let mut newest: Option<Version> = None;

    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        let entry: IndexEntry =
            serde_json::from_str(line).context("failed to parse crates.io index entry")?;
        if entry.yanked {
            continue;
        }

        let version = Version::parse(&entry.vers)
            .with_context(|| format!("failed to parse crates.io version '{}'", entry.vers))?;

        match newest.as_ref() {
            Some(current) if &version <= current => {}
            _ => newest = Some(version),
        }
    }

    newest
        .map(|version| version.to_string())
        .context("no versions found")
}

fn build_message(current: &Version, latest: &str) -> Option<String> {
    let latest = Version::parse(latest).ok()?;
    if latest <= *current {
        return None;
    }

    Some(format!(
        "A newer ferrus version is available: {latest} (current: {current}). Update with `cargo install ferrus` or rerun the install script."
    ))
}

fn sparse_index_url(crate_name: &str) -> String {
    format!(
        "https://index.crates.io/{}",
        sparse_index_path(crate_name.to_ascii_lowercase().as_str())
    )
}

fn sparse_index_path(crate_name: &str) -> String {
    match crate_name.len() {
        1 => format!("1/{crate_name}"),
        2 => format!("2/{crate_name}"),
        3 => format!("3/{}/{}", &crate_name[..1], crate_name),
        _ => format!("{}/{}/{}", &crate_name[..2], &crate_name[2..4], crate_name),
    }
}

async fn load_cache() -> Result<Option<UpdateCache>> {
    let Some(path) = cache_file_path() else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }

    let content = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read cache file {}", path.display()))?;
    let cache = serde_json::from_str(&content).context("failed to parse update check cache")?;
    Ok(Some(cache))
}

async fn save_cache(cache: &UpdateCache) -> Result<()> {
    let Some(path) = cache_file_path() else {
        return Ok(());
    };
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("failed to create cache directory {}", parent.display()))?;
    let content = serde_json::to_string(cache).context("failed to serialize update cache")?;
    tokio::fs::write(&path, content)
        .await
        .with_context(|| format!("failed to write cache file {}", path.display()))?;
    Ok(())
}

fn cache_is_stale(cache: &UpdateCache) -> bool {
    cache.checked_at + ChronoDuration::hours(CACHE_TTL_HOURS) <= Utc::now()
}

fn cache_file_path() -> Option<PathBuf> {
    let mut path = dirs::cache_dir()?;
    path.push("ferrus");
    path.push("update-check.json");
    Some(path)
}

#[cfg(test)]
mod tests {
    //! Sparse-index paths, release selection, and cache expiration.

    use super::{
        UpdateCache, build_message, cache_is_stale, newest_non_yanked_version, sparse_index_path,
    };
    use chrono::{Duration, Utc};
    use semver::Version;

    #[test]
    fn sparse_index_path_matches_cargo_layout() {
        assert_eq!(sparse_index_path("a"), "1/a");
        assert_eq!(sparse_index_path("ab"), "2/ab");
        assert_eq!(sparse_index_path("abc"), "3/a/abc");
        assert_eq!(sparse_index_path("ferrus"), "fe/rr/ferrus");
    }

    #[test]
    fn newest_non_yanked_version_skips_yanked_entries() {
        let body = r#"{"vers":"0.2.4","yanked":false}
{"vers":"0.2.5","yanked":true}
{"vers":"0.2.6","yanked":false}"#;

        assert_eq!(
            newest_non_yanked_version(body).expect("version should parse"),
            "0.2.6"
        );
    }

    #[test]
    fn build_message_returns_none_when_current_is_latest() {
        let current = Version::parse("0.2.5").expect("current version should parse");
        assert!(build_message(&current, "0.2.5").is_none());
        assert!(build_message(&current, "0.2.4").is_none());
    }

    #[test]
    fn build_message_detects_newer_prerelease() {
        let current = Version::parse("0.2.5-alpha.5").expect("current version should parse");
        let message = build_message(&current, "0.2.5-alpha.6")
            .expect("newer prerelease should produce a message");

        assert!(message.contains("0.2.5-alpha.6"));
        assert!(message.contains("0.2.5-alpha.5"));
    }

    #[test]
    fn cache_staleness_uses_ttl_window() {
        let fresh = UpdateCache {
            checked_at: Utc::now() - Duration::hours(1),
            latest_version: "0.2.5".to_string(),
        };
        let stale = UpdateCache {
            checked_at: Utc::now() - Duration::hours(25),
            latest_version: "0.2.5".to_string(),
        };

        assert!(!cache_is_stale(&fresh));
        assert!(cache_is_stale(&stale));
    }
}
