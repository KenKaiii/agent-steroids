//! A repository's changelog, split into releases, so "what changed in v1.21.0"
//! costs one section rather than a search through tests for the behaviour.

use anyhow::{Result, bail};

/// Root files that record releases, best first.
const NAMES: &[&str] = &["changelog", "changes", "history", "releases", "news"];
const EXTENSIONS: &[&str] = &["", ".md", ".markdown", ".rst", ".txt"];

/// How much of a changelog is kept: the newest releases are at the top, and
/// a porting task wants those, not a decade of history.
pub const MAX_STORED_BYTES: usize = 64 * 1024;

/// Rank of a root-level changelog file name, lower is better; None for any
/// other path.
pub fn rank(path: &str) -> Option<usize> {
    if path.contains('/') {
        return None;
    }
    let lowered = path.to_ascii_lowercase();
    NAMES.iter().position(|name| {
        EXTENSIONS
            .iter()
            .any(|extension| lowered == format!("{name}{extension}"))
    })
}

/// The first `MAX_STORED_BYTES` of a changelog, cut at a line end.
pub fn head(bytes: &[u8]) -> String {
    let cut = if bytes.len() > MAX_STORED_BYTES {
        bytes[..MAX_STORED_BYTES]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(MAX_STORED_BYTES, |end| end + 1)
    } else {
        bytes.len()
    };
    String::from_utf8_lossy(&bytes[..cut]).into_owned()
}

/// One release: its heading text and its lines (0-based, end exclusive).
#[derive(Debug, PartialEq)]
pub struct Release {
    pub heading: String,
    pub start: usize,
    pub end: usize,
}

/// A heading: its 0-based line, depth (1 = top) and text.
struct Heading {
    line: usize,
    depth: usize,
    text: String,
}

fn headings(lines: &[&str]) -> Vec<Heading> {
    let mut found = Vec::new();
    let mut fenced = false;
    // Underline characters in the order a document first uses them: RST and
    // setext headings take their depth from that order.
    let mut underlines: Vec<char> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            found.push(Heading {
                line: index,
                depth: hashes,
                text: trimmed[hashes..].trim().trim_end_matches('#').trim().into(),
            });
            continue;
        }
        let Some(next) = lines.get(index + 1).map(|next| next.trim()) else {
            continue;
        };
        let Some(mark) = next.chars().next() else {
            continue;
        };
        if !trimmed.is_empty()
            && !trimmed.starts_with(['-', '*', '+', '|'])
            && next.len() >= 3
            && "=-~^".contains(mark)
            && next.chars().all(|c| c == mark)
        {
            if !underlines.contains(&mark) {
                underlines.push(mark);
            }
            let depth = underlines.iter().position(|c| *c == mark).unwrap_or(0) + 1;
            found.push(Heading {
                line: index,
                depth,
                text: trimmed.into(),
            });
        }
    }
    found
}

/// `1.2`, `v1.2.3`, `2024.1`: what a release heading carries.
fn version_in(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    // A version opens a word, or follows the `v` of `v1.2`.
    let opens =
        |at: usize| at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'.');
    let mut index = 0;
    while index < bytes.len() {
        let starts = bytes[index].is_ascii_digit()
            && (opens(index) || (matches!(bytes[index - 1], b'v' | b'V') && opens(index - 1)));
        if !starts {
            index += 1;
            continue;
        }
        let end = text[index..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+'))
            .map_or(text.len(), |offset| index + offset);
        let candidate = text[index..end].trim_end_matches(['.', '-', '+']);
        let major_is_number = candidate
            .split('.')
            .next()
            .is_some_and(|major| !major.is_empty() && major.chars().all(|c| c.is_ascii_digit()));
        if candidate.contains('.') && major_is_number {
            return Some(candidate);
        }
        index = end.max(index + 1);
    }
    None
}

fn is_unreleased(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    lowered.contains("unreleased") || lowered.contains("upcoming")
}

/// Releases in document order, newest first in any changelog that keeps
/// the usual order.
pub fn releases(lines: &[&str]) -> Vec<Release> {
    let headings = headings(lines);
    // The depth that most often carries a version is the release depth; the
    // other headings at it (axios puts "## New Features" beside "## v1.21.0")
    // belong to the release above them.
    let mut counts = [0usize; 8];
    for heading in &headings {
        if version_in(&heading.text).is_some() {
            counts[heading.depth.min(7)] += 1;
        }
    }
    let Some(depth) = (1..8).max_by_key(|depth| (counts[*depth], std::cmp::Reverse(*depth))) else {
        return Vec::new();
    };
    if counts[depth] == 0 {
        return Vec::new();
    }
    let starts: Vec<&Heading> = headings
        .iter()
        .filter(|heading| {
            heading.depth == depth
                && (version_in(&heading.text).is_some() || is_unreleased(&heading.text))
        })
        .collect();
    starts
        .iter()
        .map(|start| {
            let end = headings
                .iter()
                .find(|heading| {
                    heading.line > start.line
                        && (heading.depth < depth
                            || starts.iter().any(|other| other.line == heading.line))
                })
                .map_or(lines.len(), |heading| heading.line);
            Release {
                heading: start.text.clone(),
                start: start.line,
                end,
            }
        })
        .collect()
}

/// The release a `--version` names: same digits, a leading `v` optional.
fn find<'a>(releases: &'a [Release], wanted: &str) -> Option<&'a Release> {
    let wanted = wanted.trim().trim_start_matches(['v', 'V']);
    releases
        .iter()
        .find(|release| version_in(&release.heading) == Some(wanted))
        .or_else(|| {
            releases
                .iter()
                .find(|release| version_in(&release.heading).is_some_and(|v| v.starts_with(wanted)))
        })
}

/// `[#11156](https://github.com/…/pull/11156)` as `#11156`: the link text
/// carries what a reader uses, and the URL is often half the line's tokens.
fn without_link_targets(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find("](") {
        let Some(label_start) = rest[..open].rfind('[') else {
            out.push_str(&rest[..open + 2]);
            rest = &rest[open + 2..];
            continue;
        };
        let Some(close) = rest[open + 2..].find(')') else {
            break;
        };
        let target = &rest[open + 2..open + 2 + close];
        if target.contains(char::is_whitespace) {
            out.push_str(&rest[..open + 2]);
            rest = &rest[open + 2..];
            continue;
        }
        // An image's `!` goes with it: there is nothing to read.
        let image = rest[..label_start].ends_with('!');
        out.push_str(&rest[..label_start - usize::from(image)]);
        if !image {
            out.push_str(&rest[label_start + 1..open]);
        }
        rest = &rest[open + 2 + close + 1..];
    }
    out.push_str(rest);
    out
}

/// Longest line the contents keep: enough for a label and its gist.
const SUMMARY_LINE_CHARS: usize = 90;

/// One line of a release entry as its summary shows it: headings whole, a
/// top-level item or paragraph cut to its first sentence, everything else
/// (nested items, tables, code, blank lines) left to the full entry.
fn summary_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('|') || line.starts_with([' ', '\t']) {
        return None;
    }
    if trimmed.starts_with('#') {
        return Some(trimmed.to_string());
    }
    // A sentence ends at a full stop followed by a space, past any short
    // label such as `**Header Defaults:**`.
    let end = trimmed
        .match_indices(". ")
        .map(|(at, _)| at + 1)
        .find(|at| *at >= 20)
        .unwrap_or(trimmed.len());
    let sentence = &trimmed[..end];
    if sentence.chars().count() <= SUMMARY_LINE_CHARS {
        return Some(sentence.to_string());
    }
    let cut: String = sentence.chars().take(SUMMARY_LINE_CHARS).collect();
    Some(format!("{}…", cut.trim_end()))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Headings and each item's opening words.
    Contents,
    /// Only the items a pattern matches, whole, under their headings.
    Matching,
    Full,
}

enum Unit<'a> {
    Heading(&'a str),
    /// A top-level line and everything nested under it.
    Item(Vec<&'a str>),
}

/// An entry's lines as headings and items. Blank lines go; code blocks,
/// tables and nested lines stay with the item they follow.
fn units<'a>(lines: &[&'a str]) -> Vec<Unit<'a>> {
    let mut units: Vec<Unit<'a>> = Vec::new();
    let mut fenced = false;
    for line in lines {
        let trimmed = line.trim();
        let fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        let nested = fenced || fence || line.starts_with([' ', '\t']) || trimmed.starts_with('|');
        if fence {
            fenced = !fenced;
        }
        if trimmed.is_empty() && !fenced {
            continue;
        }
        if trimmed.starts_with('#') && !nested {
            units.push(Unit::Heading(trimmed));
        } else if let (true, Some(Unit::Item(item))) = (nested, units.last_mut()) {
            item.push(line);
        } else {
            units.push(Unit::Item(vec![line]));
        }
    }
    units
}

pub struct Request<'a> {
    pub repo: &'a str,
    pub path: &'a str,
    pub version: Option<&'a str>,
    pub list: bool,
    /// The whole entry rather than its contents.
    pub full: bool,
    /// Only the items this matches, whole.
    pub matching: Option<regex::Regex>,
    pub max_tokens: usize,
    pub json: bool,
}

pub fn render(text: &str, request: &Request) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let releases = releases(&lines);
    let names: Vec<String> = releases
        .iter()
        .map(|release| without_link_targets(&release.heading))
        .collect();
    if request.list {
        if request.json {
            return Ok(serde_json::json!({
                "repo": request.repo,
                "path": request.path,
                "releases": names,
            })
            .to_string());
        }
        let mut out = format!(
            "# {}/{}: {} releases in the stored part, newest first\n",
            request.repo,
            request.path,
            names.len()
        );
        for name in &names {
            out.push_str(name);
            out.push('\n');
        }
        return Ok(out);
    }

    let (heading, start, end, note) = if releases.is_empty() {
        (
            String::new(),
            0,
            lines.len(),
            "no release headings found; showing the top".to_string(),
        )
    } else if let Some(wanted) = request.version {
        let Some(release) = find(&releases, wanted) else {
            let recent: Vec<&str> = names.iter().take(15).map(String::as_str).collect();
            bail!(
                "no release {wanted} in {}/{} (stored part only); recent ones: {}",
                request.repo,
                request.path,
                recent.join(" | ")
            );
        };
        (
            release.heading.clone(),
            release.start,
            release.end,
            String::new(),
        )
    } else {
        // An empty "Unreleased" placeholder is not what "latest" means.
        let latest = releases
            .iter()
            .find(|release| {
                !is_unreleased(&release.heading)
                    // An underline is not content; a placeholder is all marks.
                    || lines[release.start + 1..release.end]
                        .iter()
                        .any(|line| line.chars().any(char::is_alphanumeric))
            })
            .unwrap_or(&releases[0]);
        (
            latest.heading.clone(),
            latest.start,
            latest.end,
            "the latest; ask for another by version, or for the list of releases".to_string(),
        )
    };

    // The entry's own heading is already in the header line.
    let body_start = if heading.is_empty() { start } else { start + 1 };
    let units = units(&lines[body_start..end]);
    let mode = match (&request.matching, request.full) {
        (Some(_), _) => Mode::Matching,
        (None, true) => Mode::Full,
        (None, false) => Mode::Contents,
    };
    let mut picked: Vec<String> = Vec::new();
    let mut pending_heading: Option<&str> = None;
    let mut matched = 0usize;
    for unit in &units {
        match unit {
            Unit::Heading(text) => {
                if mode == Mode::Matching {
                    pending_heading = Some(text);
                } else {
                    picked.push((*text).to_string());
                }
            }
            Unit::Item(item) => match mode {
                Mode::Full => picked.extend(item.iter().map(|line| (*line).to_string())),
                Mode::Contents => picked.extend(summary_line(item[0])),
                Mode::Matching => {
                    let pattern = request.matching.as_ref().expect("matching mode");
                    if item.iter().any(|line| pattern.is_match(line)) {
                        matched += 1;
                        picked.extend(pending_heading.take().map(str::to_string));
                        picked.extend(item.iter().map(|line| (*line).to_string()));
                    }
                }
            },
        }
    }

    let mut used = 0usize;
    let mut shown: Vec<String> = Vec::new();
    let mut cut = false;
    for line in picked {
        let line = without_link_targets(&line);
        used += crate::render::estimate_tokens(&line) + 1;
        if used > request.max_tokens && !shown.is_empty() {
            cut = true;
            break;
        }
        shown.push(line);
    }
    while shown.last().is_some_and(|line| line.trim().is_empty()) {
        shown.pop();
    }
    if request.json {
        return Ok(serde_json::json!({
            "repo": request.repo,
            "path": request.path,
            "release": heading,
            "mode": match mode {
                Mode::Contents => "contents",
                Mode::Matching => "matching",
                Mode::Full => "full",
            },
            "lines": shown,
            "truncated": cut,
        })
        .to_string());
    }
    let mut out = format!("# {}/{}", request.repo, request.path);
    if !heading.is_empty() {
        out.push_str(&format!(": {heading}"));
    }
    if !note.is_empty() {
        out.push_str(&format!(" ({note})"));
    }
    out.push('\n');
    out.push_str(match mode {
        Mode::Contents => "# contents: each item's opening words; ask for the items that mention a word (pattern), or the full entry (full)\n".to_string(),
        Mode::Matching => format!("# {matched} item(s) matching the pattern, whole\n"),
        Mode::Full => String::new(),
    }.as_str());
    for line in &shown {
        out.push_str(line);
        out.push('\n');
    }
    if cut {
        out.push_str(&format!(
            "… stopped at the {}-token budget\n",
            request.max_tokens
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AXIOS: &str = "# Changelog

## v1.21.0 — October 21, 2026

This release adds response timing.

## ⚠️ Breaking Changes & Deprecations

* **Header Defaults:** only plain objects.

## 🐛 Bug Fixes

* Circular parameters now reject with `ERR_BAD_OPTION_VALUE`.

## v1.20.0 — September 2, 2026

* Older change.
";

    fn request<'a>(version: Option<&'a str>, list: bool) -> Request<'a> {
        Request {
            repo: "axios/axios",
            path: "CHANGELOG.md",
            version,
            list,
            full: true,
            matching: None,
            max_tokens: 3000,
            json: false,
        }
    }

    #[test]
    fn subsections_at_the_release_depth_stay_in_their_release() {
        let lines: Vec<&str> = AXIOS.lines().collect();
        let found = releases(&lines);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].heading, "v1.21.0 — October 21, 2026");
        assert_eq!((found[0].start, found[0].end), (2, 14));
        let latest = render(AXIOS, &request(None, false)).unwrap();
        assert!(latest.contains("ERR_BAD_OPTION_VALUE"), "{latest}");
        assert!(!latest.contains("Older change"), "{latest}");
        let older = render(AXIOS, &request(Some("1.20.0"), false)).unwrap();
        assert!(
            older.contains("Older change") && !older.contains("Circular"),
            "{older}"
        );
        assert!(render(AXIOS, &request(Some("v9.9.9"), false)).is_err());
        let list = render(AXIOS, &request(None, true)).unwrap();
        assert!(
            list.contains("v1.20.0") && !list.contains("Bug Fixes"),
            "{list}"
        );
    }

    #[test]
    fn rst_underlines_and_unreleased_placeholders() {
        let rst = "Changelog\n=========\n\nUnreleased\n----------\n\n2.32.3 (2024-05-29)\n-------------------\n\n- Fixed a bug.\n\n2.32.2\n------\n\n- Another.\n";
        let lines: Vec<&str> = rst.lines().collect();
        let found: Vec<String> = releases(&lines).into_iter().map(|r| r.heading).collect();
        assert_eq!(found, ["Unreleased", "2.32.3 (2024-05-29)", "2.32.2"]);
        let latest = render(rst, &request(None, false)).unwrap();
        assert!(
            latest.contains("2.32.3") && latest.contains("Fixed a bug"),
            "{latest}"
        );
    }

    #[test]
    fn versions_and_names() {
        assert_eq!(version_in("v1.21.0 — October 21, 2026"), Some("1.21.0"));
        assert_eq!(
            version_in("[2.0.0-beta.1] - 2024-01-01"),
            Some("2.0.0-beta.1")
        );
        assert_eq!(version_in("Release 15.1"), Some("15.1"));
        assert_eq!(version_in("🐛 Bug Fixes"), None);
        assert_eq!(version_in("Python 3 support"), None);
        assert_eq!(rank("CHANGELOG.md"), Some(0));
        assert_eq!(rank("HISTORY.rst"), Some(2));
        assert_eq!(rank("docs/CHANGELOG.md"), None);
        assert_eq!(rank("changelog.js"), None);
        let long = "line\n".repeat(MAX_STORED_BYTES);
        let kept = head(long.as_bytes());
        assert!(kept.len() <= MAX_STORED_BYTES && kept.ends_with('\n'));
    }

    #[test]
    fn link_targets_are_dropped_and_labels_kept() {
        assert_eq!(
            without_link_targets(
                "* **Fix:** it. (**[#11156](https://github.com/axios/axios/pull/11156)**)"
            ),
            "* **Fix:** it. (**#11156**)"
        );
        assert_eq!(
            without_link_targets(
                "[1.13.3](https://x/compare/a...b) (2026) ![badge](https://i/b.svg) ok"
            ),
            "1.13.3 (2026)  ok"
        );
        assert_eq!(
            without_link_targets("call f(x) and g[0](y z)"),
            "call f(x) and g[0](y z)"
        );
        assert_eq!(without_link_targets("broken [a](b"), "broken [a](b");
    }

    #[test]
    fn summary_keeps_headings_and_first_sentences() {
        assert_eq!(
            summary_line("## 🐛 Bug Fixes").as_deref(),
            Some("## 🐛 Bug Fixes")
        );
        assert_eq!(
            summary_line(
                "* **Header Defaults:** Only plain objects are defaults. Applications must convert."
            )
            .as_deref(),
            Some("* **Header Defaults:** Only plain objects are defaults.")
        );
        assert_eq!(summary_line("  * nested detail"), None);
        assert_eq!(summary_line("| a | b |"), None);
        assert_eq!(summary_line(""), None);
        let long = format!("* {}", "word ".repeat(60));
        let short = summary_line(&long).unwrap();
        assert!(short.ends_with('…') && short.chars().count() <= SUMMARY_LINE_CHARS + 1);
    }

    #[test]
    fn contents_by_default_whole_items_on_a_pattern() {
        let entry = "# Changelog\n\n## v2.0.0\n\nIntro paragraph that runs on for a while. More.\n\n## Fixes\n\n* **Forms:** Sparse fields serialize as keyed objects. Details follow here.\n  * nested note about forms\n* **Proxy:** TLS options apply. More text.\n\n## Chores\n\n* Bumped deps.\n\n## v1.0.0\n\n* Old.\n";
        let base = |full, matching: Option<&str>| Request {
            repo: "o/r",
            path: "CHANGELOG.md",
            version: None,
            list: false,
            full,
            matching: matching.map(|p| {
                regex::RegexBuilder::new(p)
                    .case_insensitive(true)
                    .build()
                    .unwrap()
            }),
            max_tokens: 3000,
            json: false,
        };
        let contents = render(entry, &base(false, None)).unwrap();
        assert!(
            contents.contains("* **Forms:** Sparse fields serialize as keyed objects.\n"),
            "{contents}"
        );
        assert!(
            !contents.contains("Details follow") && !contents.contains("nested note"),
            "{contents}"
        );
        assert!(!contents.contains("Old."), "{contents}");
        assert_eq!(
            contents.matches("v2.0.0").count(),
            1,
            "heading repeated: {contents}"
        );

        let forms = render(entry, &base(false, Some("FORM"))).unwrap();
        assert!(forms.contains("1 item(s) matching"), "{forms}");
        assert!(
            forms.contains("## Fixes\n* **Forms:**")
                && forms.contains("  * nested note about forms"),
            "{forms}"
        );
        assert!(
            !forms.contains("Proxy") && !forms.contains("## Chores"),
            "{forms}"
        );

        let full = render(entry, &base(true, None)).unwrap();
        assert!(
            full.contains("Details follow here.") && full.contains("Bumped deps."),
            "{full}"
        );
    }
}
