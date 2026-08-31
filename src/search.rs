//! Query the corpus: narrow with trigrams, confirm with a real regex.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use regex::bytes::RegexBuilder;
use rusqlite::params;

use crate::index::{decode, trigrams};
use crate::store::Store;

const METACHARACTERS: &[char] = &[
    '.', '*', '+', '?', '[', ']', '{', '}', '(', ')', '|', '^', '$', '\\',
];
/// A pathological pattern can hang the matcher, so cap how much we feed it.
const MAX_PATTERN_LENGTH: usize = 512;
const MAX_CANDIDATES: usize = 20_000;
/// Files read to confirm a literal really occurs, when diagnosing empty results.
const FRAGMENT_CONFIRM_LIMIT: usize = 40;
/// Lines of context shown either side of a match.
pub const DEFAULT_CONTEXT_LINES: usize = 3;

/// A hit inside a comment or docstring is prose about code, not an
/// implementation to learn from. Cheap prefix check, no parser.
///
/// Bare quotes are deliberately absent: a line starting with one is usually a
/// dict entry or continued literal (`"model": Model(...)`), which is exactly
/// the code an agent wants. Bare `*` is absent too, so C pointer lines like
/// `*ptr = value` survive; block comments are handled by BLOCK_DELIMITERS.
const COMMENT_PREFIXES: &[&str] = &["#", "//", "/*", "--", "\"\"\"", "'''"];
/// (opening, closing) delimiters for multi-line documentation blocks.
const BLOCK_DELIMITERS: &[(&str, &str)] = &[("\"\"\"", "\"\"\""), ("'''", "'''"), ("/*", "*/")];

pub struct Match {
    pub repo: String,
    pub path: String,
    pub line_number: usize,
    pub context: Vec<String>,
    /// The enclosing def/class, so the agent can judge relevance without
    /// spending a `steroids show` call on the whole file.
    pub scope: String,
}

#[derive(Default)]
pub struct Query<'a> {
    pub repo: Option<&'a str>,
    pub language: Option<&'a str>,
    pub path_glob: Option<&'a str>,
    pub ignore_case: bool,
    pub skip_comments: bool,
    pub context_lines: usize,
    pub limit: usize,
}

impl<'a> Query<'a> {
    pub fn new(limit: usize) -> Self {
        Self {
            skip_comments: true,
            context_lines: DEFAULT_CONTEXT_LINES,
            limit,
            ..Default::default()
        }
    }
}

fn is_prose(line: &str) -> bool {
    let trimmed = line.trim_start();
    COMMENT_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// 1-based line numbers inside docstrings or block comments.
///
/// Deliberately a scanner, not a parser: it only has to be right often enough
/// to keep documentation out of results that should show implementations.
fn prose_lines(lines: &[&str]) -> HashSet<usize> {
    let mut inside: Option<&str> = None;
    let mut marked = HashSet::new();
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        let trimmed = line.trim();
        match inside {
            None => {
                for (opener, closer) in BLOCK_DELIMITERS {
                    let Some(rest) = trimmed.split_once(opener).map(|(_, rest)| rest) else {
                        continue;
                    };
                    marked.insert(number);
                    // A one-line docstring opens and closes on the same line.
                    if !rest.contains(closer) {
                        inside = Some(closer);
                    }
                    break;
                }
            }
            Some(closer) => {
                marked.insert(number);
                if trimmed.contains(closer) {
                    inside = None;
                }
            }
        }
    }
    marked
}

/// Nearest preceding definition line above `number` (1-based).
fn enclosing_scope(lines: &[&str], number: usize) -> String {
    const KEYWORDS: &[&str] = &[
        "def ",
        "class ",
        "func ",
        "fn ",
        "type ",
        "struct ",
        "interface ",
        "impl ",
        "public ",
        "private ",
        "protected ",
        "static ",
        "export ",
        "async ",
    ];
    for line in lines[..number.min(lines.len())].iter().rev() {
        let trimmed = line.trim_start();
        if KEYWORDS.iter().any(|word| trimmed.starts_with(word)) && !is_prose(line) {
            let text = line.trim();
            return text.chars().take(120).collect();
        }
    }
    String::new()
}

/// Index just past the group or character class opening at `start`.
fn skip_group(pattern: &[char], start: usize) -> usize {
    let opener = pattern[start];
    let closer = if opener == '(' { ')' } else { ']' };
    let mut depth = 0usize;
    let mut index = start;
    while index < pattern.len() {
        let char = pattern[index];
        if char == '\\' {
            index += 2;
            continue;
        }
        if char == opener {
            depth += 1;
        } else if char == closer {
            depth -= 1;
            if depth == 0 {
                return index + 1;
            }
        }
        index += 1;
    }
    pattern.len()
}

/// Literal runs in a regex that every match must contain verbatim.
///
/// Group and character-class contents are skipped entirely: text inside them
/// may be optional or one of several alternatives, so it is not required.
pub fn literals(pattern: &str) -> Vec<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut runs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        let char = chars[index];
        if char == '\\' {
            index += 2;
            runs.push(std::mem::take(&mut current));
            continue;
        }
        if char == '(' || char == '[' {
            index = skip_group(&chars, index);
            runs.push(std::mem::take(&mut current));
            continue;
        }
        if METACHARACTERS.contains(&char) {
            // A quantifier makes the preceding character optional, so that
            // character cannot belong to a required literal.
            if (char == '*' || char == '?') && !current.is_empty() {
                current.pop();
            }
            runs.push(std::mem::take(&mut current));
        } else {
            current.push(char);
        }
        index += 1;
    }
    runs.push(current);
    runs.into_iter()
        .filter(|run| run.chars().count() >= 3)
        .collect()
}

/// Split a regex on top-level `|`.
///
/// Branches are independent: a match satisfies only one of them, so their
/// required literals must never be intersected with each other.
pub fn alternatives(pattern: &str) -> Vec<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut branches = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut index = 0usize;
    while index < chars.len() {
        let char = chars[index];
        if char == '\\' {
            current.push(char);
            if index + 1 < chars.len() {
                current.push(chars[index + 1]);
            }
            index += 2;
            continue;
        }
        match char {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            '|' if depth == 0 => {
                branches.push(std::mem::take(&mut current));
                index += 1;
                continue;
            }
            _ => {}
        }
        current.push(char);
        index += 1;
    }
    branches.push(current);
    branches
}

fn posting(store: &Store, gram: &[u8; 3]) -> Result<Option<Vec<i64>>> {
    let blob: Option<Vec<u8>> = store
        .db
        .query_row(
            "SELECT doc_ids FROM postings WHERE trigram = ?1",
            params![gram.as_slice()],
            |row| row.get(0),
        )
        .ok();
    blob.map(|bytes| decode(&bytes)).transpose()
}

/// Documents that could match one branch, or None if it cannot narrow.
fn branch_candidates(store: &Store, branch: &str) -> Result<Option<HashSet<i64>>> {
    let runs = literals(branch);
    if runs.is_empty() {
        return Ok(None);
    }
    let mut best: Option<HashSet<i64>> = None;
    for run in runs {
        let mut grams: Vec<[u8; 3]> = trigrams(run.as_bytes()).into_iter().collect();
        grams.sort_unstable();
        for gram in grams {
            // Absent, or too common to be stored; neither narrows anything.
            let Some(ids) = posting(store, &gram)? else {
                continue;
            };
            let ids: HashSet<i64> = ids.into_iter().collect();
            best = Some(match best {
                None => ids,
                Some(previous) => previous.intersection(&ids).copied().collect(),
            });
            if best.as_ref().is_some_and(|set| set.is_empty()) {
                return Ok(Some(HashSet::new()));
            }
        }
    }
    Ok(best)
}

/// Documents that could match, or None if the index cannot narrow.
fn candidates(store: &Store, pattern: &str) -> Result<Option<HashSet<i64>>> {
    let mut combined = HashSet::new();
    for branch in alternatives(pattern) {
        match branch_candidates(store, &branch)? {
            // This branch could match anything, so narrowing would lose results.
            None => return Ok(None),
            Some(found) => combined.extend(found),
        }
    }
    Ok(Some(combined))
}

/// Whether a literal string really occurs in the corpus.
///
/// Stricter than candidate selection: every trigram must be known, and a
/// surviving candidate must actually contain the string. Trigrams are
/// non-positional, so a file can hold every trigram of "solidity_contract_g"
/// without containing it.
fn fragment_present(store: &mut Store, run: &str) -> Result<bool> {
    let mut grams: Vec<[u8; 3]> = trigrams(run.as_bytes()).into_iter().collect();
    if grams.is_empty() {
        return Ok(true);
    }
    grams.sort_unstable();

    let mut narrowed: Option<HashSet<i64>> = None;
    for gram in grams {
        match posting(store, &gram)? {
            Some(ids) => {
                let ids: HashSet<i64> = ids.into_iter().collect();
                narrowed = Some(match narrowed {
                    None => ids,
                    Some(previous) => previous.intersection(&ids).copied().collect(),
                });
                if narrowed.as_ref().is_some_and(|set| set.is_empty()) {
                    return Ok(false);
                }
            }
            None => {
                // Absent, or dropped for being too common. Only the former is
                // decisive, so a stop-trigram counts as no evidence either way.
                let known_common = store
                    .stop_trigrams()
                    .is_none_or(|stop| stop.contains(&gram));
                if !known_common {
                    return Ok(false);
                }
            }
        }
    }

    let Some(narrowed) = narrowed else {
        return Ok(true);
    };
    let mut ids: Vec<i64> = narrowed.into_iter().collect();
    ids.sort_unstable();
    for doc_id in ids.into_iter().take(FRAGMENT_CONFIRM_LIMIT) {
        let content = store.read_document(doc_id)?;
        if memchr::memmem::find(&content, run.as_bytes()).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub enum Diagnosis {
    EmptyCorpus,
    /// The corpus covers the topic but not this exact spelling.
    NearMiss {
        missing: String,
        nearest: String,
    },
    /// No indexed project mentions this at all: more repositories are needed.
    TopicAbsent {
        missing: String,
    },
    /// Some fragments exist, so a looser pattern may work.
    SpellingMismatch {
        known: String,
    },
    /// Nothing literal to search on.
    TooBroad,
}

pub struct Facts {
    pub diagnosis: Diagnosis,
    pub repos: usize,
    pub languages: Vec<String>,
}

/// Explain why a search found nothing.
///
/// The agent should act differently per cause: index repositories (empty
/// corpus), ask the user for relevant repositories (topic absent), retry a
/// corrected spelling (near miss), or supply a real literal (too broad).
pub fn diagnose(store: &mut Store, pattern: &str) -> Result<Facts> {
    let repos: usize = store
        .db
        .query_row("SELECT COUNT(*) FROM repos", [], |row| row.get::<_, i64>(0))?
        as usize;
    if repos == 0 {
        return Ok(Facts {
            diagnosis: Diagnosis::EmptyCorpus,
            repos: 0,
            languages: Vec::new(),
        });
    }
    let languages: Vec<String> = store
        .db
        .prepare(
            "SELECT language, COUNT(*) c FROM documents WHERE offset >= 0 \
             GROUP BY language ORDER BY c DESC LIMIT 6",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;

    // Do the longest literals in the pattern appear anywhere at all? If even a
    // fragment is unknown, no rephrasing will help and the honest answer is
    // that more repositories are needed.
    let mut runs: Vec<String> = alternatives(pattern)
        .iter()
        .flat_map(|branch| literals(branch))
        .collect();
    runs.sort_by_key(|run| std::cmp::Reverse(run.len()));
    runs.dedup();

    let (mut known, mut unknown) = (Vec::new(), Vec::new());
    for run in runs.iter().take(3) {
        if fragment_present(store, run)? {
            known.push(run.clone());
        } else {
            unknown.push(run.clone());
        }
    }

    // A near-miss spelling ('max_retriez') is absent as written while the topic
    // is still covered, so look for the longest known prefix. Only a
    // substantial one counts: 'rust_' out of 'rust_borrow_checker' would send
    // the agent chasing unrelated matches.
    let mut nearest = String::new();
    if let Some(longest) = unknown.iter().max_by_key(|run| run.len()) {
        let floor = ((longest.len() as f64 * 0.6) as usize).max(6);
        let mut size = longest.len().saturating_sub(1);
        while size >= floor {
            let prefix: String = longest.chars().take(size).collect();
            if fragment_present(store, &prefix)? {
                nearest = prefix;
                break;
            }
            size -= 1;
        }
    }

    let diagnosis = if !runs.is_empty() && known.is_empty() {
        let missing = unknown.join(", ");
        if nearest.is_empty() {
            Diagnosis::TopicAbsent { missing }
        } else {
            Diagnosis::NearMiss { missing, nearest }
        }
    } else if !known.is_empty() {
        Diagnosis::SpellingMismatch {
            known: known.join(", "),
        }
    } else {
        Diagnosis::TooBroad
    };
    Ok(Facts {
        diagnosis,
        repos,
        languages,
    })
}

/// Shell-style glob, supporting `*`, `?` and `**`.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            pi += 1;
            mark = ti;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn search(store: &mut Store, pattern: &str, query: &Query) -> Result<Vec<Match>> {
    if pattern.len() > MAX_PATTERN_LENGTH {
        bail!("pattern too long");
    }
    let matcher = RegexBuilder::new(pattern)
        .case_insensitive(query.ignore_case)
        .build()
        .map_err(|error| anyhow!("invalid pattern: {error}"))?;

    // Trigram narrowing is case-sensitive, so it would discard documents that a
    // case-insensitive matcher should still find. Scan everything instead.
    let narrowed = if query.ignore_case {
        None
    } else {
        candidates(store, pattern)?
    };

    let mut sql = String::from(
        "SELECT d.id, r.name, d.path FROM documents d \
         JOIN repos r ON r.id = d.repo_id WHERE d.offset >= 0",
    );
    let mut binds: Vec<String> = Vec::new();
    if let Some(repo) = query.repo {
        sql.push_str(" AND r.name = ?");
        binds.push(repo.to_string());
    }
    if let Some(language) = query.language {
        sql.push_str(&format!(" AND d.language = ?{}", binds.len() + 1));
        binds.push(language.to_string());
    }

    let mut statement = store.db.prepare(&sql)?;
    let rows: Vec<(i64, String, String)> = statement
        .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<Result<_, _>>()?;
    drop(statement);

    let rows: Vec<_> = rows
        .into_iter()
        .filter(|(id, _, path)| {
            narrowed.as_ref().is_none_or(|set| set.contains(id))
                && query.path_glob.is_none_or(|glob| glob_matches(glob, path))
        })
        .take(MAX_CANDIDATES)
        .collect();

    // Documents come back grouped by repository, so taking the first N matches
    // would fill the whole budget from one project. The point of the corpus is
    // comparing how different projects solved the same thing, so collect per
    // repo and interleave.
    let per_repo_cap = (query.limit / 2).max(1);
    let mut by_repo: HashMap<String, Vec<Match>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut collected = 0usize;

    for (doc_id, repo, path) in rows {
        if by_repo.get(&repo).is_some_and(|v| v.len() >= per_repo_cap) {
            continue;
        }
        let content = store.read_document(doc_id)?;
        if !matcher.is_match(&content) {
            continue;
        }
        let text = String::from_utf8_lossy(&content).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        let prose = if query.skip_comments {
            prose_lines(&lines)
        } else {
            HashSet::new()
        };

        if !by_repo.contains_key(&repo) {
            order.push(repo.clone());
        }
        let bucket = by_repo.entry(repo.clone()).or_default();
        let mut last_end = 0usize;
        for (index, line) in lines.iter().enumerate() {
            let number = index + 1;
            if !matcher.is_match(line.as_bytes()) {
                continue;
            }
            if query.skip_comments && (is_prose(line) || prose.contains(&number)) {
                continue;
            }
            // Consecutive hits in one file would emit near-identical context
            // blocks; report the first and skip the rest of that window.
            if number <= last_end {
                continue;
            }
            let low = number.saturating_sub(1 + query.context_lines);
            let high = (number + query.context_lines).min(lines.len());
            last_end = high;
            bucket.push(Match {
                repo: repo.clone(),
                path: path.clone(),
                line_number: number,
                context: lines[low..high].iter().map(|s| s.to_string()).collect(),
                scope: enclosing_scope(&lines, number),
            });
            collected += 1;
            if bucket.len() >= per_repo_cap {
                break;
            }
        }
        if collected >= query.limit * 4 {
            break;
        }
    }

    let mut queues: Vec<std::vec::IntoIter<Match>> = order
        .into_iter()
        .filter_map(|repo| by_repo.remove(&repo).map(|v| v.into_iter()))
        .collect();
    let mut results = Vec::new();
    while !queues.is_empty() && results.len() < query.limit {
        queues.retain_mut(|queue| match queue.next() {
            Some(item) => {
                if results.len() < query.limit {
                    results.push(item);
                }
                true
            }
            None => false,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_ignore_group_contents() {
        assert_eq!(literals(r"(def|class)\s+RunContext\b"), vec!["RunContext"]);
        assert_eq!(literals(r"colou?r_value"), vec!["colo", "r_value"]);
        assert_eq!(literals(r"[abc]handler"), vec!["handler"]);
    }

    #[test]
    fn alternatives_split_only_at_top_level() {
        assert_eq!(alternatives(r"a(x|y)b|cde"), vec!["a(x|y)b", "cde"]);
        assert_eq!(alternatives(r"[a|b]c"), vec!["[a|b]c"]);
    }

    #[test]
    fn docstrings_and_comments_are_excluded() {
        let src = vec![
            "def f():",
            "    \"\"\"Doc one.",
            "    Doc two.",
            "    \"\"\"",
            "    x = 1",
        ];
        let prose = prose_lines(&src);
        assert!(prose.contains(&2) && prose.contains(&3) && prose.contains(&4));
        assert!(!prose.contains(&1) && !prose.contains(&5));

        let single = prose_lines(&["    \"\"\"one liner.\"\"\"", "    y = 2"]);
        assert!(!single.contains(&2), "one-line docstring leaked");
    }

    /// Dict entries and C pointer lines are code an agent wants to see.
    #[test]
    fn code_starting_with_quote_or_star_is_not_prose() {
        for code in [
            "\"model\": Model(max_retries=3),",
            "*ptr = value;",
            "'key': 1,",
        ] {
            assert!(!is_prose(code), "real code flagged as prose: {code}");
        }
        for prose in ["# a comment", "// a comment", "-- sql comment"] {
            assert!(is_prose(prose), "comment not detected: {prose}");
        }
    }

    #[test]
    fn globs_match_paths() {
        assert!(glob_matches("src/*.py", "src/agent.py"));
        assert!(glob_matches("*/agent.py", "src/agent.py"));
        assert!(!glob_matches("src/*.py", "src/agent.rs"));
    }
}
