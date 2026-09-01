//! Turning results into what a coding agent should read.
//!
//! Agent context is the scarce resource, not disk or latency, so every line
//! printed has to earn its tokens.

use crate::search::{Diagnosis, Facts, Match, SearchResults};

/// Machine-readable results, for callers that parse rather than read.
///
/// The token budget applies here exactly as it does to text. JSON is what an
/// agent reads, so it is the output most likely to land in a context window;
/// ripgrep's JSON printer leaves truncation to the consumer, but its consumer
/// is a program with unbounded memory, not a window that overflows silently.
pub fn render_matches_json(
    results: &SearchResults,
    pattern: &str,
    unindexed: &[String],
    budget: usize,
) -> String {
    let render = |shown: &[Match]| {
        let items: Vec<serde_json::Value> = shown
            .iter()
            .map(|item| {
                serde_json::json!({
                    "repo": item.repo,
                    "path": item.path,
                    "line": item.line_number,
                    // Where `context` starts, so a caller can number the lines it
                    // was handed instead of guessing the context width.
                    "context_first_line": item.context_start(),
                    "url": item.permalink(),
                    "scope": item.scope,
                    "context": item.context.iter().map(|line| clamp(line)).collect::<Vec<_>>(),
                })
            })
            .collect();
        serde_json::to_string_pretty(&serde_json::json!({
            "pattern": pattern,
            "count": items.len(),
            // The text output says so in its header; without it here an agent
            // reading JSON cannot tell a complete answer from a capped one.
            "more_available": results.more_available,
            // Matches found but cut to honour the token budget. Distinct from
            // `more_available`, which is about the search, not the output.
            "omitted": results.len() - shown.len(),
            // Repositories that could not have appeared above, whatever they hold.
            // A count plus a few names: the full list of a corpus that was never
            // indexed is hundreds of lines nobody asked for.
            "unindexed_count": unindexed.len(),
            "unindexed_repositories": &unindexed[..unindexed.len().min(UNINDEXED_SHOWN)],
            "matches": items,
        }))
        .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"))
    };
    fit_within(results, budget, render).0
}

/// Machine-readable explanation of an empty result set.
pub fn render_empty_json(facts: &Facts, pattern: &str, unindexed: &[String]) -> String {
    let reason = match &facts.diagnosis {
        Diagnosis::EmptyCorpus => "empty_corpus",
        Diagnosis::NearMiss { .. } => "near_miss",
        Diagnosis::TopicAbsent { .. } => "topic_absent",
        Diagnosis::SpellingMismatch { .. } => "spelling_mismatch",
        Diagnosis::TooBroad => "pattern_too_broad",
        Diagnosis::CrossLine => "pattern_spans_lines",
        Diagnosis::FilterExcludesAll { .. } => "filter_excludes_all",
    };
    let suggestion = match &facts.diagnosis {
        Diagnosis::NearMiss { nearest, .. } => Some(nearest.clone()),
        _ => None,
    };
    serde_json::to_string_pretty(&serde_json::json!({
        "pattern": pattern,
        "count": 0,
        // Always present so both result shapes carry the same keys: a caller
        // reading `more_available` should not have to handle it going missing
        // just because the search happened to find nothing.
        "more_available": false,
        "omitted": 0,
        "matches": [],
        "reason": reason,
        "suggestion": suggestion,
        "repositories": facts.repos,
        "languages": facts.languages,
        "unindexed_count": unindexed.len(),
        "unindexed_repositories": &unindexed[..unindexed.len().min(UNINDEXED_SHOWN)],
        "advice": format!("{}{}", unindexed_note(unindexed), render_empty(facts)),
    }))
    .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"))
}

/// One line telling the reader which repositories a result cannot include.
///
/// Empty when everything is indexed, which is the normal case. Names are
/// listed up to a handful so the agent can recognise the one it just added,
/// and the fix is spelled out because the alternative is a wild goose chase
/// through `discover`.
/// How many unindexed repository names an answer carries by name.
const UNINDEXED_SHOWN: usize = 5;

pub fn unindexed_note(unindexed: &[String]) -> String {
    if unindexed.is_empty() {
        return String::new();
    }
    let shown: Vec<&str> = unindexed
        .iter()
        .take(UNINDEXED_SHOWN)
        .map(String::as_str)
        .collect();
    let more = if unindexed.len() > shown.len() {
        format!(" and {} more", unindexed.len() - shown.len())
    } else {
        String::new()
    };
    format!(
        "Note: {} repositories are not yet indexed and cannot appear here ({}{more}). \
         Run `steroids index` first.\n\n",
        unindexed.len(),
        shown.join(", ")
    )
}

/// Rough token count for a block of code.
///
/// Code tokenises at roughly one token per four characters, denser than prose
/// because of punctuation and symbols. Taking the larger of the two estimates
/// keeps prose-heavy and symbol-heavy text both from slipping under a budget,
/// which is the failure that makes a cap useless: an answer reported as fitting
/// and then blowing the caller's context anyway.
pub fn estimate_tokens(text: &str) -> usize {
    let words = text.split_whitespace().count() * 13 / 10;
    let chars = text.chars().count() / 4;
    words.max(chars)
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

/// Render results, stopping once `budget` tokens are spent.
///
/// A caller with a context window cares about tokens, not result count: twenty
/// one-line matches and twenty matches inside deeply nested code differ by an
/// order of magnitude. Truncating on a real measurement, and saying so, beats
/// returning whatever a fixed limit happens to produce.
pub fn render_matches_within(matches: &[Match], header: &str, budget: usize) -> String {
    let (mut out, shown) = fit_within(matches, budget, |shown| render_matches(shown, header));
    if shown < matches.len() {
        out.push_str(&format!(
            "\n[{} more match(es) omitted to stay within {budget} tokens; \
             raise --max-tokens or narrow the search]\n",
            matches.len() - shown
        ));
    }
    out
}

/// The longest prefix of `matches` whose rendering fits `budget`, and how many
/// that is. Always at least one, so a budget below a single match still
/// answers rather than returning nothing.
///
/// Grows the output result by result, so the cut lands on a whole match
/// rather than mid-snippet. Quadratic in the number of matches, which is
/// bounded by --limit and measured in tens.
fn fit_within(
    matches: &[Match],
    budget: usize,
    render: impl Fn(&[Match]) -> String,
) -> (String, usize) {
    let mut shown = 0usize;
    let mut out = String::new();
    for take in 1..=matches.len() {
        let candidate = render(&matches[..take]);
        if estimate_tokens(&candidate) > budget && take > 1 {
            break;
        }
        out = candidate;
        shown = take;
    }
    (out, shown)
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
        // One line, not two: `Repo:` and `File:` on separate lines cost a
        // fifth of the output on a five-result page, and the reader needs the
        // pair together anyway to fetch the file. Separated by a space rather
        // than a slash, because `owner/name/a/b.rs` gives a reader no way to
        // tell where the repository ends and the path begins.
        let date = if stale {
            items
                .first()
                .map(|i| {
                    format!(
                        "  (last commit {})",
                        &i.pushed_at[..10.min(i.pushed_at.len())]
                    )
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        out.push_str(&format!("\n{} {}{date}\n", key.0, key.1));
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
            // Trim blank lines from the edges of the window. They cost a
            // gutter each and say nothing; a blank line between two matched
            // lines is kept, since it is part of the shape of the code.
            let first = item.context.iter().position(|l| !l.trim().is_empty());
            let last = item.context.iter().rposition(|l| !l.trim().is_empty());
            let (Some(first), Some(last)) = (first, last) else {
                continue;
            };
            let width = (start + last).to_string().len();
            for (offset, line) in item.context[first..=last].iter().enumerate() {
                let number = start + first + offset;
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
             `steroids add <repos>` once they agree."
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
             \x20 3. With their go-ahead: steroids add <repos> --tag <label>\n\
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
        Diagnosis::CrossLine => "No matches, and this pattern cannot match: it requires a \
             newline, but matching runs one line at a time. Search for the single line you \
             want, e.g. 'try:' instead of 'try:\\n'."
            .to_string(),
        Diagnosis::FilterExcludesAll { advice } => advice.clone(),
    }
}

#[cfg(test)]
mod budget_tests {
    use crate::search::Match;

    fn sample(n: usize) -> Vec<Match> {
        (0..n)
            .map(|i| Match {
                repo: format!("org/repo{i}"),
                path: format!("src/file{i}.rs"),
                line_number: 10,
                context: (0..11)
                    .map(|l| format!("    let value_{l} = compute_something({l});"))
                    .collect(),
                scope: "fn handler()".into(),
                commit_sha: String::new(),
                context_first_line: 5,
                pushed_at: String::new(),
            })
            .collect()
    }

    /// A caller with a context window cares about tokens, not result count.
    /// The budget must actually hold, and the caller must be told what was cut
    /// rather than silently receiving less.
    #[test]
    fn budget_is_respected_and_reported() {
        let matches = sample(30);
        for budget in [200usize, 800, 2000] {
            let out = super::render_matches_within(&matches, "30 match(es)", budget);
            let used = super::estimate_tokens(&out);
            assert!(
                used <= budget + super::estimate_tokens("[30 more match(es) omitted]") + 40,
                "budget {budget} produced {used} tokens"
            );
            assert!(
                out.contains("omitted"),
                "budget {budget} cut without saying so"
            );
        }
    }

    /// JSON is the agent's path, so the budget must hold there too, and the
    /// cut must be reported in a field rather than by a shorter list.
    #[test]
    fn json_budget_is_respected_and_reported() {
        let results = crate::search::SearchResults {
            matches: sample(30),
            more_available: false,
        };
        for budget in [200usize, 800, 2000] {
            let out = super::render_matches_json(&results, "x", &[], budget);
            let used = super::estimate_tokens(&out);
            assert!(
                used <= budget + 40,
                "budget {budget} produced {used} tokens"
            );
            let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
            let count = parsed["count"].as_u64().unwrap();
            let omitted = parsed["omitted"].as_u64().unwrap();
            assert_eq!(count + omitted, 30, "budget {budget} lost matches");
            assert!(omitted > 0, "budget {budget} cut without saying so");
        }
        let out = super::render_matches_json(&results, "x", &[], 100_000);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["omitted"], 0);
        assert_eq!(parsed["count"], 30);
    }

    /// A budget large enough for everything must not truncate or add a notice.
    #[test]
    fn a_generous_budget_shows_everything() {
        let matches = sample(5);
        let out = super::render_matches_within(&matches, "5 match(es)", 100_000);
        assert!(!out.contains("omitted"));
        for i in 0..5 {
            assert!(out.contains(&format!("org/repo{i}")), "lost result {i}");
        }
    }

    /// Code is denser than prose, and a word-based estimate undercounts it by
    /// several times, which silently blows every budget built on it.
    #[test]
    fn estimate_does_not_undercount_dense_code() {
        let dense = "a=b(c,d);e=f[g]+h.i(j,k);".repeat(40);
        let estimated = super::estimate_tokens(&dense);
        assert!(
            estimated >= dense.len() / 5,
            "estimated {estimated} tokens for {} characters of dense code",
            dense.len()
        );
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
        let results = crate::search::SearchResults {
            matches: vec![item],
            more_available: false,
        };
        let json = super::render_matches_json(&results, "needle", &[], 100_000);
        assert!(
            json.len() < 4_000,
            "one JSON match produced {} bytes of output",
            json.len()
        );
    }
}
