//! What changed upstream recently.
//!
//! An indexed corpus is a snapshot. This answers the other question: what have
//! these projects actually done in the last few days? Useful for tracking a
//! field that moves weekly, where the newest commits say more about current
//! practice than the code sitting in the corpus.
//!
//! Reads each repository's commit feed, which GitHub publishes as Atom. That
//! endpoint is not rate limited, so this scales to a whole corpus without a
//! token, unlike the REST commits API.

use anyhow::Result;

use crate::fetch;

/// Commits held in one feed. GitHub does not paginate it.
const FEED_LIMIT: usize = 20;

pub struct Commit {
    pub repo: String,
    /// ISO timestamp, e.g. `2026-08-25T00:45:57`.
    pub when: String,
    pub author: String,
    pub title: String,
    pub url: String,
}

/// Commits across many repositories, fetched concurrently.
///
/// Each feed is a separate request, so checking a whole corpus sequentially
/// would take minutes. A repository whose feed fails is skipped rather than
/// failing the run: one renamed or deleted project should not hide what every
/// other one did this week.
pub fn for_repos(repos: &[String], hours: u32, parallel: usize) -> Vec<Commit> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let cursor = AtomicUsize::new(0);
    let collected: Mutex<Vec<Commit>> = Mutex::new(Vec::new());
    let workers = parallel.clamp(1, 32).min(repos.len().max(1));

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(repo) = repos.get(index) else { return };
                    if let Ok(found) = for_repo(repo, hours)
                        && !found.is_empty()
                    {
                        collected.lock().expect("poisoned").extend(found);
                    }
                }
            });
        }
    });

    collected.into_inner().expect("poisoned")
}

/// Commits to one repository within the last `hours`.
pub fn for_repo(stored: &str, hours: u32) -> Result<Vec<Commit>> {
    let (host, bare) = fetch::split_host(stored);
    if host != fetch::Host::GitHub {
        // Only GitHub publishes this feed.
        return Ok(Vec::new());
    }
    let body = fetch::get_text(
        &format!("https://github.com/{bare}/commits/HEAD.atom"),
        "application/atom+xml",
    )?;

    let cutoff = cutoff_timestamp(hours);
    let mut commits = Vec::new();
    for entry in body.split("<entry>").skip(1).take(FEED_LIMIT) {
        let when = field(entry, "updated").unwrap_or_default();
        // Timestamps are ISO 8601 in UTC, so a string compare orders them.
        if when.as_str() < cutoff.as_str() {
            continue;
        }
        commits.push(Commit {
            repo: stored.to_string(),
            when: when.chars().take(19).collect(),
            author: field(entry, "name").unwrap_or_default(),
            // Titles wrap across lines in the feed; collapse to one.
            title: field(entry, "title")
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(100)
                .collect(),
            url: attribute(entry, "href=").unwrap_or_default(),
        });
    }
    Ok(commits)
}

/// ISO timestamp `hours` before now, for comparing against feed entries.
fn cutoff_timestamp(hours: u32) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target = now.saturating_sub(hours as u64 * 3600);
    let seconds_today = target % 86_400;
    format!(
        "{}T{:02}:{:02}:{:02}",
        crate::discover::iso_date(target),
        seconds_today / 3600,
        (seconds_today % 3600) / 60,
        seconds_today % 60
    )
}

/// Text of the first `<tag>` in a fragment.
///
/// A hand-rolled reader rather than an XML dependency: the feed is a fixed,
/// machine-generated shape and only four fields are needed from it.
fn field(fragment: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = fragment.find(&open)? + open.len();
    let end = fragment[start..].find(&close)? + start;
    Some(unescape(fragment[start..end].trim()))
}

/// Value of the first attribute starting with `prefix`.
fn attribute(fragment: &str, prefix: &str) -> Option<String> {
    let start = fragment.find(prefix)? + prefix.len();
    let rest = &fragment[start..];
    let quote = rest.chars().next()?;
    let end = rest[1..].find(quote)? + 1;
    Some(unescape(&rest[1..end]))
}

fn unescape(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<feed><entry>
      <id>tag:github.com,2008:Grit::Commit/abc123</id>
      <link rel="alternate" type="text/html" href="https://github.com/a/b/commit/abc123"/>
      <title>Fix &amp; tidy the parser</title>
      <updated>2026-08-25T00:45:57Z</updated>
      <author><name>someone</name></author>
    </entry></feed>"#;

    #[test]
    fn reads_a_feed_entry() {
        let entry = SAMPLE.split("<entry>").nth(1).unwrap();
        assert_eq!(field(entry, "title").unwrap(), "Fix & tidy the parser");
        assert_eq!(field(entry, "name").unwrap(), "someone");
        assert_eq!(field(entry, "updated").unwrap(), "2026-08-25T00:45:57Z");
        assert_eq!(
            attribute(entry, "href=").unwrap(),
            "https://github.com/a/b/commit/abc123"
        );
    }

    #[test]
    fn cutoff_is_ordered_before_now() {
        let day = cutoff_timestamp(24);
        let hour = cutoff_timestamp(1);
        assert!(day < hour, "{day} should sort before {hour}");
        assert_eq!(day.len(), 19);
    }
}
