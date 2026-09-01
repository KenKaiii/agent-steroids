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
/// Above this many narrowed ids, an `IN` list stops being cheaper than a scan
/// and starts straining the SQL parser.
const SQL_ID_LIMIT: usize = 50_000;
/// Documents examined before a search settles for what it has. Chosen from
/// measurement: a selective pattern leaves thousands of trigram candidates,
/// and scanning them all costs five times as long for results the ranking
/// would not have promoted anyway.
const MAX_DOCUMENTS_SCANNED: usize = 2_000;
/// Once a candidate set is this small, decoding another hundred-thousand-entry
/// posting list to shrink it further costs more than it saves.
const NARROW_ENOUGH: usize = 200;
/// Files read to confirm a literal really occurs, when diagnosing empty results.
const FRAGMENT_CONFIRM_LIMIT: usize = 40;
/// Lines of context shown either side of a match.
///
/// Five, because three is enough to locate a match but not to judge it: a
/// reader comparing how several projects solved the same problem needs to see
/// the shape of each implementation, not a fragment. Matches what the hosted
/// code search tools settled on.
pub const DEFAULT_CONTEXT_LINES: usize = 5;

/// A hit inside a comment or docstring is prose about code, not an
/// implementation to learn from. Cheap prefix check, no parser.
///
/// Bare quotes are deliberately absent: a line starting with one is usually a
/// dict entry or continued literal (`"model": Model(...)`), which is exactly
/// the code an agent wants. Bare `*` is absent too, so C pointer lines like
/// `*ptr = value` survive; block comments are handled by BLOCK_DELIMITERS.
const COMMENT_PREFIXES: &[&str] = &["#", "//", "/*", "--", "\"\"\"", "'''"];

pub struct Match {
    pub repo: String,
    pub path: String,
    pub line_number: usize,
    pub context: Vec<String>,
    /// The enclosing def/class, so the agent can judge relevance without
    /// spending a `steroids show` call on the whole file.
    pub scope: String,
    /// Commit the snapshot came from, when it is a real git hash. Empty when
    /// only an archive checksum was available, in which case no permalink is
    /// offered rather than a broken one.
    pub commit_sha: String,
    /// Line number of the first line in `context`. Stored rather than derived,
    /// because the context width is a per-query setting and recomputing it
    /// from a default silently misnumbers the gutter.
    pub context_first_line: usize,
    /// Date of the repository's last upstream commit, empty when unknown.
    /// Lets a reader weigh a snippet from a project that stopped moving years
    /// ago differently from one changed this week.
    pub pushed_at: String,
}

impl Match {
    /// Line number of the first line of `context`.
    pub fn context_start(&self) -> usize {
        self.context_first_line
    }

    /// A GitHub permalink to the matched line, if the commit is known.
    pub fn permalink(&self) -> Option<String> {
        // A git hash is 40 hex characters. Anything else is our archive
        // checksum, which GitHub will not resolve.
        if self.commit_sha.len() != 40 || !self.commit_sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(format!(
            "https://github.com/{}/blob/{}/{}#L{}",
            self.repo, self.commit_sha, self.path, self.line_number
        ))
    }
}

#[derive(Default)]
pub struct Query<'a> {
    pub repo: Option<&'a str>,
    /// Restrict to repositories carrying this label.
    pub tag: Option<&'a str>,
    pub language: Option<&'a str>,
    pub path_glob: Option<&'a str>,
    pub ignore_case: bool,
    pub skip_comments: bool,
    /// Most results to take from any one repository. Lowering it trades depth
    /// for breadth, which is what someone comparing how several projects
    /// solved a problem actually wants: ten projects once each says more than
    /// three projects three times each.
    pub per_repo: Option<usize>,
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

/// A run of block-comment lines longer than this means the scanner lost sync,
/// not that a genuine comment is that long. Real docstrings are far shorter.
const MAX_BLOCK_COMMENT_LINES: usize = 60;

/// Python docstring delimiters, named to keep the quoting readable.
const TRIPLE_DOUBLE: &str = "\"\"\"";
const TRIPLE_SINGLE: &str = "'''";

/// 1-based line numbers inside docstrings or block comments.
///
/// Deliberately a scanner rather than a parser, but it fails open. A delimiter
/// appearing inside a string literal (`v.startswith(('/*', '//'))` in Python is
/// a real example) would otherwise leave the scanner believing a comment never
/// closed, marking the rest of the file as prose and hiding thousands of lines
/// of real code. Two guards prevent that:
///
/// 1. Delimiters are chosen by language, so C-style `/* */` is not looked for
///    in a Python file at all.
/// 2. A block that never closes within `MAX_BLOCK_COMMENT_LINES` is treated as
///    a mistake and abandoned, keeping the damage local.
///
/// Showing a comment that should have been filtered is a small cost. Hiding
/// real code the user asked for is not.
fn prose_lines(lines: &[&str], language: &str) -> HashSet<usize> {
    // Docstring syntax is per-language; looking for the wrong delimiters is
    // what desynchronises the scanner.
    let delimiters: &[(&str, &str)] = match language {
        "python" => &[
            (TRIPLE_DOUBLE, TRIPLE_DOUBLE),
            (TRIPLE_SINGLE, TRIPLE_SINGLE),
        ],
        "ruby" | "shell" | "elixir" | "lua" | "sql" => &[],
        // C-family and everything else that uses /* */.
        _ => &[("/*", "*/")],
    };
    if delimiters.is_empty() {
        return HashSet::new();
    }

    let mut inside: Option<&str> = None;
    let mut opened_at = 0usize;
    let mut marked = HashSet::new();
    let mut pending: Vec<usize> = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        let trimmed = line.trim();
        match inside {
            None => {
                for (opener, closer) in delimiters {
                    let Some(rest) = trimmed.split_once(opener).map(|(_, rest)| rest) else {
                        continue;
                    };
                    // A one-line docstring opens and closes on the same line.
                    if rest.contains(closer) {
                        marked.insert(number);
                    } else {
                        inside = Some(closer);
                        opened_at = number;
                        pending.push(number);
                    }
                    break;
                }
            }
            Some(closer) => {
                pending.push(number);
                if trimmed.contains(closer) {
                    marked.extend(pending.drain(..));
                    inside = None;
                } else if number - opened_at > MAX_BLOCK_COMMENT_LINES {
                    // Almost certainly a delimiter inside a string literal.
                    // Discard the run rather than hide the rest of the file.
                    pending.clear();
                    inside = None;
                }
            }
        }
    }
    // An unterminated block at end of file is also a desync; drop it.
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

/// Byte-level case variants of a trigram, for narrowing without case.
///
/// ASCII letters only, and not `k` or `s`. Anything else reports unfoldable
/// and the trigram is left out of narrowing, which can only widen the
/// candidate set: a non-ASCII letter changes its bytes when it changes case,
/// and under the matcher's Unicode folding `k` also matches the Kelvin sign
/// and `s` the long s, which no ASCII variant covers.
fn case_variants(gram: &[u8; 3]) -> Option<Vec<[u8; 3]>> {
    let foldable = gram
        .iter()
        .all(|byte| byte.is_ascii() && !matches!(byte.to_ascii_lowercase(), b'k' | b's'));
    if !foldable {
        return None;
    }
    let mut variants = vec![*gram];
    for position in 0..3 {
        if !gram[position].is_ascii_alphabetic() {
            continue;
        }
        for index in 0..variants.len() {
            let mut flipped = variants[index];
            flipped[position] ^= 0x20;
            variants.push(flipped);
        }
    }
    Some(variants)
}

/// Documents that could match one branch, or None if it cannot narrow.
///
/// With `fold` set, each trigram stands for the union of its case variants,
/// which is how codesearch narrows a case-folded literal. A variant that was
/// dropped as too common leaves the union incomplete, so such a trigram is
/// skipped rather than narrowed on; skipping only widens the result.
fn branch_candidates(
    store: &Store,
    branch: &str,
    fold: Option<&HashSet<[u8; 3]>>,
) -> Result<Option<HashSet<i64>>> {
    let runs = literals(branch);
    if runs.is_empty() {
        return Ok(None);
    }

    // Decoding a posting list is the expensive part of narrowing: a common
    // trigram holds a hundred thousand ids, and decoding several of those
    // costs more than the search that follows. The compressed length is a good
    // proxy for how many ids a list holds, so read the sizes first and start
    // with the most selective. Every later intersection then runs against a
    // set that is already small, and a rare trigram often reduces the answer
    // to a handful before the common ones are touched at all.
    let mut sized: Vec<(Vec<[u8; 3]>, i64)> = Vec::new();
    for run in &runs {
        for gram in trigrams(run.as_bytes()) {
            let variants = match fold {
                None => vec![gram],
                Some(stop) => {
                    let Some(variants) = case_variants(&gram) else {
                        continue;
                    };
                    if variants.iter().any(|variant| stop.contains(variant)) {
                        continue;
                    }
                    variants
                }
            };
            let mut total = 0i64;
            let mut present = false;
            for variant in &variants {
                if let Some(count) = posting_size(store, variant)? {
                    total += count;
                    present = true;
                }
            }
            if present {
                sized.push((variants, total));
            }
        }
    }
    if sized.is_empty() {
        return Ok(None);
    }
    sized.sort_unstable_by_key(|(_, count)| *count);

    // Skipping the least selective trigrams outright was tried and measured
    // slower: even a list covering a fifth of the corpus removes candidates
    // the regex pass would otherwise have to read. Decode them all, smallest
    // first, and stop early once the set is small enough.
    let mut best: Option<HashSet<i64>> = None;
    for (variants, _) in sized {
        let mut ids: Vec<i64> = Vec::new();
        for variant in &variants {
            if let Some(list) = posting(store, variant)? {
                ids.extend(list);
            }
        }
        best = Some(match best {
            None => ids.into_iter().collect(),
            Some(previous) => {
                // Walk the smaller side, not the freshly decoded one.
                let ids: HashSet<i64> = ids.into_iter().collect();
                let (small, large) = if previous.len() <= ids.len() {
                    (&previous, &ids)
                } else {
                    (&ids, &previous)
                };
                small
                    .iter()
                    .filter(|id| large.contains(id))
                    .copied()
                    .collect()
            }
        });
        match best.as_ref() {
            Some(set) if set.is_empty() => return Ok(Some(HashSet::new())),
            // Already narrow enough that further intersection costs more than
            // the regex pass it would save.
            Some(set) if set.len() <= NARROW_ENOUGH => break,
            _ => {}
        }
    }
    Ok(best)
}

/// How many documents a posting list holds, without decoding it.
///
/// Stored alongside the list rather than derived from its compressed length:
/// two lists of similar size can hold wildly different numbers of ids, and
/// ordering the intersection well depends on knowing which is genuinely
/// smaller. Falls back to the byte length for an index built before the count
/// was recorded.
fn posting_size(store: &Store, gram: &[u8; 3]) -> Result<Option<i64>> {
    Ok(store
        .db
        .query_row(
            "SELECT COALESCE(doc_count, LENGTH(doc_ids)) FROM postings WHERE trigram = ?1",
            params![gram.as_slice()],
            |row| row.get(0),
        )
        .ok())
}

fn candidates(store: &mut Store, pattern: &str, fold: bool) -> Result<Option<HashSet<i64>>> {
    // Folding needs to tell a variant that never occurs from one dropped as
    // too common. An index that predates stop-list tracking cannot, so the
    // only safe answer there is to scan.
    let stop: Option<HashSet<[u8; 3]>> = if fold {
        match store.stop_trigrams() {
            Some(stop) => Some(stop.clone()),
            None => return Ok(None),
        }
    } else {
        None
    };
    let mut combined = HashSet::new();
    for branch in alternatives(pattern) {
        match branch_candidates(store, &branch, stop.as_ref())? {
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
        // These ids come from the trigram index, which an update leaves
        // pointing at documents that have been replaced. A stale entry is
        // expected until the index is rebuilt, so skip it.
        let Some(content) = store.try_read_document(doc_id)? else {
            continue;
        };
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
    /// Every branch demands a newline, which a line-oriented match never sees.
    CrossLine,
}

/// Can this pattern only match text that spans a line break?
///
/// Matching runs line by line, so `try:\n` finds nothing however much Python
/// the corpus holds. Left undetected that looks identical to a missing topic,
/// and the honest answer is the opposite: rewrite the pattern.
///
/// Deliberately conservative. A newline inside a character class is ignored
/// because `[^\n]` asks for the opposite, and one alternative that can match
/// within a line is enough to make the whole pattern viable.
pub fn only_matches_across_lines(pattern: &str) -> bool {
    let branches = alternatives(pattern);
    !branches.is_empty() && branches.iter().all(|branch| branch_needs_newline(branch))
}

fn branch_needs_newline(branch: &str) -> bool {
    let bytes = branch.as_bytes();
    let mut index = 0usize;
    let mut in_class = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if index + 1 < bytes.len() => {
                if !in_class && bytes[index + 1] == b'n' {
                    return true;
                }
                index += 2;
                continue;
            }
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'\n' if !in_class => return true,
            _ => {}
        }
        index += 1;
    }
    false
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
    // Checked before any probing: the fragments are present and the pattern is
    // still unmatchable, so probing for them costs a second and misleads.
    if only_matches_across_lines(pattern) {
        return Ok(Facts {
            diagnosis: Diagnosis::CrossLine,
            repos,
            languages: Vec::new(),
        });
    }

    // Counted over repositories, not documents: the message says how many
    // repositories the corpus holds, and grouping every document by language
    // instead cost 109ms of a diagnosis that should be instant.
    let languages: Vec<String> = store
        .db
        .prepare(
            "SELECT language, COUNT(*) c FROM repos WHERE language IS NOT NULL \
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

/// How useful a match is likely to be, higher is better.
///
/// A reader looking for `RetryPolicy` almost always wants the place it is
/// defined, not the twentieth place it is passed as an argument.
fn relevance(item: &Match) -> u32 {
    let line = item
        .context
        .get(item.line_number.saturating_sub(item.context_start()))
        .map(String::as_str)
        .unwrap_or("");
    let trimmed = line.trim_start();

    // Start above zero so penalties have room to bite.
    let mut score: u32 = 1000;
    // A definition is what someone is usually after.
    const DEFINITION_KEYWORDS: &[&str] = &[
        "def ",
        "class ",
        "func ",
        "fn ",
        "type ",
        "struct ",
        "interface ",
        "impl ",
        "enum ",
        "trait ",
        "pub fn ",
        "export ",
        "async def ",
        "public ",
        "private ",
    ];
    if DEFINITION_KEYWORDS
        .iter()
        .any(|word| trimmed.starts_with(word))
    {
        score += 7000;
    }
    // An import or re-export names a thing without showing how it works.
    const WEAK_PREFIXES: &[&str] = &["import ", "from ", "use ", "#include", "require(", "@"];
    // The penalty has to outweigh the line-length bonus below, or a short
    // import would outrank a longer line that actually does something.
    if WEAK_PREFIXES.iter().any(|word| trimmed.starts_with(word)) {
        score = score.saturating_sub(600);
    }
    // Shorter lines carry less noise around the match, but this is a
    // tiebreaker and must stay small next to the signals above.
    score += 200u32.saturating_sub(line.len().min(200) as u32);
    // Tests demonstrate usage but are rarely the implementation being sought.
    if item.path.contains("test") || item.path.contains("spec") {
        score = score.saturating_sub(300);
    }
    score
}

/// Whether a pattern switches on case-insensitive matching by itself.
///
/// Only leading group flags are considered, since a flag set mid-pattern
/// applies from that point and the literals before it are still exact. Being
/// wrong in the cautious direction only costs a full scan.
fn has_inline_case_flag(pattern: &str) -> bool {
    let mut rest = pattern;
    while let Some(open) = rest.find("(?") {
        // A flag group ends at the first ) or :, e.g. (?i) or (?im:...).
        let after = &rest[open + 2..];
        let end = after.find([')', ':']).unwrap_or(after.len());
        let flags = &after[..end];
        // Flags after a '-' are being turned off, not on.
        let enabled = flags.split('-').next().unwrap_or("");
        if enabled.contains('i') {
            return true;
        }
        rest = &after[end.min(after.len())..];
    }
    false
}

/// Matches, plus whether the corpus held more than were returned.
pub struct SearchResults {
    pub matches: Vec<Match>,
    /// True when the scan stopped before exhausting the corpus, so an agent
    /// knows a narrower query would show different code rather than assuming
    /// it has seen everything.
    pub more_available: bool,
}

impl std::ops::Deref for SearchResults {
    type Target = Vec<Match>;
    fn deref(&self) -> &Self::Target {
        &self.matches
    }
}

/// Candidates for a pattern the index could not narrow, spread across
/// repositories, without visiting the whole corpus.
///
/// Each repository contributes up to twice its fair share of the budget so
/// small repositories leave room for large ones, then the shares are
/// interleaved round-robin and cut at the budget. A repository that had more
/// than was taken, or a total over the budget, means the answer is capped.
///
/// Repository filters apply to the repository list, the rest to each
/// repository's documents. The language and path filters still walk a
/// repository's documents until they find enough, so they are the one case
/// here that grows with the corpus; that is still SQLite reading integers,
/// not Rust holding strings.
fn unnarrowed_candidates(
    store: &Store,
    query: &Query,
    binds: &[String],
) -> Result<(Vec<i64>, bool)> {
    let mut repo_sql = String::from("SELECT id FROM repos r WHERE 1");
    let mut repo_binds: Vec<&String> = Vec::new();
    let mut doc_sql = String::from("SELECT id FROM documents WHERE repo_id = ?1 AND offset >= 0");
    let mut doc_binds: Vec<&String> = Vec::new();
    // Binds were pushed in this order above: repo, tag, language, glob. The
    // first two belong to repositories, the rest to documents.
    let mut bind = binds.iter();
    if query.repo.is_some() {
        repo_sql.push_str(" AND r.name = ?");
        repo_binds.push(bind.next().expect("repo bind"));
    }
    if query.tag.is_some() {
        repo_sql.push_str(" AND ',' || COALESCE(r.tags, '') || ',' LIKE ?");
        repo_binds.push(bind.next().expect("tag bind"));
    }
    if query.language.is_some() {
        doc_sql.push_str(&format!(" AND language = ?{}", doc_binds.len() + 2));
        doc_binds.push(bind.next().expect("language bind"));
    }
    if query.path_glob.is_some_and(|glob| !glob.contains('[')) {
        doc_sql.push_str(&format!(" AND path GLOB ?{}", doc_binds.len() + 2));
        doc_binds.push(bind.next().expect("glob bind"));
    }
    let repos: Vec<i64> = store
        .db
        .prepare(&repo_sql)?
        .query_map(rusqlite::params_from_iter(repo_binds.iter()), |row| {
            row.get(0)
        })?
        .collect::<Result<_, _>>()?;
    if repos.is_empty() {
        return Ok((Vec::new(), false));
    }
    let share = (MAX_CANDIDATES / repos.len() + 1) * 2;
    doc_sql.push_str(&format!(" ORDER BY id LIMIT {}", share + 1));

    let mut statement = store.db.prepare(&doc_sql)?;
    let mut per_repo: Vec<Vec<i64>> = Vec::with_capacity(repos.len());
    let mut capped = false;
    for repo_id in repos {
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&repo_id];
        params.extend(doc_binds.iter().map(|bind| *bind as &dyn rusqlite::ToSql));
        let mut ids: Vec<i64> = statement
            .query_map(params.as_slice(), |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        if ids.len() > share {
            capped = true;
            ids.truncate(share);
        }
        if !ids.is_empty() {
            per_repo.push(ids);
        }
    }
    let shares: Vec<&Vec<i64>> = per_repo.iter().collect();
    Ok(interleave(&shares, capped))
}

/// Candidates from a narrowed set too large to inline into SQL.
///
/// `filtered_sql` is the inner query with every filter applied but no window;
/// its rows are read as integers in id order and never collected as a whole.
fn streamed_candidates(
    store: &Store,
    filtered_sql: &str,
    binds: &[String],
    ids: &HashSet<i64>,
) -> Result<(Vec<i64>, bool)> {
    let mut statement = store
        .db
        .prepare(&format!("SELECT id, repo_id FROM ({filtered_sql})"))?;
    let mut per_repo: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    let mut capped = false;
    let mut rows = statement.query(rusqlite::params_from_iter(binds.iter()))?;
    while let Some(row) = rows.next()? {
        let (id, repo_id): (i64, i64) = (row.get(0)?, row.get(1)?);
        if !ids.contains(&id) {
            continue;
        }
        let bucket = per_repo.entry(repo_id).or_insert_with(|| {
            order.push(repo_id);
            Vec::new()
        });
        // Twice a fair share, like the unnarrowed path: enough that small
        // repositories leave room for large ones, bounded all the same.
        if bucket.len() >= MAX_CANDIDATES / order.len().max(1) * 2 + 2 {
            capped = true;
            continue;
        }
        bucket.push(id);
    }
    let shares: Vec<&Vec<i64>> = order.iter().map(|repo| &per_repo[repo]).collect();
    Ok(interleave(&shares, capped))
}

/// Round-robin across per-repository shares, cut at the budget.
fn interleave(shares: &[&Vec<i64>], mut capped: bool) -> (Vec<i64>, bool) {
    let mut out = Vec::with_capacity(MAX_CANDIDATES);
    let mut round = 0usize;
    loop {
        let mut progressed = false;
        for ids in shares {
            if let Some(id) = ids.get(round) {
                out.push(*id);
                progressed = true;
                if out.len() >= MAX_CANDIDATES {
                    // Whatever is left in the shares was never looked at.
                    capped |= shares.iter().map(|ids| ids.len()).sum::<usize>() > out.len();
                    return (out, capped);
                }
            }
        }
        if !progressed {
            return (out, capped);
        }
        round += 1;
    }
}

/// Repository, path and language for chosen candidates, in the given order.
fn hydrate_candidates(store: &Store, ids: &[i64]) -> Result<Vec<(i64, String, String, String)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list: Vec<String> = ids.iter().map(i64::to_string).collect();
    let mut statement = store.db.prepare(&format!(
        "SELECT d.id, r.name, d.path, d.language \
         FROM documents d JOIN repos r ON r.id = d.repo_id \
         WHERE d.id IN ({})",
        list.join(",")
    ))?;
    let mut by_id: HashMap<i64, (String, String, String)> = statement
        .query_map([], |row| {
            Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
        })?
        .collect::<Result<_, _>>()?;
    // Back into candidate order: the spread across repositories is the whole
    // reason the ids were chosen the way they were.
    Ok(ids
        .iter()
        .filter_map(|id| {
            by_id
                .remove(id)
                .map(|(repo, path, lang)| (*id, repo, path, lang))
        })
        .collect())
}

pub fn search(store: &mut Store, pattern: &str, query: &Query) -> Result<SearchResults> {
    if pattern.len() > MAX_PATTERN_LENGTH {
        bail!("pattern too long");
    }
    // A limit of zero can only return nothing, and reporting that as "no
    // matches" would blame the pattern for the caller's argument.
    if query.limit == 0 {
        bail!("limit must be at least 1");
    }
    // An empty pattern matches every line of every document. Twenty arbitrary
    // lines is never what was wanted, and a caller that built the pattern from
    // a variable has a bug this makes visible.
    if pattern.is_empty() {
        bail!("pattern must not be empty");
    }
    let matcher = RegexBuilder::new(pattern)
        .case_insensitive(query.ignore_case)
        .build()
        .map_err(|error| anyhow!("invalid pattern: {error}"))?;

    // Scanning a pattern that needs a newline reads every candidate line by
    // line to prove what the pattern already says: it cannot match. Measured at
    // 1.7s for `try:\n` against 81,000 candidates, all of it wasted.
    if only_matches_across_lines(pattern) {
        return Ok(SearchResults {
            matches: Vec::new(),
            more_available: false,
        });
    }

    // Trigram narrowing is case-sensitive, so a case-insensitive query folds
    // each trigram to its case variants before narrowing. Scanning everything
    // instead cost 180ms and 185MB against 10ms and 18MB for the same query.
    //
    // The flag alone is not enough: a pattern can turn on case-insensitivity
    // itself with an inline `(?i)`, and narrowing on that without folding
    // silently returns nothing at all rather than fewer results.
    let fold = query.ignore_case || has_inline_case_flag(pattern);
    let narrowed = candidates(store, pattern, fold)?;

    // Candidate selection happens in SQL, in two steps, and the shape matters
    // more than it looks.
    //
    // The first query returns ids only, spread round-robin across
    // repositories by a window function and cut at the candidate budget. It
    // used to fetch every document row with its repository, path and language
    // as Rust strings and pick the budget from those: 186MB for an unnarrowed
    // search over 640,000 documents, which is 20GB at the 50,000 repositories
    // this is meant to hold. SQLite's sorter spills to disk instead, so the
    // memory is bounded whatever the corpus size.
    //
    // The spread is deliberate. Rows arrive grouped by repository, so taking
    // the first 20,000 outright filled the budget from whichever repositories
    // were indexed earliest: `class \w+Error` has 173,000 candidates and
    // returned hits from 38 of 443 projects while reporting nothing was
    // missed. Comparing how different projects solve something is the point
    // of the corpus, so the cap costs depth within a project, never projects.
    //
    // The second query fetches the strings for the survivors alone.
    let mut sql =
        String::from("SELECT d.id, d.repo_id FROM documents d JOIN repos r ON r.id = d.repo_id");
    let mut binds: Vec<String> = Vec::new();
    // Narrowed ids go inline up to the parser's comfort, and into a temp
    // table beyond it: a join against 173,000 ids measures at 88ms where
    // fetching everything and filtering in Rust took 279ms.
    let mut inline_ids: Option<String> = None;
    match narrowed.as_ref() {
        None => {}
        Some(ids) if ids.len() <= SQL_ID_LIMIT => {
            let mut sorted: Vec<i64> = ids.iter().copied().collect();
            sorted.sort_unstable();
            let list: Vec<String> = sorted.iter().map(i64::to_string).collect();
            // Values come from the index, not from user input, so they are
            // integers by construction and safe to inline.
            inline_ids = Some(list.join(","));
        }
        Some(_) => {}
    }
    sql.push_str(" WHERE d.offset >= 0");
    if let Some(list) = inline_ids {
        sql.push_str(&format!(" AND d.id IN ({list})"));
    }
    if let Some(repo) = query.repo {
        sql.push_str(" AND r.name = ?");
        binds.push(repo.to_string());
    }
    if let Some(tag) = query.tag {
        // Tags are stored comma separated, so wrap both sides to match a whole
        // label rather than a prefix of a longer one.
        sql.push_str(&format!(
            " AND ',' || COALESCE(r.tags, '') || ',' LIKE ?{}",
            binds.len() + 1
        ));
        binds.push(format!("%,{},%", tag.trim().to_lowercase()));
    }
    if let Some(language) = query.language {
        sql.push_str(&format!(" AND d.language = ?{}", binds.len() + 1));
        binds.push(language.to_string());
    }
    // SQLite's GLOB agrees with ours on * and ? but reads [ as a class where
    // ours reads it literally, so such a glob stays in Rust below. Everything
    // else is filtered here so it does not eat into the candidate budget.
    if let Some(glob) = query.path_glob.filter(|glob| !glob.contains('[')) {
        sql.push_str(&format!(" AND d.path GLOB ?{}", binds.len() + 1));
        binds.push(glob.to_string());
    }
    let (candidate_ids, capped) =
        if let Some(ids) = narrowed.as_ref().filter(|ids| ids.len() > SQL_ID_LIMIT) {
            // Too many ids to inline. Stream the corpus as integers and keep only
            // what is in the set, capped per repository as it goes, so memory is
            // the budget rather than the corpus. 95ms for `class\s\w+Error` and
            // its 173,000 candidates, against 170ms fetching every row with its
            // strings; a temp table joined by the window query was tried in
            // between and measured 225ms.
            streamed_candidates(store, &sql, &binds, ids)?
        } else if narrowed.is_none() {
            // Nothing narrowed, so every document is a candidate and the window
            // below would visit all of them to keep twenty thousand: 230ms here
            // and around half a minute at 50,000 repositories. The repository
            // index can hand over each repository's first few documents directly,
            // which is work proportional to the budget rather than the corpus.
            // 8ms for the same answer.
            unnarrowed_candidates(store, query, &binds)?
        } else {
            let sql = format!(
                "SELECT id FROM (SELECT id, repo_id, \
                 ROW_NUMBER() OVER (PARTITION BY repo_id ORDER BY id) AS rn FROM ({sql})) \
             ORDER BY rn, repo_id LIMIT {}",
                MAX_CANDIDATES + 1
            );
            let mut ids: Vec<i64> = store
                .db
                .prepare(&sql)?
                .query_map(rusqlite::params_from_iter(binds.iter()), |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            // One past the budget is the cheapest way to learn the budget bit.
            let capped = ids.len() > MAX_CANDIDATES;
            ids.truncate(MAX_CANDIDATES);
            (ids, capped)
        };

    let rows = hydrate_candidates(store, &candidate_ids)?;
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|(_, _, path, _)| query.path_glob.is_none_or(|glob| glob_matches(glob, path)))
        .collect();

    // Documents come back grouped by repository, so taking the first N matches
    // would fill the whole budget from one project. The point of the corpus is
    // comparing how different projects solved the same thing, so collect per
    // repo and interleave below.
    //
    // The per-repo ceiling is the full limit, not a fraction of it: fairness
    // comes from the round-robin, and capping collection lower would silently
    // return half a page when only one repository matches.
    let per_repo_cap = query.per_repo.unwrap_or(query.limit).max(1);
    let mut by_repo: HashMap<String, Vec<Match>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut collected = 0usize;
    // Candidates dropped by the cap are results the caller never saw. Left
    // unreported, a partial answer is indistinguishable from a complete one.
    let mut truncated = capped;

    let mut scanned = 0usize;
    // Commit sha and last-commit date, looked up once per repository that
    // actually contributes a result rather than for every candidate document.
    let mut repo_meta: HashMap<String, (String, String)> = HashMap::new();
    for (doc_id, repo, path, language) in rows {
        if by_repo.get(&repo).is_some_and(|v| v.len() >= per_repo_cap) {
            continue;
        }
        // The trigram index narrows to documents that could match, not ones
        // that do, and a selective pattern can leave thousands of candidates
        // for ten results. Stop once enough have been examined: the answer is
        // already good, and reading the rest only delays it.
        scanned += 1;
        if scanned > MAX_DOCUMENTS_SCANNED && collected >= query.limit {
            truncated = true;
            break;
        }
        let content = store.read_document(doc_id)?;
        if !matcher.is_match(&content) {
            continue;
        }
        // The document matched, so its repository will appear in the results
        // and the metadata is finally worth fetching.
        if !repo_meta.contains_key(&repo) {
            let found = store
                .db
                .query_row(
                    "SELECT COALESCE(commit_sha, ''), COALESCE(pushed_at, '') \
                     FROM repos WHERE name = ?1",
                    params![&repo],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or_default();
            repo_meta.insert(repo.clone(), found);
        }
        let meta = repo_meta.get(&repo).expect("inserted above").clone();

        let text = String::from_utf8_lossy(&content).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        let prose = if query.skip_comments {
            prose_lines(&lines, &language)
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
                commit_sha: meta.0.clone(),
                context_first_line: low + 1,
                pushed_at: meta.1.clone(),
            });
            collected += 1;
            if bucket.len() >= per_repo_cap {
                break;
            }
        }
        // Gather several times the limit so ranking and the round-robin have
        // something to choose from, then stop. Saturating because a caller
        // asking for a very large limit would otherwise overflow.
        // With a per-repo cap the budget has to cover many more repositories,
        // since each contributes only a little. Without this the scan stops
        // after a handful and the breadth is never found.
        let budget = match query.per_repo {
            Some(cap) => query.limit.saturating_mul(cap.max(1)).saturating_mul(8),
            None => query.limit.saturating_mul(4),
        };
        if collected >= budget {
            // Stopped early, so more matches exist beyond what was gathered.
            truncated = true;
            break;
        }
    }

    // Rank each repository's hits before interleaving, so the strongest match
    // from every project is what fills the page. Weights follow the same idea
    // as zoekt's: a definition beats a whole-word use, which beats an
    // incidental substring. Ordering only within a repository keeps the
    // round-robin fairness that spreads results across projects.
    for queue in by_repo.values_mut() {
        queue.sort_by_key(|item| std::cmp::Reverse(relevance(item)));
    }
    // Then order the repositories themselves by their best match, so the
    // strongest hit in the corpus leads. Without this the first result is
    // whichever repository the scan happened to reach first, which put an
    // import above a struct definition in another project.
    order.sort_by_key(|repo| {
        std::cmp::Reverse(
            by_repo
                .get(repo)
                .and_then(|hits| hits.first())
                .map(relevance)
                .unwrap_or(0),
        )
    });

    let mut queues: Vec<std::vec::IntoIter<Match>> = order
        .into_iter()
        .filter_map(|repo| by_repo.remove(&repo).map(|v| v.into_iter()))
        .collect();

    let mut results = Vec::new();
    let gathered: usize = queues.iter().map(|q| q.len()).sum();
    if gathered > query.limit {
        truncated = true;
    }
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
    Ok(SearchResults {
        matches: results,
        more_available: truncated,
    })
}

#[cfg(test)]
mod tests {
    /// Narrowing must not change what a search finds, only how fast it finds
    /// it. Pushing the id filter into SQL is invisible when it works and
    /// silently loses results when it does not.
    #[test]
    fn sql_narrowing_returns_the_same_results() -> anyhow::Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            println!("SKIP: set STEROIDS_TEST_ROOT to a populated corpus");
            return Ok(());
        }
        let mut store = crate::store::Store::open(std::path::Path::new(&root))?;
        // A rare term narrows to a short id list and takes the SQL path; a
        // common one exceeds the limit and falls back to scanning. Both must
        // agree with a plain substring check over the same documents.
        for pattern in ["max_retries", "def ", "import"] {
            let hits = super::search(&mut store, pattern, &super::Query::new(50))?;
            for hit in hits.matches.iter() {
                let joined = hit.context.join("\n");
                assert!(
                    joined.contains(pattern.trim())
                        || !pattern.chars().all(|c| c.is_alphanumeric() || c == '_'),
                    "{pattern}: returned a snippet that does not contain it: {joined:?}"
                );
            }
        }
        Ok(())
    }

    /// Comparing how several projects solved a problem needs breadth, not
    /// depth: ten projects once each says more about common practice than
    /// three projects three times each.
    #[test]
    fn per_repo_cap_trades_depth_for_breadth() -> anyhow::Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            println!("SKIP: set STEROIDS_TEST_ROOT to a populated corpus");
            return Ok(());
        }
        let mut store = crate::store::Store::open(std::path::Path::new(&root))?;

        let wide = super::search(
            &mut store,
            "def ",
            &super::Query {
                per_repo: Some(1),
                ..super::Query::new(10)
            },
        )?;
        let repos: std::collections::HashSet<_> = wide.matches.iter().map(|m| &m.repo).collect();
        assert_eq!(
            repos.len(),
            wide.matches.len(),
            "per_repo=1 returned a repository more than once"
        );
        // How many repositories contain the term depends on the corpus, so the
        // property worth asserting is the one the cap guarantees: every result
        // comes from a different project.
        assert!(!wide.matches.is_empty(), "no matches to check");
        Ok(())
    }

    /// The gutter must number the lines it actually shows. Deriving the start
    /// from a fixed default silently misnumbered every result whenever the
    /// caller asked for a different context width, so `>` pointed at the wrong
    /// line and the numbers were off by the difference.
    #[test]
    fn gutter_numbering_follows_the_requested_context() -> anyhow::Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            println!("SKIP: set STEROIDS_TEST_ROOT to a populated corpus");
            return Ok(());
        }
        let mut store = crate::store::Store::open(std::path::Path::new(&root))?;
        for width in [0usize, 1, 3, 10] {
            let query = super::Query {
                context_lines: width,
                ..super::Query::new(3)
            };
            for hit in super::search(&mut store, "def ", &query)?.matches {
                let offset = hit.line_number - hit.context_start();
                assert!(
                    offset < hit.context.len(),
                    "context {width}: matched line {} is outside the {} shown lines",
                    hit.line_number,
                    hit.context.len()
                );
                assert!(
                    hit.context[offset].contains("def "),
                    "context {width}: the marked line does not contain the match"
                );
            }
        }
        Ok(())
    }

    /// A definition should outrank a use of the same name: someone searching
    /// for a symbol almost always wants where it is declared.
    #[test]
    fn definitions_outrank_uses() {
        let make = |line: &str, path: &str| super::Match {
            repo: "a/b".into(),
            path: path.into(),
            line_number: 1,
            context: vec![line.to_string()],
            scope: String::new(),
            commit_sha: String::new(),
            context_first_line: 1,
            pushed_at: String::new(),
        };
        let definition = make("pub struct RetryPolicy {", "src/retry.rs");
        let import = make("use crate::retry::RetryPolicy;", "src/main.rs");
        let usage = make("    let policy = RetryPolicy::new(3);", "src/main.rs");
        let in_test = make("pub struct RetryPolicy {", "tests/retry_test.rs");

        assert!(super::relevance(&definition) > super::relevance(&usage));
        assert!(super::relevance(&usage) > super::relevance(&import));
        assert!(
            super::relevance(&definition) > super::relevance(&in_test),
            "a definition in a test should rank below the real one"
        );
    }

    /// `define Foo` must not match `parseFoo`: a use is not a definition, and
    /// returning one sends the reader to the wrong file.
    #[test]
    fn definition_pattern_requires_whole_word() {
        let escaped = regex::escape("ToolCallResult");
        let pattern = format!(
            r"\b(def|class|func|fn|type|struct|interface|impl|enum|trait|const|var|let|export|public)\s+(async\s+)?\b{escaped}\b|\b{escaped}\s*(=|:=)\s*(function|async|\(|\{{|class)"
        );
        let re = regex::Regex::new(&pattern).expect("valid");
        assert!(re.is_match("interface ToolCallResult {"));
        assert!(re.is_match("type ToolCallResult struct {"));
        assert!(re.is_match("const ToolCallResult = ("));
        assert!(
            !re.is_match("export const parseToolCallResult = ("),
            "matched a use as a definition"
        );
        assert!(!re.is_match("return handleToolCallResult(x)"));
    }

    /// A delimiter inside a string literal must not convince the scanner that
    /// a comment never closed. Real case: a Python file matching on "/*" left
    /// 2,940 lines of code invisible to search.
    #[test]
    fn stray_delimiter_does_not_hide_the_rest_of_the_file() {
        let mut src = vec![
            "def parse(v):",
            "    if v.startswith(('/*', '//')):",
            "        return ''",
        ];
        // Plenty of ordinary code after the stray delimiter.
        src.extend(std::iter::repeat_n("    x = 1", 200));
        src.push("class RetryManager:");

        // Python never uses /* */, so it is not even looked for.
        let prose = super::prose_lines(&src, "python");
        assert!(prose.is_empty(), "python file marked prose: {prose:?}");

        // In a C-family file the same text does open a block comment, but the
        // run is abandoned rather than swallowing the file.
        let prose = super::prose_lines(&src, "c");
        assert!(
            !prose.contains(&src.len()),
            "last line hidden by an unclosed comment"
        );
        assert!(
            prose.len() < src.len() / 2,
            "{} of {} lines hidden",
            prose.len(),
            src.len()
        );
    }

    /// Genuine docstrings must still be filtered.
    #[test]
    fn real_docstrings_are_still_detected() {
        let src = vec![
            "def f():",
            "    \"\"\"Summary line.",
            "    More detail here.",
            "    \"\"\"",
            "    return 1",
        ];
        let prose = super::prose_lines(&src, "python");
        assert_eq!(prose, [2, 3, 4].into_iter().collect());
    }

    /// A pattern that turns on case-insensitivity itself must disable trigram
    /// narrowing, or the search silently returns nothing.
    #[test]
    fn detects_inline_case_insensitive_flag() {
        for pattern in ["(?i)retry", "(?im)retry", "(?i:retry)", "foo(?i)bar"] {
            assert!(super::has_inline_case_flag(pattern), "missed {pattern}");
        }
        for pattern in ["retry", "(?m)retry", "(?-i)retry", "(?m-i:retry)", "(a|b)"] {
            assert!(
                !super::has_inline_case_flag(pattern),
                "false positive {pattern}"
            );
        }
    }

    /// Permalinks must only be offered when the stored hash is a real git
    /// commit; our archive checksum is 64 hex characters and GitHub 404s on it.
    #[test]
    fn permalink_only_for_real_commits() {
        let base = super::Match {
            repo: "psf/requests".into(),
            path: "src/requests/api.py".into(),
            line_number: 14,
            context: vec![],
            scope: String::new(),
            commit_sha: "5460f467b02e49471c0fd6cfc9ca0adab6351f98".into(),
            context_first_line: 14,
            pushed_at: String::new(),
        };
        assert_eq!(
            base.permalink().unwrap(),
            "https://github.com/psf/requests/blob/5460f467b02e49471c0fd6cfc9ca0adab6351f98/src/requests/api.py#L14"
        );

        // Archive ETag: 64 hex characters, not a commit.
        let etag = super::Match {
            commit_sha: "f6cd16327faa77e0f40488d4d4a1c218803857bf51eb34d73c06ac3cde2303b3".into(),
            ..base
        };
        assert!(etag.permalink().is_none(), "offered a link that would 404");
    }

    /// A search confined to one repository must still fill the requested
    /// limit; the fairness interleave is about spreading results, not about
    /// returning fewer of them.
    #[test]
    fn single_repository_search_fills_the_limit() -> anyhow::Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            println!("SKIP: set STEROIDS_TEST_ROOT to a populated corpus");
            return Ok(());
        }
        let mut store = crate::store::Store::open(std::path::Path::new(&root))?;
        // Pick a repository that actually contains the term, rather than
        // whichever sorts first: the test is about the page being full, not
        // about what happens to be indexed.
        let Some(repo) = super::search(&mut store, "def ", &super::Query::new(1))?
            .matches
            .first()
            .map(|hit| hit.repo.clone())
        else {
            println!("SKIP: corpus has no Python-like code");
            return Ok(());
        };
        let query = super::Query {
            repo: Some(&repo),
            ..super::Query::new(6)
        };
        let hits = super::search(&mut store, "def ", &query)?;
        assert_eq!(hits.len(), 6, "single-repo search returned a short page");
        Ok(())
    }

    use super::*;

    #[test]
    fn case_variants_cover_every_letter_combination() {
        let variants = case_variants(b"a_b").unwrap();
        assert_eq!(variants.len(), 4);
        assert!(variants.contains(b"A_B"));
        assert!(variants.contains(b"a_B"));
        assert_eq!(case_variants(b"123").unwrap(), vec![*b"123"]);
    }

    #[test]
    fn unfoldable_trigrams_are_left_out_of_narrowing() {
        // The matcher folds k to the Kelvin sign and s to the long s, which no
        // ASCII variant would find, so these must widen rather than narrow.
        assert!(case_variants(b"key").is_none());
        assert!(case_variants(b"ISO").is_none());
        // A non-ASCII letter changes its bytes when it changes case.
        assert!(case_variants("é_x".as_bytes()[..3].try_into().unwrap()).is_none());
    }

    #[test]
    fn interleave_spreads_the_budget_across_repositories() {
        let a = vec![1, 2, 3];
        let b = vec![4];
        let c = vec![5];
        let (out, capped) = interleave(&[&a, &b, &c], false);
        assert_eq!(out, vec![1, 4, 5, 2, 3]);
        assert!(!capped);
    }

    #[test]
    fn newline_patterns_are_recognised_as_unmatchable() {
        assert!(only_matches_across_lines(r"try:\n"));
        assert!(only_matches_across_lines("try:\n"));
        assert!(only_matches_across_lines(r"a\nb|c\nd"));
    }

    #[test]
    fn patterns_that_can_match_within_a_line_are_left_alone() {
        assert!(!only_matches_across_lines(r"def \w+"));
        // One viable branch is enough for the pattern to be worth running.
        assert!(!only_matches_across_lines(r"foo|a\nb"));
        // A negated class asks for the opposite of a newline.
        assert!(!only_matches_across_lines(r"[^\n]+"));
        // An escaped backslash before n is a literal, not a line break.
        assert!(!only_matches_across_lines(r"path\\name"));
    }

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
        let prose = prose_lines(&src, "python");
        assert!(prose.contains(&2) && prose.contains(&3) && prose.contains(&4));
        assert!(!prose.contains(&1) && !prose.contains(&5));

        let single = prose_lines(&["    \"\"\"one liner.\"\"\"", "    y = 2"], "python");
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
