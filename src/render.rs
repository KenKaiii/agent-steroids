//! Turning results into what a coding agent should read.
//!
//! Agent context is the scarce resource, not disk or latency, so every line
//! printed has to earn its tokens.

use crate::search::{Diagnosis, Facts, Match};

/// Machine-readable results, for callers that parse rather than read.
pub fn render_matches_json(matches: &[Match], pattern: &str) -> String {
    let items: Vec<serde_json::Value> = matches
        .iter()
        .map(|item| {
            serde_json::json!({
                "repo": item.repo,
                "path": item.path,
                "line": item.line_number,
                "url": item.permalink(),
                "scope": item.scope,
                "context": item.context,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "pattern": pattern,
        "count": items.len(),
        "matches": items,
    }))
    .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"))
}

/// Machine-readable explanation of an empty result set.
pub fn render_empty_json(facts: &Facts, pattern: &str) -> String {
    let reason = match &facts.diagnosis {
        Diagnosis::EmptyCorpus => "empty_corpus",
        Diagnosis::NearMiss { .. } => "near_miss",
        Diagnosis::TopicAbsent { .. } => "topic_absent",
        Diagnosis::SpellingMismatch { .. } => "spelling_mismatch",
        Diagnosis::TooBroad => "pattern_too_broad",
    };
    let suggestion = match &facts.diagnosis {
        Diagnosis::NearMiss { nearest, .. } => Some(nearest.clone()),
        _ => None,
    };
    serde_json::to_string_pretty(&serde_json::json!({
        "pattern": pattern,
        "count": 0,
        "matches": [],
        "reason": reason,
        "suggestion": suggestion,
        "repositories": facts.repos,
        "languages": facts.languages,
        "advice": render_empty(facts),
    }))
    .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"))
}

/// Longest context line shown. Generated or data-heavy source can hold a
/// single line of hundreds of kilobytes, and one such match would otherwise
/// fill a reader's whole context with something they cannot use.
const MAX_LINE_CHARS: usize = 400;

/// Shorten a line to something readable, noting what was cut.
fn clamp(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let kept: String = line.chars().take(MAX_LINE_CHARS).collect();
    let dropped = line.chars().count() - MAX_LINE_CHARS;
    format!("{kept}… [{dropped} more characters]")
}

/// A repository with no commit in this long is worth flagging: its patterns may
/// predate current practice in a field that moves quickly.
const STALE_AFTER_DAYS: u64 = 365;

/// Whether an ISO date is older than the staleness threshold.
fn is_stale(pushed_at: &str) -> bool {
    if pushed_at.len() < 10 {
        // Unknown, so say nothing rather than implying it is fresh or stale.
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = crate::discover::iso_date(now.saturating_sub(STALE_AFTER_DAYS * 86_400));
    &pushed_at[..10] < cutoff.as_str()
}

pub fn render_matches(matches: &[Match], header: &str) -> String {
    let mut out = String::from(header);
    out.push('\n');

    // Results are interleaved across repositories for fairness, so hits in one
    // file arrive scattered. Group them first, or the same file header is
    // printed several times and burns the reader's attention for nothing.
    let mut order: Vec<(&str, &str)> = Vec::new();
    let mut grouped: std::collections::HashMap<(&str, &str), Vec<&Match>> = Default::default();
    for item in matches {
        let key = (item.repo.as_str(), item.path.as_str());
        grouped.entry(key).or_default().push(item);
        if !order.contains(&key) {
            order.push(key);
        }
    }

    for key in order {
        let items = &grouped[&key];
        // Flag stale sources inline. A pattern from a project that stopped
        // moving years ago may well be out of date, and the reader cannot know
        // that from the snippet alone.
        let stale = items
            .first()
            .map(|item| is_stale(&item.pushed_at))
            .unwrap_or(false);
        out.push_str(&format!(
            "\nRepo: {}{}\n",
            key.0,
            if stale { "  (last commit " } else { "" }
        ));
        if stale {
            // Close the marker with the actual date, so it is checkable.
            let date = items
                .first()
                .map(|i| i.pushed_at.chars().take(10).collect::<String>())
                .unwrap_or_default();
            out.truncate(out.len() - 1);
            out.push_str(&format!("{date})\n"));
        }
        out.push_str(&format!("File: {}\n", key.1));
        for item in items {
            // A gutter of real line numbers lets the agent cite an exact
            // location and lets a person scroll straight to it.
            out.push('\n');
            // Skip the scope line when the match is the definition itself, or
            // when the definition is already visible in the context below it.
            let start = item.context_start();
            let scope_shown = item
                .context
                .iter()
                .any(|line| line.trim() == item.scope.trim());
            if !item.scope.is_empty() && !scope_shown {
                out.push_str(&format!("  in {}\n", item.scope));
            }
            let width = (start + item.context.len()).to_string().len();
            for (offset, line) in item.context.iter().enumerate() {
                let number = start + offset;
                let marker = if number == item.line_number { ">" } else { " " };
                out.push_str(&format!(
                    "{marker} {number:>width$} \u{2502} {}\n",
                    clamp(line)
                ));
            }
        }
    }
    out
}

/// Say why a search returned nothing, and what to do about it.
///
/// A bare "no matches" invites blind retries. Naming the cause lets the agent
/// either rephrase or tell the user the corpus needs more repositories.
pub fn render_empty(facts: &Facts) -> String {
    let scope = format!(
        "Corpus holds {} repositories ({}).",
        facts.repos,
        facts.languages.join(", ")
    );
    match &facts.diagnosis {
        Diagnosis::EmptyCorpus => "No matches: the corpus is empty, so there is \
             nothing to search yet.\n\n\
             Offer to fill it: look at what the user is building, run \
             `steroids discover '<topic>' --limit 20` to find well starred, actively \
             maintained projects that solve similar problems, then \
             `steroids add <repos> && steroids index` once they agree."
            .to_string(),
        Diagnosis::NearMiss { missing, nearest } => format!(
            "No matches for '{missing}', but the corpus does contain '{nearest}'. {scope} \
             Likely a spelling or naming difference: retry searching for '{nearest}'."
        ),
        // The corpus genuinely does not cover this, so rephrasing will not
        // help. Hand over the commands that fix it instead, and tell the
        // caller to check with the user before spending their disk.
        Diagnosis::TopicAbsent { missing } => format!(
            "No code for '{missing}' in this corpus. {scope}\n\n\
             This is a gap in what is indexed, not a bad search, so do not retry \
             variations. Fill the gap instead:\n\
             \x20 1. Find candidates:  steroids discover '<topic or language>' --limit 20\n\
             \x20 2. Tell the user what you found and why it fits their project\n\
             \x20 3. With their go-ahead: steroids add <repos> --tag <label> && steroids index\n\
             \x20 4. Re-run this search\n\n\
             If you cannot reach GitHub, ask the user for repository names that solve \
             this problem and add those."
        ),
        Diagnosis::SpellingMismatch { known } => format!(
            "No matches for this exact pattern, but '{known}' does appear in the corpus. \
             {scope} Retry with a shorter or looser pattern, or --ignore-case, before \
             concluding the code is absent."
        ),
        Diagnosis::TooBroad => format!(
            "No matches. {scope} The pattern has no literal run of 3+ characters to search \
             on; add one, e.g. a function or parameter name."
        ),
    }
}

#[cfg(test)]
mod line_length_tests {
    use crate::search::Match;

    /// A match inside a very long line must not dump the whole line into the
    /// reader's context. Minified files are filtered out at ingest, but a
    /// legitimate source file can still hold one enormous generated line.
    #[test]
    fn long_match_line_is_truncated() {
        let huge = format!("const DATA = '{}'; // needle", "x".repeat(150_000));
        let item = Match {
            repo: "a/b".into(),
            path: "src/big.js".into(),
            line_number: 1,
            context: vec![huge],
            scope: String::new(),
            commit_sha: String::new(),
            context_first_line: 1,
            pushed_at: String::new(),
        };
        let out = super::render_matches(std::slice::from_ref(&item), "1 match");
        assert!(
            out.len() < 4_000,
            "one match produced {} bytes of output",
            out.len()
        );
    }
}
