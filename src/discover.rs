//! Finding repositories worth indexing, via the GitHub search API.
//!
//! "Trending" is not a public API, so it is expressed as a search: recently
//! pushed repositories ordered by stars.

use anyhow::{Context, Result, bail};

use crate::fetch;

/// GitHub caps a search page at 100 results.
const MAX_PER_PAGE: usize = 100;

pub struct Candidate {
    pub repo: String,
    pub stars: i64,
    pub pushed_at: String,
    pub language: String,
    pub description: String,
}

/// Search GitHub for repositories matching `query`.
///
/// `query` is raw search qualifiers, e.g. `topic:ai-agents language:python`.
/// Star and archive filters are appended so every caller gets them.
pub fn search(query: &str, min_stars: u32, limit: usize) -> Result<Vec<Candidate>> {
    if query.trim().is_empty() {
        bail!("empty discovery query");
    }
    let limit = limit.clamp(1, MAX_PER_PAGE);
    // Two `stars:` qualifiers are ANDed by GitHub, so appending ours to a query
    // that already has one silently narrows to the intersection. Defer to what
    // the user wrote.
    let mut full = query.trim().to_string();
    if !full.contains("stars:") {
        full.push_str(&format!(" stars:>={min_stars}"));
    }
    // Archived repositories are frozen by definition, so never suggest them.
    if !full.contains("archived:") {
        full.push_str(" archived:false");
    }
    let url = format!(
        "https://api.github.com/search/repositories?q={}&sort=stars&order=desc&per_page={limit}",
        urlencode(&full)
    );

    let body = fetch::get_json(&url).context("searching GitHub")?;
    let payload: serde_json::Value = serde_json::from_str(&body)?;
    if let Some(message) = payload["message"].as_str() {
        bail!("GitHub search failed: {message}");
    }

    let items = payload["items"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unexpected search response"))?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let repo = item["full_name"].as_str()?;
            // The API is a network boundary; its names still have to survive
            // the same validation as anything a user types.
            fetch::validate_repo(repo).ok()?;
            Some(Candidate {
                repo: repo.to_string(),
                stars: item["stargazers_count"].as_i64().unwrap_or(0),
                pushed_at: item["pushed_at"].as_str().unwrap_or_default().to_string(),
                language: item["language"].as_str().unwrap_or("-").to_string(),
                description: item["description"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(100)
                    .collect(),
            })
        })
        .collect())
}

/// Repositories with recent activity, most-starred first.
///
/// GitHub's trending page has no API, so approximate it: anything pushed in the
/// last `days`, ranked by stars.
pub fn trending(
    days: u32,
    language: Option<&str>,
    min_stars: u32,
    limit: usize,
) -> Result<Vec<Candidate>> {
    let since = days_ago(days);
    let mut query = format!("pushed:>{since}");
    if let Some(language) = language {
        query.push_str(&format!(" language:{language}"));
    }
    search(&query, min_stars, limit)
}

/// An ISO date `days` before today, computed from the Unix epoch so no date
/// library is needed.
fn days_ago(days: u32) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target = now.saturating_sub(days as u64 * 86_400);
    iso_date(target)
}

/// Convert a Unix timestamp to `YYYY-MM-DD` (civil-from-days, Howard Hinnant).
pub fn iso_date(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Percent-encode everything outside the unreserved set.
///
/// The query is user-supplied and goes into a URL; `+` must survive as a space
/// separator rather than being read as a literal plus.
fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_safely() {
        assert_eq!(
            urlencode("topic:ai stars:>=10"),
            "topic%3Aai+stars%3A%3E%3D10"
        );
        assert_eq!(urlencode("a/b"), "a%2Fb");
    }

    #[test]
    fn converts_timestamps_to_dates() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(1_000_000_000), "2001-09-09");
        assert_eq!(iso_date(1_700_000_000), "2023-11-14");
    }

    #[test]
    fn rejects_an_empty_query() {
        assert!(search("   ", 0, 10).is_err());
    }

    #[test]
    fn does_not_duplicate_user_qualifiers() {
        // Building the query is the part worth asserting; the request itself
        // needs the network.
        let build = |query: &str, min_stars: u32| {
            let mut full = query.trim().to_string();
            if !full.contains("stars:") {
                full.push_str(&format!(" stars:>={min_stars}"));
            }
            if !full.contains("archived:") {
                full.push_str(" archived:false");
            }
            full
        };
        assert_eq!(
            build("topic:mcp", 100),
            "topic:mcp stars:>=100 archived:false"
        );
        assert_eq!(
            build("topic:mcp stars:>5000", 100),
            "topic:mcp stars:>5000 archived:false",
            "user star filter was overridden"
        );
        assert_eq!(
            build("topic:mcp archived:true", 100),
            "topic:mcp archived:true stars:>=100"
        );
    }
}
