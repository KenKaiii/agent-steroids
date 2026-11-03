//! Reading one file the way an agent needs it: a definition's body rather
//! than the file around it, an outline of what the file declares, or its code
//! without the comments.
//!
//! Every function here works on numbered lines and keeps the original
//! numbers, so whatever an agent reads can be cited and re-read exactly.

use std::collections::HashSet;

use crate::search::{
    binds_function, definition_pattern, indent_of, is_binding_not_definition, is_prose,
    prose_lines, without_visibility,
};

/// Longest body printed for one definition before pointing at the rest.
pub const MAX_BODY_LINES: usize = 150;

/// One definition found in a file: its 1-based first and last line.
#[derive(Debug, PartialEq)]
pub struct Body {
    pub start: usize,
    pub end: usize,
}

/// Where `symbol` is defined in `lines`, and how far each definition runs.
pub fn definitions(lines: &[&str], symbol: &str, language: &str) -> Vec<Body> {
    let Ok(pattern) = regex::Regex::new(&definition_pattern(symbol)) else {
        return Vec::new();
    };
    let prose = prose_lines(lines, language);
    lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            (pattern.is_match(line) || method_name(line.trim_start()) == Some(symbol))
                && !is_binding_not_definition(line, symbol)
                && !is_prose(line)
                && !prose.contains(&(index + 1))
        })
        .map(|(index, _)| Body {
            start: index + 1,
            end: block_end(lines, index + 1, language),
        })
        .collect()
}

/// Last line (1-based, inclusive) of the block that opens at line `start`.
///
/// Braces decide for C-family languages, indentation for Python and for
/// anything that never opens a brace. Approximate by design: a body cut a
/// line early or late is still far better than the whole file.
pub fn block_end(lines: &[&str], start: usize, language: &str) -> usize {
    if start == 0 || start > lines.len() {
        return start.min(lines.len());
    }
    if language != "python"
        && let Some(end) = brace_block_end(lines, start - 1)
    {
        return end;
    }
    indent_block_end(lines, start - 1, language == "python")
}

fn brace_block_end(lines: &[&str], index: usize) -> Option<usize> {
    let header_indent = indent_of(lines[index]);
    let mut depth = 0i32;
    // Braces inside parentheses are destructured parameters or object
    // arguments (`fn, { allOwnKeys = false } = {}`), not the body.
    let mut parens = 0i32;
    let mut opened = false;
    let mut block_comment = false;
    let mut template = false;
    let mut previous = "";
    for (offset, line) in lines[index..].iter().enumerate() {
        let trimmed = line.trim();
        // Nothing opened yet and a new statement starts at the header's own
        // depth: the definition was a single statement without a terminator,
        // and the brace further down belongs to whatever comes next.
        if offset > 0 && !opened && !block_comment && !template && !trimmed.is_empty() {
            let continues_line = trimmed
                .starts_with(['{', '}', ')', ']', '.', '?', ':', '&', '|', '+', '-', '='])
                || trimmed.starts_with("where")
                || previous.ends_with([',', '(', '[', '=', '>', '?', ':', '&', '|', '+', '\\']);
            if indent_of(line) <= header_indent && !continues_line {
                return Some(index + offset);
            }
        }
        if !trimmed.is_empty() {
            previous = trimmed;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut quote = template.then_some('`');
        let mut at = 0usize;
        while at < chars.len() {
            let c = chars[at];
            let next = chars.get(at + 1).copied();
            if block_comment {
                if c == '*' && next == Some('/') {
                    block_comment = false;
                    at += 1;
                }
                at += 1;
                continue;
            }
            if let Some(open) = quote {
                if c == '\\' {
                    at += 2;
                    continue;
                }
                if c == open {
                    quote = None;
                }
                at += 1;
                continue;
            }
            match c {
                '/' if next == Some('/') => break,
                '/' if next == Some('*') => {
                    block_comment = true;
                    at += 1;
                }
                '"' | '`' => quote = Some(c),
                // A quote with no partner on the line is a Rust lifetime or
                // an apostrophe, not a string.
                '\'' if chars[at + 1..].contains(&'\'') => quote = Some(c),
                '(' | '[' => parens += 1,
                ')' | ']' => parens = (parens - 1).max(0),
                '{' if parens == 0 => {
                    depth += 1;
                    opened = true;
                }
                '}' if parens == 0 => {
                    depth -= 1;
                    if opened && depth <= 0 {
                        return Some(index + offset + 1);
                    }
                }
                ';' if !opened && depth == 0 && parens == 0 => return Some(index + offset + 1),
                _ => {}
            }
            at += 1;
        }
        template = quote == Some('`');
        if !opened && offset >= 12 {
            return None;
        }
    }
    None
}

fn indent_block_end(lines: &[&str], index: usize, python: bool) -> usize {
    let header_indent = indent_of(lines[index]);
    // A Python signature can run over several lines; the body starts after
    // the line that ends in a colon.
    let mut body_from = index;
    if python {
        for (offset, line) in lines[index..].iter().enumerate().take(20) {
            let code = line.split(" #").next().unwrap_or(line).trim_end();
            if code.ends_with(':') {
                body_from = index + offset;
                break;
            }
        }
    }
    let mut last = body_from;
    for (offset, line) in lines[body_from + 1..].iter().enumerate() {
        let number = body_from + 1 + offset;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if indent_of(line) > header_indent {
            last = number;
            continue;
        }
        // Ruby and Lua close with `end` at the header's own indent.
        if !python && (trimmed == "end" || trimmed.starts_with(['}', ')', ']'])) {
            last = number;
        }
        break;
    }
    last + 1
}

/// The lines worth seeing to know what a file holds: its declarations, or
/// in a test file its cases and top-level helpers. Assertions are left to a
/// read of the case: listing them made a test outline a third of the file.
pub fn outline(lines: &[&str], language: &str, test_file: bool) -> Vec<usize> {
    let prose = prose_lines(lines, language);
    lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            if prose.contains(&(index + 1)) || is_prose(line) {
                return false;
            }
            if !test_file {
                return is_outline_line(line, false);
            }
            // Helpers and classes defined inside a test body belong to it.
            is_test_case(line.trim_start())
                || (indent_of(line) == 0 && is_outline_line(line, false))
        })
        .map(|(index, _)| index + 1)
        .collect()
}

const TEST_CALLS: &[&str] = &[
    "describe(",
    "it(",
    "test(",
    "context(",
    "describe.",
    "it.",
    "test.",
    "def test",
    "async def test",
    "fn test",
    "func Test",
    "#[test]",
    "@Test",
];

fn is_test_case(trimmed: &str) -> bool {
    TEST_CALLS.iter().any(|word| trimmed.starts_with(word))
}

fn is_outline_line(line: &str, test_file: bool) -> bool {
    const DECLARATIONS: &[&str] = &[
        "def ",
        "async def ",
        "class ",
        "func ",
        "fn ",
        "async fn ",
        "unsafe fn ",
        "const fn ",
        "function ",
        "function*",
        "async function ",
        "fun ",
        "interface ",
        "protocol ",
        "extension ",
        "namespace ",
        "object ",
        "trait ",
        "impl ",
        "impl<",
        "mod ",
        "module ",
        "type ",
        "abstract class ",
        "export ",
        "macro_rules!",
    ];
    // Top-level bindings are how JavaScript defines most of its helpers
    // (`const isString = typeOfTest('string')`), so at column zero they count.
    const TOP_LEVEL: &[&str] = &[
        "const ", "let ", "var ", "static ", "pub ", "public ", "private ",
    ];
    const TEST_CALLS: &[&str] = &[
        "describe(",
        "it(",
        "test(",
        "context(",
        "describe.",
        "it.",
        "test.",
    ];
    const ASSERTIONS: &[&str] = &["expect(", "assert", "await expect(", "should."];

    let trimmed = line.trim_start();
    let bare = without_visibility(trimmed);
    if DECLARATIONS.iter().any(|word| bare.starts_with(word)) || binds_function(trimmed) {
        return true;
    }
    // In C a local is spelled `struct client *c = …`, so a struct or enum
    // counts only where it is declared: at the top, or opening a body.
    if ["struct ", "enum ", "union "]
        .iter()
        .any(|word| bare.starts_with(word))
        && !trimmed.contains('=')
        && (indent_of(line) == 0 || trimmed.ends_with('{'))
    {
        return true;
    }
    if indent_of(line) == 0 && TOP_LEVEL.iter().any(|word| trimmed.starts_with(word)) {
        return true;
    }
    if TEST_CALLS.iter().any(|word| trimmed.starts_with(word))
        || trimmed.starts_with("#[test]")
        || trimmed.starts_with("@Test")
    {
        return true;
    }
    if test_file && ASSERTIONS.iter().any(|word| trimmed.starts_with(word)) {
        return true;
    }
    is_method_header(trimmed)
}

/// A function or method header with no keyword of its own: `name(args) {`
/// in a JavaScript class, `static int count(char *s) {` in C or Java.
fn is_method_header(trimmed: &str) -> bool {
    method_name(trimmed).is_some()
}

/// The name such a header declares.
fn method_name(trimmed: &str) -> Option<&str> {
    const NOT_METHODS: &[&str] = &[
        "if",
        "for",
        "while",
        "switch",
        "catch",
        "return",
        "function",
        "else",
        "do",
        "with",
        "await",
        "new",
        "typeof",
        "super",
        "this",
        "case",
        "throw",
        "yield",
        "delete",
        "sizeof",
        "until",
        "unless",
        "foreach",
        "using",
        "lock",
        "synchronized",
    ];
    // A call taking a callback (`run(() => {`) ends the same way; a header's
    // parameter list closes right before the body.
    if !trimmed.ends_with('{') || trimmed.contains("=>") || trimmed.contains("->") {
        return None;
    }
    let (head, _) = trimmed.split_once('(')?;
    if head.is_empty() || head.contains(['=', '.', ';', '"', '\'', '(', ')', '}', '!']) {
        return None;
    }
    let mut words = head.split_whitespace();
    let first = words.next()?;
    let name = head
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .trim_start_matches(['*', '&']);
    let identifier = |word: &str| {
        !word.is_empty()
            && word
                .chars()
                .all(|c| c.is_alphanumeric() || "_$#:~<>[]*&,?".contains(c))
    };
    let header = !name.is_empty()
        && !NOT_METHODS.contains(&first)
        && !NOT_METHODS.contains(&name)
        && identifier(name)
        && head.split_whitespace().all(identifier)
        && trimmed.contains(')');
    header.then_some(name)
}

/// Lines that are comments or docstrings, by 1-based number.
pub fn comment_lines(lines: &[&str], language: &str) -> HashSet<usize> {
    let mut hidden = prose_lines(lines, language);
    for (index, line) in lines.iter().enumerate() {
        if is_prose(line) {
            hidden.insert(index + 1);
        }
    }
    hidden
}

/// Whether a path looks like a test, so its outline includes assertions.
pub fn is_test_path(path: &str) -> bool {
    let lowered = path.to_ascii_lowercase();
    lowered.contains("test") || lowered.contains("spec")
}

/// Most characters a read returns unless all of it was asked for. Every
/// character an agent reads is paid for again on each later turn; past this,
/// the outline plus a targeted read costs less than the read.
pub const READ_CAP_CHARS: usize = 6_000;

/// How `show` words its next steps. Plain names, not flag syntax: an agent
/// calling a built-in tool knows these as parameters, a shell user as flags.
const HOW_TO_READ: &str =
    "ask for definitions by name (symbols), a smaller line range (from/to), or all of it (full)";

/// What `show` was asked for, after the range defaults are resolved.
pub struct ShowRequest<'a> {
    pub repo: &'a str,
    pub path: &'a str,
    /// First and last line of a plain range read.
    pub range: (usize, usize),
    /// The range was given, rather than the default of the whole file.
    pub ranged: bool,
    pub symbols: &'a [String],
    pub outline: bool,
    /// All of the range, however long.
    pub full: bool,
    pub no_comments: bool,
    pub max_tokens: Option<usize>,
    pub json: bool,
}

/// `lines[start..=end]` without comment lines or blank ones, as runs.
fn without_comments(
    lines: &[&str],
    blocks: Vec<(usize, usize)>,
    hidden: &HashSet<usize>,
) -> (Vec<(usize, usize)>, usize) {
    let mut kept = Vec::new();
    let mut dropped = 0usize;
    for (start, end) in blocks {
        let mut run: Option<(usize, usize)> = None;
        for number in start..=end {
            if hidden.contains(&number) || lines[number - 1].trim().is_empty() {
                dropped += usize::from(hidden.contains(&number));
                kept.extend(run.take());
                continue;
            }
            run = Some(run.map_or((number, number), |(first, _)| (first, number)));
        }
        kept.extend(run);
    }
    (kept, dropped)
}

/// Characters `blocks` print as.
fn printed_chars(lines: &[&str], blocks: &[(usize, usize)]) -> usize {
    blocks
        .iter()
        .flat_map(|&(start, end)| start..=end)
        .map(|number| numbered(number, lines[number - 1]).len() + 1)
        .sum()
}

/// The leading part of `blocks` that prints within `cap` characters (at
/// least one line), and the first line left out.
fn within_chars(
    lines: &[&str],
    blocks: &[(usize, usize)],
    cap: usize,
) -> (Vec<(usize, usize)>, Option<usize>) {
    let mut used = 0usize;
    let mut kept = Vec::new();
    for &(start, end) in blocks {
        for number in start..=end {
            used += numbered(number, lines[number - 1]).len() + 1;
            if used > cap && !(kept.is_empty() && number == start) {
                if number > start {
                    kept.push((start, number - 1));
                }
                return (kept, Some(number));
            }
        }
        kept.push((start, end));
    }
    (kept, None)
}

/// `show`'s answer for one file, text or JSON.
pub fn render_show(lines: &[&str], request: &ShowRequest) -> anyhow::Result<String> {
    let language = crate::filters::language_of(request.path).unwrap_or("");
    let test_file = is_test_path(request.path);
    let total = lines.len();
    let mut notes: Vec<String> = Vec::new();
    // Contiguous runs of lines to print, 1-based and inclusive.
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    let mut outlined = false;
    let (start, end) = request.range;
    let hidden = if request.no_comments {
        comment_lines(lines, language)
    } else {
        HashSet::new()
    };

    if !request.symbols.is_empty() {
        let mut found = Vec::new();
        for symbol in request.symbols {
            let bodies = definitions(lines, symbol.trim(), language);
            if bodies.is_empty() {
                missing.push(symbol.trim());
            }
            for body in bodies {
                let last = body.end.min(body.start + MAX_BODY_LINES - 1);
                if last < body.end {
                    notes.push(format!(
                        "{symbol} cut at {MAX_BODY_LINES} lines; it runs to line {}",
                        body.end
                    ));
                }
                found.push(format!("{symbol} {}-{}", body.start, body.end));
                blocks.push((body.start, last));
            }
        }
        if blocks.is_empty() {
            anyhow::bail!(
                "no definition of {} in {}/{}; its outline lists what it defines",
                missing.join(", "),
                request.repo,
                request.path
            );
        }
        blocks.sort_unstable();
        blocks.dedup();
        notes.insert(0, found.join(", "));
        if !missing.is_empty() {
            notes.push(format!("not defined here: {}", missing.join(", ")));
        }
        if request.no_comments {
            let (kept, dropped) = without_comments(lines, blocks, &hidden);
            blocks = kept;
            notes.push(format!("{dropped} comment lines hidden, and blank ones"));
        }
    } else if start > total {
        // Not an error: a caller paging forward has simply reached the end.
        notes.push(format!("file has {total} lines; nothing at line {start}"));
    } else {
        let mut range = vec![(start, end)];
        let mut dropped = 0;
        if request.no_comments {
            (range, dropped) = without_comments(lines, range, &hidden);
        }
        let size = printed_chars(lines, &range);
        let too_long = !request.full && size > READ_CAP_CHARS;
        let picked: Vec<usize> = if request.outline || too_long {
            let scope = if request.outline && !request.ranged {
                (1, total)
            } else {
                (start, end)
            };
            outline(lines, language, test_file)
                .into_iter()
                .filter(|line| (scope.0..=scope.1).contains(line))
                .collect()
        } else {
            Vec::new()
        };
        if request.outline || (too_long && !picked.is_empty()) {
            outlined = true;
            let scope = if request.ranged {
                format!("lines {start}-{end} of {total}")
            } else {
                format!("{total} lines")
            };
            notes.push(if request.outline {
                format!("outline of {scope}; {HOW_TO_READ}")
            } else {
                format!(
                    "{scope} would be {size} characters, so this is their outline; {HOW_TO_READ}"
                )
            });
            if test_file {
                notes.push("test cases only; read a case by its line range".into());
            }
            blocks = picked.into_iter().map(|line| (line, line)).collect();
            // An outline is a map, and a map past the cap is not one.
            let (kept, next) = within_chars(lines, &blocks, READ_CAP_CHARS);
            if let Some(next) = next {
                blocks = kept;
                notes.push(format!(
                    "outline stops at line {}; ask for the outline from line {next}",
                    next - 1
                ));
            }
        } else {
            // Too long and nothing declared inside: the range is one long
            // body, so give its start and say where to continue.
            let cut = if too_long {
                within_chars(lines, &range, READ_CAP_CHARS)
            } else {
                (range, None)
            };
            blocks = cut.0;
            let shown_end = blocks.last().map_or(end, |block| block.1);
            notes.push(format!("lines {start}-{shown_end} of {total}"));
            if let Some(next) = cut.1 {
                notes.push(format!(
                    "stopped at {READ_CAP_CHARS} characters; continue from line {next}, or ask for all of it (full)"
                ));
            } else if end < total {
                notes.push("ask for a line range (from/to) for the rest".into());
            }
            if request.no_comments {
                notes.push(format!("{dropped} comment lines hidden, and blank ones"));
            }
        }
    }

    // Whole lines only, and at least one, so a small budget still answers.
    if let Some(budget) = request.max_tokens {
        let mut used = 0usize;
        let mut cut: Option<usize> = None;
        let mut kept = Vec::new();
        'blocks: for &(first, last) in &blocks {
            for number in first..=last {
                used += crate::render::estimate_tokens(&numbered(number, lines[number - 1]));
                if used > budget && !(kept.is_empty() && number == first) {
                    if number > first {
                        kept.push((first, number - 1));
                    }
                    cut = Some(number);
                    break 'blocks;
                }
            }
            kept.push((first, last));
        }
        if let Some(next) = cut {
            blocks = kept;
            notes.push(format!(
                "stopped at the {budget}-token budget; continue from line {next}"
            ));
        }
    }

    if request.json {
        let items: Vec<serde_json::Value> = blocks
            .iter()
            .map(|&(first, last)| {
                serde_json::json!({ "from": first, "to": last, "lines": &lines[first - 1..last] })
            })
            .collect();
        return Ok(serde_json::json!({
            "repo": request.repo,
            "path": request.path,
            "total_lines": total,
            "outline": outlined,
            "blocks": items,
            "missing": missing,
            "notes": notes,
        })
        .to_string());
    }

    let mut out = format!(
        "# {}/{}\n# {}\n",
        request.repo,
        request.path,
        notes.join("; ")
    );
    let mut previous_end = None;
    for (first, last) in blocks {
        // A blank line marks where one definition ends and the next begins;
        // gaps inside a range are already visible in the numbering.
        if !request.symbols.is_empty() || previous_end.is_none() {
            out.push('\n');
        }
        for number in first..=last {
            out.push_str(&numbered(number, lines[number - 1]));
            out.push('\n');
        }
        previous_end = Some(last);
    }
    Ok(out)
}

/// The outline of every file under a directory, in path order, within the
/// read cap (or a smaller token budget). Files that do not fit are named, so
/// the reader knows what to ask for next.
pub fn render_folder(
    repo: &str,
    directory: &str,
    files: &[(String, String)],
    max_tokens: Option<usize>,
    json: bool,
) -> String {
    let cap = max_tokens.map_or(READ_CAP_CHARS, |tokens| READ_CAP_CHARS.min(tokens * 4));
    let mut used = 0usize;
    let mut shown: Vec<(&str, Vec<(usize, &str)>)> = Vec::new();
    let mut omitted: Vec<&str> = Vec::new();
    for (path, text) in files {
        let lines: Vec<&str> = text.lines().collect();
        let language = crate::filters::language_of(path).unwrap_or("");
        let picked: Vec<(usize, &str)> = outline(&lines, language, is_test_path(path))
            .into_iter()
            .map(|number| (number, lines[number - 1]))
            .collect();
        let cost = path.len()
            + 5
            + picked
                .iter()
                .map(|(number, line)| numbered(*number, line).len() + 1)
                .sum::<usize>();
        if used + cost > cap && !shown.is_empty() {
            omitted.push(path);
            continue;
        }
        used += cost;
        shown.push((path, picked));
    }
    if json {
        let items: Vec<serde_json::Value> = shown
            .iter()
            .map(|(path, picked)| {
                serde_json::json!({
                    "path": path,
                    "outline": picked
                        .iter()
                        .map(|(number, line)| serde_json::json!([number, line]))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        return serde_json::json!({
            "repo": repo,
            "path": directory,
            "files": items,
            "omitted": omitted,
        })
        .to_string();
    }
    let mut out = format!(
        "# {repo}/{directory}: outline of {} files; ask for definitions by name (symbols) in one file for bodies\n",
        shown.len()
    );
    for (path, picked) in &shown {
        out.push_str(&format!("\n## {path}\n"));
        for (number, line) in picked {
            out.push_str(&numbered(*number, line));
            out.push('\n');
        }
    }
    if !omitted.is_empty() {
        out.push_str(&format!(
            "\n… {} more files, past {cap} characters; ask for their outlines by path: {}\n",
            omitted.len(),
            omitted.join(" ")
        ));
    }
    out
}

/// One line as `show` prints it: a tab after the number aligns like padding
/// would, for fewer tokens than a padded gutter and a box character.
fn numbered(number: usize, line: &str) -> String {
    format!("{number}\t{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTILS: &str = "'use strict';
/**
 * Iterate over an Array or an Object invoking a function for each item.
 */
function forEach(obj, fn, { allOwnKeys = false } = {}) {
  if (obj === null || typeof obj === 'undefined') {
    return;
  }
  // Force an array if not already something iterable
  if (typeof obj !== 'object') {
    obj = [obj];
  }
}

const isString = typeOfTest('string');

const isPlainObject = (val) => {
  if (kindOf(val) !== 'object') {
    return false;
  }
  const brace = '}';
  return true;
};

const asap = typeof setImmediate === 'function'
  ? setImmediate
  : queueMicrotask;

export default {
  forEach,
  isPlainObject,
};";

    fn lines() -> Vec<&'static str> {
        UTILS.lines().collect()
    }

    #[test]
    fn bodies_run_from_the_header_to_the_closing_brace() {
        let lines = lines();
        assert_eq!(
            definitions(&lines, "forEach", "javascript"),
            vec![Body { start: 5, end: 13 }]
        );
        // A brace inside a string does not close the block.
        assert_eq!(
            definitions(&lines, "isPlainObject", "javascript"),
            vec![Body { start: 17, end: 23 }]
        );
        assert_eq!(block_end(&lines, 15, "javascript"), 15);
        assert_eq!(block_end(&lines, 25, "javascript"), 27);
    }

    /// axios's `kindOf`: the body opens inside an immediately-called arrow.
    #[test]
    fn an_immediately_called_function_ends_at_its_call() {
        let lines = [
            "const kindOf = ((cache) => (thing) => {",
            "  const str = toString.call(thing);",
            "  return cache[str];",
            "})(Object.create(null));",
            "",
            "const next = 1;",
        ];
        assert_eq!(block_end(&lines, 1, "javascript"), 4);
    }

    #[test]
    fn a_statement_without_terminator_ends_before_the_next_one() {
        let lines = [
            "const double = x => x * 2",
            "function next() {",
            "  go();",
            "}",
        ];
        assert_eq!(block_end(&lines, 1, "javascript"), 1);
        assert_eq!(block_end(&lines, 2, "javascript"), 4);
    }

    #[test]
    fn python_bodies_follow_indentation() {
        let lines = [
            "class Store:",
            "    def open(",
            "        self,",
            "    ):",
            "        lock()",
            "",
            "        return self",
            "    def close(self):",
            "        pass",
        ];
        assert_eq!(block_end(&lines, 2, "python"), 7);
        assert_eq!(block_end(&lines, 1, "python"), 9);
        let found = definitions(&lines, "close", "python");
        assert_eq!(found, vec![Body { start: 8, end: 9 }]);
    }

    #[test]
    fn rust_lifetimes_do_not_open_strings() {
        let lines = [
            "impl<'a> Reader<'a> {",
            "    fn next(&mut self) -> char {",
            "        '}'",
            "    }",
            "}",
        ];
        assert_eq!(block_end(&lines, 1, "rust"), 5);
        assert_eq!(block_end(&lines, 2, "rust"), 4);
    }

    #[test]
    fn outline_lists_declarations_not_bodies() {
        let lines = lines();
        assert_eq!(
            outline(&lines, "javascript", false),
            vec![5, 15, 17, 25, 29]
        );
        let class = [
            "class AxiosHeaders {",
            "  constructor(headers) {",
            "    if (headers) {",
            "  static from(thing) {",
            "  get(header, parser) {",
        ];
        assert_eq!(outline(&class, "javascript", false), vec![1, 2, 4, 5]);
    }

    /// smallchat-server.c: functions carry no keyword, and `struct` starts
    /// local variables as often as declarations.
    #[test]
    fn c_and_java_outlines_find_typed_signatures() {
        let c = [
            "struct client {",
            "    int fd;",
            "};",
            "struct chatState *Chat;",
            "struct client *createClient(int fd) {",
            "    struct client *c = chatMalloc(sizeof(*c));",
            "    if (c == NULL) {",
            "    for (int j = 0; j < 10; j++) {",
            "void freeClient(struct client *c) {",
            "int main(void) {",
            "    run(() => {",
            "    } else if (x) {",
        ];
        assert_eq!(outline(&c, "c", false), vec![1, 4, 5, 9, 10]);
        assert_eq!(
            definitions(&c, "createClient", "c"),
            vec![Body { start: 5, end: 8 }]
        );
        let java = [
            "public class Joiner {",
            "    public static String join(List<String> parts, char sep) {",
            "        return String.join(sep, parts);",
            "    private Map<K, V> cache() {",
            "        synchronized (lock) {",
        ];
        assert_eq!(outline(&java, "java", false), vec![1, 2, 4]);
    }

    #[test]
    fn test_outline_lists_cases_not_assertions() {
        let lines = [
            "describe('toFormData', function () {",
            "  it('should throw on depth', function () {",
            "    const data = nest(101);",
            "    expect(() => toFormData(data)).toThrow('Max depth: 100');",
            "  });",
            "});",
        ];
        assert_eq!(outline(&lines, "javascript", true), vec![1, 2]);
        assert_eq!(outline(&lines, "javascript", false), vec![1, 2]);
    }

    /// A class or accessor defined inside a test is part of that test.
    #[test]
    fn test_outline_skips_helpers_nested_in_cases() {
        let lines = [
            "const helper = (x) => x;",
            "describe('headers', () => {",
            "  it('reads a bag', () => {",
            "    class HeaderBag {",
            "      constructor() {",
            "    assert.strictEqual(bag.get('x'), '1');",
        ];
        assert_eq!(outline(&lines, "javascript", true), vec![1, 2, 3]);
    }

    #[test]
    fn comments_and_docstrings_are_found() {
        let lines = lines();
        let hidden = comment_lines(&lines, "javascript");
        for number in [2, 3, 4, 9] {
            assert!(hidden.contains(&number), "line {number} not hidden");
        }
        assert!(!hidden.contains(&5) && !hidden.contains(&10));
    }

    fn request<'a>(symbols: &'a [String]) -> ShowRequest<'a> {
        ShowRequest {
            repo: "axios/axios",
            path: "lib/utils.js",
            range: (1, 31),
            ranged: false,
            symbols,
            outline: false,
            full: false,
            no_comments: false,
            max_tokens: None,
            json: false,
        }
    }

    #[test]
    fn show_symbols_prints_only_the_bodies() {
        let lines = lines();
        let symbols = vec![
            "isPlainObject".to_string(),
            "forEach".to_string(),
            "nope".to_string(),
        ];
        let out = render_show(&lines, &request(&symbols)).unwrap();
        assert!(out.contains("5\tfunction forEach("), "{out}");
        assert!(out.contains("23\t};"), "{out}");
        assert!(out.contains("not defined here: nope"), "{out}");
        assert!(
            !out.contains("isString"),
            "printed more than the bodies: {out}"
        );
        assert!(
            !out.contains("Iterate over"),
            "printed the doc comment: {out}"
        );
        let none = vec!["nope".to_string()];
        assert!(render_show(&lines, &request(&none)).is_err());
    }

    #[test]
    fn show_without_comments_keeps_line_numbers() {
        let lines = lines();
        let out = render_show(
            &lines,
            &ShowRequest {
                no_comments: true,
                ..request(&[])
            },
        )
        .unwrap();
        assert!(
            !out.contains("Iterate over") && !out.contains("Force an array"),
            "{out}"
        );
        assert!(
            out.contains("10\t  if (typeof obj !== 'object') {"),
            "{out}"
        );
        assert!(out.contains("4 comment lines"), "{out}");
    }

    #[test]
    fn show_budget_says_where_to_continue() {
        let lines = lines();
        let out = render_show(
            &lines,
            &ShowRequest {
                max_tokens: Some(30),
                ..request(&[])
            },
        )
        .unwrap();
        let next: usize = out
            .split("continue from line ")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse().ok())
            .expect("no continuation offered");
        assert!(out.contains(&format!("{}\t", next - 1)) && !out.contains(&format!("\n{next}\t")));
        let json = render_show(
            &lines,
            &ShowRequest {
                json: true,
                outline: true,
                ..request(&[])
            },
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["blocks"][0]["from"], 5);
    }

    #[test]
    fn folder_outline_covers_every_file_within_budget() {
        let files = vec![
            (
                "lib/a.js".to_string(),
                "const one = (x) => x;\n// note\nfunction two() {\n  return 1;\n}\n".to_string(),
            ),
            (
                "lib/b.js".to_string(),
                "export function three() {}\n".to_string(),
            ),
        ];
        let out = render_folder("o/r", "lib", &files, None, false);
        assert!(
            out.contains("## lib/a.js\n1\tconst one = (x) => x;\n3\tfunction two() {"),
            "{out}"
        );
        assert!(
            out.contains("## lib/b.js\n1\texport function three() {}"),
            "{out}"
        );
        assert!(!out.contains("note") && !out.contains("return 1"), "{out}");
        let tight = render_folder("o/r", "lib", &files, Some(1), false);
        assert!(
            tight.contains(
                "1 more files, past 4 characters; ask for their outlines by path: lib/b.js"
            ),
            "{tight}"
        );
        let json = render_folder("o/r", "lib", &files, None, true);
        assert!(
            json.contains(r#""outline":[[1,"const one = (x) => x;"]"#),
            "{json}"
        );
    }

    /// A read past the cap becomes the outline of what was asked for, says
    /// why in plain words, and lists a test file's cases only. `full` gets
    /// all of it; a range with nothing declared in it is cut and continued.
    #[test]
    fn long_reads_become_outlines_unless_full() {
        let mut lines = vec!["describe('x', () => {", "  it('works', () => {"];
        lines.extend(std::iter::repeat_n(
            "    assert.strictEqual(serialize({ a: [1, 2, 3] }), 'a[]=1&a[]=2&a[]=3');",
            200,
        ));
        lines.push("  it('also works', () => {");
        let total = lines.len();
        let test = ShowRequest {
            path: "tests/x.test.js",
            range: (1, total),
            ..request(&[])
        };
        let out = render_show(&lines, &test).unwrap();
        assert!(out.contains(&format!("{total} lines would be")), "{out}");
        assert!(out.contains("ask for definitions by name (symbols), a smaller line range (from/to), or all of it (full)"), "{out}");
        assert!(!out.contains("--"), "flag syntax in a hint: {out}");
        assert!(
            out.contains("1\tdescribe(") && out.contains("203\t  it('also works'"),
            "{out}"
        );
        assert!(!out.contains("assert.strictEqual"), "{out}");

        let all = render_show(&lines, &ShowRequest { full: true, ..test }).unwrap();
        assert!(
            all.contains("100\t    assert.strictEqual"),
            "full was not all of it"
        );

        // Lines 3-200 hold no case: cut at the cap, with where to continue.
        let body = render_show(
            &lines,
            &ShowRequest {
                range: (3, 200),
                ranged: true,
                ..test
            },
        )
        .unwrap();
        assert!(body.len() < READ_CAP_CHARS + 300, "{}", body.len());
        assert!(body.contains("continue from line"), "{body}");
    }
}
