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
                out.push_str(&format!("{marker} {number:>width$} \u{2502} {line}\n"));
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
        Diagnosis::EmptyCorpus => "No matches: the corpus is empty. Ask the user to index \
             repositories first, e.g. `steroids add owner/name`."
            .to_string(),
        Diagnosis::NearMiss { missing, nearest } => format!(
            "No matches for '{missing}', but the corpus does contain '{nearest}'. {scope} \
             Likely a spelling or naming difference: retry searching for '{nearest}'."
        ),
        Diagnosis::TopicAbsent { missing } => format!(
            "No matches, and '{missing}' does not appear anywhere in the corpus. {scope} \
             The indexed projects likely do not cover this topic. Tell the user which \
             repositories would need indexing (`steroids add owner/name`) rather than \
             retrying variations of this search."
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
