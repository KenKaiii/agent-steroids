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

/// A plain read of a file longer than this prints its outline, and an
/// outline or folder listing stops here.
pub const READ_CAP_CHARS: usize = 6_000;

/// Most characters a read the caller asked for explicitly (a range, `full`,
/// definitions by name) returns before it is cut. Measured in races, a model
/// round trip costs more than 4,000 tokens read once: cutting at 6,000 sent
/// agents back for the rest of a file they had already decided to read.
pub const EXPLICIT_CAP_CHARS: usize = 16_000;

/// How `show` words its next steps. Plain names, not flag syntax: an agent
/// calling a built-in tool knows these as parameters, a shell user as flags.
const HOW_TO_READ: &str =
    "read bodies by name (symbols), or code that is not a definition by line range (from/to)";

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
    /// Add the same-repo helpers the shown code calls.
    pub uses: bool,
    pub json: bool,
}

/// Looks up the helpers shown lines use; a hook so tests need no corpus.
pub type UsesLookup<'a> = dyn FnMut(&[(usize, usize)]) -> crate::deps::Uses + 'a;

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
///
/// A plain read of a file over [`READ_CAP_CHARS`] becomes its outline. A
/// range, `full` or a set of bodies is the code asked for, up to
/// [`EXPLICIT_CAP_CHARS`] (or a smaller token budget), then cut where a line
/// ends with the call that continues it. Turning ranges and `full` into
/// outlines too taught agents to ask for `full` on every read.
pub fn render_show(
    lines: &[&str],
    request: &ShowRequest,
    lookup: &mut UsesLookup,
) -> anyhow::Result<String> {
    let language = crate::filters::language_of(request.path).unwrap_or("");
    let test_file = is_test_path(request.path);
    let total = lines.len();
    let budget = |ceiling: usize| {
        request
            .max_tokens
            .map_or(ceiling, |tokens| ceiling.min(tokens.saturating_mul(4)))
    };
    let cap = budget(EXPLICIT_CAP_CHARS);
    let outline_cap = budget(READ_CAP_CHARS);
    let mut notes: Vec<String> = Vec::new();
    // Contiguous runs of lines to print, 1-based and inclusive.
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    let mut outlined = false;
    let mut next_step: Option<String> = None;
    let (start, end) = request.range;
    let hidden = if request.no_comments {
        comment_lines(lines, language)
    } else {
        HashSet::new()
    };
    let strip = |range: Vec<(usize, usize)>| {
        if request.no_comments {
            without_comments(lines, range, &hidden)
        } else {
            (range, 0)
        }
    };

    if !request.symbols.is_empty() {
        let mut found = Vec::new();
        let mut bodies: Vec<(usize, usize, &str)> = Vec::new();
        for symbol in request.symbols {
            let symbol = symbol.trim();
            let defined = definitions(lines, symbol, language);
            if defined.is_empty() {
                missing.push(symbol);
            }
            for body in defined {
                let last = body.end.min(body.start + MAX_BODY_LINES - 1);
                if last < body.end {
                    notes.push(format!(
                        "{symbol} cut at {MAX_BODY_LINES} lines; it runs to line {}",
                        body.end
                    ));
                }
                found.push(format!("{symbol} {}-{}", body.start, body.end));
                bodies.push((body.start, last, symbol));
            }
        }
        if bodies.is_empty() {
            anyhow::bail!(
                "no definition of {} in {}/{}; its outline lists what it defines",
                missing.join(", "),
                request.repo,
                request.path
            );
        }
        bodies.sort_unstable();
        bodies.dedup_by_key(|body| (body.0, body.1));
        // Whole bodies, in file order, while they fit; the rest are named so
        // the next call asks for exactly those.
        let mut used = 0usize;
        let mut dropped = 0usize;
        let mut later: Vec<&str> = Vec::new();
        for (first, last, symbol) in bodies {
            let (pieces, hidden_here) = strip(vec![(first, last)]);
            let size = printed_chars(lines, &pieces);
            if used + size > cap && !blocks.is_empty() {
                if !later.contains(&symbol) {
                    later.push(symbol);
                }
                continue;
            }
            dropped += hidden_here;
            if size > cap {
                let (kept, next) = within_chars(lines, &pieces, cap);
                blocks.extend(kept);
                if let Some(next) = next {
                    notes.push(format!(
                        "{symbol} stopped at {cap} characters; continue with from={next} to={last}"
                    ));
                }
                used = cap;
            } else {
                blocks.extend(pieces);
                used += size;
            }
        }
        notes.insert(0, found.join(", "));
        if !missing.is_empty() {
            notes.push(format!("not defined here: {}", missing.join(", ")));
        }
        if !later.is_empty() {
            notes.push(format!(
                "past {cap} characters, not shown: {}; ask for them next (symbols={})",
                later.join(", "),
                later.join(",")
            ));
        }
        if request.no_comments {
            notes.push(format!("{dropped} comment lines hidden, and blank ones"));
        }
    } else if start > total {
        // Not an error: a caller paging forward has simply reached the end.
        notes.push(format!("file has {total} lines; nothing at line {start}"));
    } else {
        let (range, dropped) = strip(vec![(start, end)]);
        let size = printed_chars(lines, &range);
        let scope = if request.ranged {
            (start, end)
        } else {
            (1, total)
        };
        // The file's size decides, not a smaller budget: a tight budget on a
        // short file still wants its code, cut where the budget ends.
        let picked: Vec<usize> =
            if request.outline || (!request.ranged && !request.full && size > READ_CAP_CHARS) {
                outline(lines, language, test_file)
                    .into_iter()
                    .filter(|line| (scope.0..=scope.1).contains(line))
                    .collect()
            } else {
                Vec::new()
            };
        if request.outline || !picked.is_empty() {
            outlined = true;
            let what = if request.ranged {
                format!("lines {start}-{end} of {total}")
            } else {
                format!("{total} lines")
            };
            notes.push(if request.outline {
                format!("outline of {what}; {HOW_TO_READ}")
            } else {
                format!("{what} would be {size} characters, so this is the outline; {HOW_TO_READ}")
            });
            if test_file {
                notes.push("test cases only".into());
            }
            next_step = next_step_example(lines, &picked, test_file);
            blocks = picked.into_iter().map(|line| (line, line)).collect();
            // An outline is a map, and a map past the cap is not one.
            let (kept, next) = within_chars(lines, &blocks, outline_cap);
            if let Some(next) = next {
                blocks = kept;
                notes.push(format!(
                    "outline stops at line {}; continue it with outline from={next}",
                    next - 1
                ));
            }
        } else {
            // A range, `full`, or a long file that declares nothing: the
            // lines asked for, up to the cap, and where to continue.
            let (kept, next) = within_chars(lines, &range, cap);
            blocks = kept;
            let shown_end = blocks.last().map_or(end, |block| block.1);
            notes.push(format!("lines {start}-{shown_end} of {total}"));
            if let Some(next) = next {
                notes.push(format!(
                    "stopped at {cap} characters; continue with from={next}"
                ));
            } else if !request.ranged && end < total {
                notes.push(format!("continue with from={}", end + 1));
            }
            if request.no_comments {
                notes.push(format!("{dropped} comment lines hidden, and blank ones"));
            }
        }
    }

    // Code was shown: add what it calls, so the next hop is not a call.
    let uses = if request.uses && !outlined && !blocks.is_empty() {
        lookup(&blocks)
    } else {
        crate::deps::Uses::default()
    };

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
            "next": next_step,
            "uses": uses.to_json(),
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
    out.push_str(&uses.render());
    // Last, where the reader's eye is when it decides what to do: a worked
    // call steered agents to `symbols` where the header's advice did not.
    if let Some(step) = next_step {
        out.push_str(&format!("\n# next: {step}\n"));
    }
    Ok(out)
}

/// A ready-to-use next call after an outline, built from the file's own
/// names: bodies by name, or in a test file the first case by its lines.
fn next_step_example(lines: &[&str], picked: &[usize], test_file: bool) -> Option<String> {
    if test_file {
        let cases: Vec<usize> = picked
            .iter()
            .copied()
            .filter(|number| {
                let trimmed = lines[number - 1].trim_start();
                ["it(", "test(", "def test", "fn test", "func Test"]
                    .iter()
                    .any(|word| trimmed.starts_with(word))
            })
            .collect();
        let first = *cases.first()?;
        let last = picked
            .iter()
            .copied()
            .find(|number| *number > first)
            .map_or(first + 30, |next| next - 1);
        return Some(format!(
            "from={first} to={last} reads that test case; one range per case you need"
        ));
    }
    let mut names: Vec<&str> = Vec::new();
    for number in picked {
        if let Some(name) = declared_name(lines[number - 1])
            && !names.contains(&name)
        {
            names.push(name);
        }
        if names.len() == 3 {
            break;
        }
    }
    if names.is_empty() {
        return None;
    }
    Some(format!(
        "symbols={} reads just those bodies (use this file's names); from/to only for code that is not a definition",
        names.join(",")
    ))
}

/// The name a declaration line introduces: `isPlainObject` from
/// `const isPlainObject = (val) => {`, `forEach` from `function forEach(`.
pub(crate) fn declared_name(line: &str) -> Option<&str> {
    const MODIFIERS: &[&str] = &[
        "export ",
        "default ",
        "async ",
        "static ",
        "public ",
        "private ",
        "protected ",
        "abstract ",
        "unsafe ",
        "extern ",
    ];
    const KEYWORDS: &[&str] = &[
        "function* ",
        "function ",
        "const ",
        "let ",
        "var ",
        "class ",
        "async def ",
        "def ",
        "fn ",
        "func ",
        "fun ",
        "type ",
        "interface ",
        "struct ",
        "enum ",
        "trait ",
        "module ",
        "mod ",
    ];
    let mut rest = without_visibility(line.trim_start());
    while let Some(stripped) = MODIFIERS.iter().find_map(|word| rest.strip_prefix(word)) {
        rest = stripped;
    }
    let after_keyword = KEYWORDS.iter().find_map(|word| rest.strip_prefix(word));
    let rest = match after_keyword {
        // Go methods: `func (r *Reader) Read(` names `Read`.
        Some(after) if after.starts_with('(') => after.split_once(") ")?.1,
        Some(after) => after,
        None => method_name(rest)?,
    };
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(rest.len());
    let name = &rest[..end];
    (!name.is_empty() && !name.chars().next().is_some_and(|c| c.is_ascii_digit())).then_some(name)
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
pub(crate) fn numbered(number: usize, line: &str) -> String {
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

    fn render_show_noop(lines: &[&str], request: &ShowRequest) -> anyhow::Result<String> {
        render_show(lines, request, &mut |_| crate::deps::Uses::default())
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
            uses: false,
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
        let out = render_show_noop(&lines, &request(&symbols)).unwrap();
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
        assert!(render_show_noop(&lines, &request(&none)).is_err());
    }

    #[test]
    fn show_without_comments_keeps_line_numbers() {
        let lines = lines();
        let out = render_show_noop(
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
        let out = render_show_noop(
            &lines,
            &ShowRequest {
                max_tokens: Some(30),
                ..request(&[])
            },
        )
        .unwrap();
        let next: usize = out
            .split("continue with from=")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse().ok())
            .expect("no continuation offered");
        assert!(out.contains(&format!("{}\t", next - 1)) && !out.contains(&format!("\n{next}\t")));
        let json = render_show_noop(
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

    /// Only a plain read of a long file becomes its outline, which ends
    /// with a worked next call. A range or `full` is the code asked for, up
    /// to the cap, with where to continue: never an outline in its place.
    #[test]
    fn plain_reads_outline_ranges_and_full_are_capped_code() {
        let mut lines = vec!["describe('x', () => {", "  it('works', () => {"];
        lines.extend(std::iter::repeat_n(
            "    assert.strictEqual(serialize({ a: [1, 2, 3] }), 'a[]=1&a[]=2&a[]=3');",
            400,
        ));
        lines.push("  it('also works', () => {");
        let total = lines.len();
        let test = ShowRequest {
            path: "tests/x.test.js",
            range: (1, total),
            ..request(&[])
        };
        let out = render_show_noop(&lines, &test).unwrap();
        assert!(out.contains(&format!("{total} lines would be")), "{out}");
        assert!(!out.contains("--"), "flag syntax in a hint: {out}");
        assert!(
            out.contains("1\tdescribe(") && out.contains("403\t  it('also works'"),
            "{out}"
        );
        assert!(!out.contains("assert.strictEqual"), "{out}");
        assert!(
            out.ends_with(
                "# next: from=2 to=402 reads that test case; one range per case you need\n"
            ),
            "{out}"
        );

        for request in [
            ShowRequest { full: true, ..test },
            ShowRequest {
                range: (3, 400),
                ranged: true,
                ..test
            },
        ] {
            let read = render_show_noop(&lines, &request).unwrap();
            assert!(
                read.contains("\t    assert.strictEqual"),
                "an outline instead of code: {read}"
            );
            assert!(read.len() < EXPLICIT_CAP_CHARS + 400, "{}", read.len());
            assert!(read.contains("continue with from="), "{read}");
            assert!(!read.contains("# next:"), "{read}");
        }
    }

    #[test]
    fn symbols_past_the_cap_are_named_for_the_next_call() {
        let mut lines: Vec<String> = Vec::new();
        for name in ["alpha", "beta", "gamma"] {
            lines.push(format!("function {name}(input) {{"));
            lines.extend(std::iter::repeat_n(
                "  input = transform(input, { deep: true, keepOrder: true, label: 'step' });"
                    .to_string(),
                80,
            ));
            lines.push("}".to_string());
        }
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let symbols = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        let out = render_show_noop(&lines, &request(&symbols)).unwrap();
        assert!(
            out.contains("1\tfunction alpha(") && out.contains("83\tfunction beta("),
            "{out}"
        );
        assert!(!out.contains("function gamma("), "{out}");
        assert!(
            out.contains("not shown: gamma; ask for them next (symbols=gamma)"),
            "{out}"
        );
        assert!(out.len() < EXPLICIT_CAP_CHARS + 400, "{}", out.len());
    }

    #[test]
    fn declared_names_come_from_the_line() {
        for (line, name) in [
            ("const isPlainObject = (val) => {", "isPlainObject"),
            ("export default async function forEach(obj) {", "forEach"),
            ("pub(crate) fn search_path(", "search_path"),
            ("func (r *Reader) Read(p []byte) (int, error) {", "Read"),
            ("  static int count(char *s) {", "count"),
            ("class AxiosHeaders {", "AxiosHeaders"),
        ] {
            assert_eq!(declared_name(line), Some(name), "{line}");
        }
        assert_eq!(declared_name("if (x) {"), None);
    }
}
