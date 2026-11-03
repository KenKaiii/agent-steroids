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
                    "scope": item.scope,
                    "context": item.context.iter().map(|line| clamp(line)).collect::<Vec<_>>(),
                    "body_end": item.body_end,
                })
            })
            .collect();
        // Once per repository, not once per match: a 40-character hash is
        // twenty-odd tokens, and a permalink on every match was the largest
        // part of a typical answer.
        let mut commits = serde_json::Map::new();
        for item in shown {
            if let Some(sha) = item.commit() {
                commits.entry(item.repo.clone()).or_insert(sha.into());
            }
        }
        // Compact: indentation is tokens the reader pays for and never uses.
        serde_json::to_string(&serde_json::json!({
            "pattern": pattern,
            "count": items.len(),
            "commits": commits,
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

/// Paths as one line per directory: `lib/core/: Axios.js AxiosError.js`.
/// Each directory is named once instead of on every file, which is most of
/// a long listing's tokens.
pub fn grouped_paths<'a>(paths: impl Iterator<Item = &'a str>) -> String {
    let mut directories: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for path in paths {
        let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
        directories.entry(directory).or_default().push(name);
    }
    let mut out = String::new();
    for (directory, names) in directories {
        out.push_str(if directory.is_empty() { "." } else { directory });
        out.push_str("/:");
        for name in names {
            out.push(' ');
            // A name with a space would read as two files.
            if name.contains(char::is_whitespace) {
                out.push_str(&format!("{name:?}"));
            } else {
                out.push_str(name);
            }
        }
        out.push('\n');
    }
    out
}

/// Each directory once, with how many indexed files it holds:
/// `lib/core/ (13)`. The map for a repository too big to list.
pub fn directory_counts<'a>(paths: impl Iterator<Item = &'a str>) -> String {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for path in paths {
        let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
        *counts.entry(directory).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(directory, count)| {
            format!(
                "{}/ ({count})\n",
                if directory.is_empty() { "." } else { directory }
            )
        })
        .collect()
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
///
/// `header` is given the number of matches actually shown, so the first line
/// can say "3 of 20" rather than a count the reader then has to distrust.
pub fn render_matches_within(
    matches: &[Match],
    header: impl Fn(usize) -> String,
    budget: usize,
    compact: bool,
) -> String {
    let (mut out, shown) = fit_within(matches, budget, |shown| {
        if compact {
            render_compact(shown, &header(shown.len()))
        } else {
            render_matches(shown, &header(shown.len()))
        }
    });
    if shown < matches.len() {
        // Named, so the reader can go straight to the ones it needs instead
        // of re-running the search with a bigger budget.
        let rest = &matches[shown..];
        let mut places: Vec<String> = rest
            .iter()
            .take(OMITTED_NAMED)
            .map(|item| format!("{}:{}", item.path, item.line_number))
            .collect();
        if rest.len() > OMITTED_NAMED {
            places.push(format!("+{}", rest.len() - OMITTED_NAMED));
        }
        out.push_str(&format!(
            "\n[{} more match(es) past the {budget}-token budget: {}. Narrow the search \
             (repo, path or pattern) or read one of these by line]\n",
            rest.len(),
            places.join(" ")
        ));
    }
    out
}

/// How many omitted matches are named individually.
const OMITTED_NAMED: usize = 12;

/// One line per match, grouped by file with a count: where every hit is,
/// without the code around it.
pub fn render_compact(matches: &[Match], header: &str) -> String {
    let mut out = format!(
        "{header}; one line each. Ask with context, or narrow the search, to see code around them\n"
    );
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
        let mut items = grouped[&key].clone();
        items.sort_by_key(|item| item.line_number);
        out.push_str(&format!("\n{} {} ({})\n", key.0, key.1, items.len()));
        for item in items {
            out.push_str(&format!(
                "{}\t{}\n",
                item.line_number,
                clamp(item.matched_line().trim())
            ));
        }
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
        // Ranking chose which hits made the page; within one file, reading
        // them top to bottom is what makes the file's shape legible.
        let mut items = grouped[&key].clone();
        items.sort_by_key(|item| item.line_number);
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
        let mut previous_scope: Option<&str> = None;
        let mut previous_window = 0usize;
        for item in items {
            // Trim blank lines from the edges of the window. They cost a
            // gutter each and say nothing; a blank line between two matched
            // lines is kept, since it is part of the shape of the code.
            let first = item.context.iter().position(|l| !l.trim().is_empty());
            let last = item.context.iter().rposition(|l| !l.trim().is_empty());
            let (Some(first), Some(last)) = (first, last) else {
                continue;
            };
            let window = last - first + 1;
            // Skip the scope line when the definition is already visible in
            // the context, or when the previous hit in this file named it.
            let start = item.context_start();
            let scope_shown = item
                .context
                .iter()
                .any(|line| line.trim() == item.scope.trim());
            let scope_line = !item.scope.is_empty()
                && !scope_shown
                && previous_scope != Some(item.scope.as_str());
            previous_scope = Some(item.scope.as_str());
            // Single-line hits (-C 0) read as one grep-like list; a blank line
            // between each doubled their cost and separated nothing.
            if previous_window != 1 || window != 1 || scope_line {
                out.push('\n');
            }
            previous_window = window;
            if scope_line {
                out.push_str(&format!("  in {}\n", item.scope));
            }
            // A gutter of real line numbers lets the agent cite an exact
            // location and lets a person scroll straight to it. A tab rather
            // than padding and a box character: same alignment, fewer tokens.
            for (offset, line) in item.context[first..=last].iter().enumerate() {
                let number = start + first + offset;
                let marker = if number == item.line_number { ">" } else { " " };
                out.push_str(&format!("{marker}{number}\t{}\n", clamp(line)));
            }
            if let Some(end) = item.body_end {
                out.push_str(&format!(
                    "  … continues to line {end}; ask {} {} for lines {}-{end}\n",
                    item.repo,
                    item.path,
                    start + item.context.len()
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
                body_end: None,
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
            let out =
                super::render_matches_within(&matches, |_| "30 match(es)".into(), budget, false);
            let used = super::estimate_tokens(&out);
            assert!(
                // The cut-off note names up to a dozen places on top.
                used <= budget + 160,
                "budget {budget} produced {used} tokens"
            );
            assert!(
                out.contains("-token budget: src/file"),
                "budget {budget} cut without naming what it left out: {out}"
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
        let out = super::render_matches_within(&matches, |_| "5 match(es)".into(), 100_000, false);
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
            body_end: None,
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

#[cfg(test)]
mod grouped_tests {
    #[test]
    fn grouped_paths_name_each_directory_once() {
        let paths = [
            "lib/utils.js",
            "lib/core/Axios.js",
            "index.js",
            "lib/core/my file.js",
            "lib/axios.js",
        ];
        assert_eq!(
            super::grouped_paths(paths.into_iter()),
            "./: index.js\nlib/: utils.js axios.js\nlib/core/: Axios.js \"my file.js\"\n"
        );
        assert_eq!(
            super::directory_counts(paths.into_iter()),
            "./ (1)\nlib/ (2)\nlib/core/ (2)\n"
        );
    }
}
