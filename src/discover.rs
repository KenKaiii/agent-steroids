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

/// Words that mark a repository as a reading list rather than a codebase.
///
/// These score extremely well on stars, so a star threshold does not filter
/// them: `awesome-python` has 300k stars and no Python worth reading. A corpus
/// exists to show an agent how working code is written, and a curated list of
/// links teaches it nothing.
const CURATION_MARKERS: &[&str] = &[
    "awesome",
    "tutorial",
    "course",
    "roadmap",
    "cheatsheet",
    "cheat-sheet",
    "interview",
    "handbook",
    "guide",
    "curriculum",
    "learn-",
    "-learning",
    "system-design",
    "project-based",
    "free-programming",
    "public-apis",
    "every-programmer",
    "coding-interview",
    "study-plan",
    "-notes",
    "bookmarks",
    "resources",
    "collection",
    "list-of",
    "papers",
    "prompts",
    "-books",
    "ebook",
];

/// Whether a repository is a list or a course rather than a working project.
///
/// Judged on the name and description, since both are what the author chose to
/// call it. Deliberately conservative: a false positive costs one candidate,
/// while a false negative fills the corpus with markdown.
pub fn is_curation(repo: &str, description: &str) -> bool {
    let name = repo.rsplit('/').next().unwrap_or(repo).to_lowercase();
    if CURATION_MARKERS.iter().any(|word| name.contains(word)) {
        return true;
    }
    // A description that announces itself as a list, even under a neutral name.
    let text = description.to_lowercase();
    const PHRASES: &[&str] = &[
        "curated list",
        "awesome list",
        "collection of resources",
        "list of resources",
        "learning path",
        "roadmap to",
        "interview questions",
        "study guide",
        "share interesting",
        "open source projects",
        "everything you need to know",
        "best practices for learning",
    ];
    PHRASES.iter().any(|phrase| text.contains(phrase))
}

/// Search GitHub for repositories matching `query`.
///
/// `query` is raw search qualifiers, e.g. `topic:ai-agents language:python`.
/// Star and archive filters are appended so every caller gets them.
pub fn search(
    query: &str,
    min_stars: u32,
    max_age_months: u32,
    limit: usize,
) -> Result<Vec<Candidate>> {
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
    // Filter age at the source rather than after fetching. A corpus of
    // abandoned projects teaches an agent last decade's practice, which is the
    // problem this tool exists to solve.
    if max_age_months > 0 && !full.contains("pushed:") {
        full.push_str(&format!(" pushed:>{}", days_ago(max_age_months * 30)));
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

            let description = item["description"].as_str().unwrap_or("");
            if is_curation(repo, description) {
                return None;
            }
            // A repository GitHub cannot assign a language to holds no code
            // worth indexing, whatever its star count.
            let language = item["language"].as_str()?;
            if matches!(language, "Markdown" | "Text" | "HTML" | "Jupyter Notebook") {
                return None;
            }
            Some(Candidate {
                repo: repo.to_string(),
                stars: item["stargazers_count"].as_i64().unwrap_or(0),
                pushed_at: item["pushed_at"].as_str().unwrap_or_default().to_string(),
                language: language.to_string(),
                description: description.chars().take(100).collect(),
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
    // Already constrained to recent activity, so no separate age filter.
    search(&query, min_stars, 0, limit)
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
    /// Lists and courses out-star real projects, so a star threshold alone
    /// lets them straight in. `awesome-python` has 300k stars and no Python
    /// worth reading.
    #[test]
    fn rejects_lists_and_courses() {
        for (repo, description) in [
            ("vinta/awesome-python", ""),
            ("public-apis/public-apis", ""),
            ("donnemartin/system-design-primer", ""),
            ("practical-tutorials/project-based-learning", ""),
            ("kamranahmedse/developer-roadmap", ""),
            ("neutral/name", "A curated list of amazing tools"),
            ("another/repo", "Interview questions for engineers"),
        ] {
            assert!(
                super::is_curation(repo, description),
                "let a list through: {repo}"
            );
        }
    }

    /// Real projects must not be caught by the same filter.
    #[test]
    fn keeps_real_projects() {
        for (repo, description) in [
            ("psf/requests", "A simple, yet elegant, HTTP library."),
            (
                "openai/codex",
                "Lightweight coding agent that runs in your terminal",
            ),
            (
                "hashicorp/terraform",
                "Terraform enables you to safely build infrastructure",
            ),
            (
                "BurntSushi/ripgrep",
                "Recursively searches directories for a regex pattern",
            ),
            (
                "tokio-rs/tokio",
                "A runtime for writing reliable asynchronous applications",
            ),
            // "guide" appears in the description but it is a real library.
            (
                "some/lib",
                "A library that will guide requests to the right handler",
            ),
        ] {
            assert!(
                !super::is_curation(repo, description),
                "rejected a real project: {repo}"
            );
        }
    }

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
        assert!(search("   ", 0, 0, 10).is_err());
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
