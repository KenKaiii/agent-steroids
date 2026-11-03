//! The same-repo code a read depends on: the helpers the shown lines call,
//! from this file and from the files it imports.
//!
//! Agents read a function, saw it call `utils.forEach`, read utils' outline,
//! then forEach, then the helpers forEach calls: one model round trip per
//! hop, each re-sending the whole conversation. Returning the small helpers
//! with the read turns that chain into one call.

use std::collections::{BTreeMap, HashSet};

use crate::outline::{comment_lines, declared_name, definitions, numbered, outline};

/// Most characters the uses section may add to a read. It is also never more
/// than the code read itself (at least [`USES_FLOOR_CHARS`]): a ten-line
/// range does not need five thousand characters of helpers.
pub const USES_CAP_CHARS: usize = 5_000;
pub const USES_FLOOR_CHARS: usize = 1_500;

/// The uses budget for a read that printed `shown_chars` characters of code.
pub fn uses_cap(shown_chars: usize) -> usize {
    shown_chars.clamp(USES_FLOOR_CHARS, USES_CAP_CHARS)
}

/// A helper with this many code lines or fewer is shown whole; a longer one
/// as its outline, or named if it has none.
const WHOLE_BODY_LINES: usize = 60;

/// Most helpers resolved before packing, however many the code names.
const MAX_CANDIDATES: usize = 80;

/// Lines of a long helper's outline shown in its place.
const OUTLINE_LINES: usize = 12;

/// How many hops from the shown code helpers are followed.
const MAX_DEPTH: usize = 2;

/// One helper the shown code uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Use {
    pub path: String,
    pub name: String,
    pub start: usize,
    pub end: usize,
    /// Line numbers printed for it: the whole body, or its outline.
    pub lines: Vec<usize>,
    pub whole: bool,
}

/// What the uses section holds: helpers shown, and ones named only because
/// they did not fit.
#[derive(Debug, Default)]
pub struct Uses {
    pub shown: Vec<Use>,
    pub named: Vec<(String, String, usize, usize)>,
    /// Helpers the caller said it already has (`--skip-uses`), by name only.
    pub earlier: Vec<(String, String, usize)>,
    /// The budget the shown helpers were packed into.
    pub cap: usize,
    files: BTreeMap<String, String>,
}

enum Binding {
    /// `import utils from './utils.js'`: `utils.x` and `utils` itself.
    Default(String),
    /// `import * as utils from` / `from . import utils`: only `utils.x`.
    Namespace(String),
    /// `import { a as b } from`: `b` means the target's `a`.
    Named { local: String, original: String },
}

struct Import {
    binding: Binding,
    /// Resolved repository path of the imported file.
    target: String,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Identifiers in `text`, each with whether a `.` follows and the member
/// name after it.
fn identifiers(text: &str) -> Vec<(&str, Option<&str>)> {
    let mut found = Vec::new();
    let mut rest = text;
    let mut previous: Option<char> = None;
    while let Some(start) = rest.find(is_ident_start) {
        let before = rest[..start].chars().next_back().or(previous);
        let after_start = &rest[start..];
        let end = after_start
            .find(|c: char| !is_ident(c))
            .unwrap_or(after_start.len());
        let word = &after_start[..end];
        let tail = &after_start[end..];
        // `a.b`: `b` is a member, not a free identifier.
        let is_member = before == Some('.');
        if !is_member && !word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            let member = tail.strip_prefix('.').and_then(|after| {
                let end = after.find(|c: char| !is_ident(c)).unwrap_or(after.len());
                (end > 0).then(|| &after[..end])
            });
            found.push((word, member));
        }
        previous = word.chars().last();
        rest = tail;
    }
    found
}

/// `a/b/../c` as `a/c`; None when it climbs out of the repository.
fn normalise(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn directory_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

/// The repository file a relative JavaScript/TypeScript import names.
fn resolve_js(from: &str, spec: &str, exists: &mut dyn FnMut(&str) -> bool) -> Option<String> {
    if !spec.starts_with('.') {
        return None;
    }
    let base = normalise(&format!("{}/{spec}", directory_of(from)))?;
    let stem = base
        .strip_suffix(".js")
        .or_else(|| base.strip_suffix(".mjs"))
        .or_else(|| base.strip_suffix(".cjs"))
        .unwrap_or(&base)
        .to_string();
    let mut candidates = vec![base.clone()];
    for extension in [".ts", ".tsx", ".js", ".mjs", ".cjs", ".jsx", ".mts"] {
        candidates.push(format!("{stem}{extension}"));
    }
    for index in ["/index.ts", "/index.js", "/index.tsx", "/index.mjs"] {
        candidates.push(format!("{base}{index}"));
    }
    candidates.into_iter().find(|candidate| exists(candidate))
}

/// The repository file a relative Python import names.
fn resolve_python(
    from: &str,
    module: &str,
    exists: &mut dyn FnMut(&str) -> bool,
) -> Option<String> {
    let dots = module.chars().take_while(|c| *c == '.').count();
    if dots == 0 {
        return None;
    }
    let mut directory = directory_of(from).to_string();
    for _ in 1..dots {
        directory = directory_of(&directory).to_string();
    }
    let rest = module[dots..].replace('.', "/");
    let base = if rest.is_empty() {
        directory
    } else if directory.is_empty() {
        rest
    } else {
        format!("{directory}/{rest}")
    };
    [format!("{base}.py"), format!("{base}/__init__.py")]
        .into_iter()
        .find(|candidate| exists(candidate))
}

fn split_names(list: &str) -> Vec<(String, String)> {
    list.split(',')
        .filter_map(|item| {
            let item = item.trim().trim_start_matches("type ");
            let (original, local) = item
                .split_once(" as ")
                .or_else(|| item.split_once(':'))
                .unwrap_or((item, item));
            let (original, local) = (original.trim(), local.trim());
            let valid = |name: &str| {
                name.chars().next().is_some_and(is_ident_start) && name.chars().all(is_ident)
            };
            (valid(original) && valid(local)).then(|| (local.to_string(), original.to_string()))
        })
        .collect()
}

/// The file's relative imports. Import statements spread over several lines
/// are joined first.
fn imports(
    path: &str,
    lines: &[&str],
    language: &str,
    exists: &mut dyn FnMut(&str) -> bool,
) -> Vec<Import> {
    let mut found = Vec::new();
    let mut statement = String::new();
    for line in lines {
        let trimmed = line.trim();
        if statement.is_empty()
            && !(trimmed.starts_with("import ")
                || trimmed.starts_with("from ")
                || trimmed.starts_with("export ")
                || (trimmed.contains("require(") && trimmed.starts_with(['c', 'l', 'v'])))
        {
            continue;
        }
        statement.push(' ');
        statement.push_str(trimmed);
        // Open braces or parentheses: the statement continues.
        let open = statement.matches(['{', '(']).count() > statement.matches(['}', ')']).count();
        if open && statement.len() < 2_000 {
            continue;
        }
        let text = std::mem::take(&mut statement);
        let text = text.trim();
        if language == "python" {
            found.extend(python_import(path, text, exists));
        } else {
            found.extend(js_import(path, text, exists));
        }
    }
    found
}

fn quoted(text: &str) -> Option<&str> {
    let start = text.find(['\'', '"', '`'])?;
    let quote = text[start..].chars().next()?;
    let rest = &text[start + 1..];
    rest.find(quote).map(|end| &rest[..end])
}

fn js_import(path: &str, text: &str, exists: &mut dyn FnMut(&str) -> bool) -> Vec<Import> {
    let mut found = Vec::new();
    let (clause, spec) = if let Some(rest) = text.strip_prefix("import ") {
        let Some((clause, from)) = rest.rsplit_once(" from ") else {
            return found;
        };
        (clause.trim().to_string(), quoted(from))
    } else if let Some(rest) = text.strip_prefix("export ") {
        // `export { a } from './x'` re-exports: nothing local to resolve.
        let _ = rest;
        return found;
    } else if text.contains("require(") {
        let Some((left, right)) = text.split_once('=') else {
            return found;
        };
        let left = ["const ", "let ", "var "]
            .iter()
            .find_map(|word| left.trim().strip_prefix(word))
            .unwrap_or("")
            .trim();
        (
            left.to_string(),
            right
                .split_once("require(")
                .and_then(|(_, rest)| quoted(rest)),
        )
    } else {
        return found;
    };
    let Some(target) = spec.and_then(|spec| resolve_js(path, spec, exists)) else {
        return found;
    };
    let clause = clause.trim_start_matches("type ").trim();
    let (head, braces) = match clause.find('{') {
        Some(open) => (
            clause[..open].trim().trim_end_matches(',').trim(),
            clause[open + 1..].split('}').next().unwrap_or(""),
        ),
        None => (clause, ""),
    };
    if let Some(namespace) = head.strip_prefix("* as ") {
        found.push(Import {
            binding: Binding::Namespace(namespace.trim().to_string()),
            target: target.clone(),
        });
    } else {
        for name in head
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            if name.chars().all(is_ident) {
                found.push(Import {
                    binding: Binding::Default(name.to_string()),
                    target: target.clone(),
                });
            }
        }
    }
    for (local, original) in split_names(braces) {
        found.push(Import {
            binding: Binding::Named { local, original },
            target: target.clone(),
        });
    }
    found
}

fn python_import(path: &str, text: &str, exists: &mut dyn FnMut(&str) -> bool) -> Vec<Import> {
    let mut found = Vec::new();
    let Some(rest) = text.strip_prefix("from ") else {
        return found;
    };
    let Some((module, names)) = rest.split_once(" import ") else {
        return found;
    };
    let names = names.trim().trim_start_matches('(').trim_end_matches(')');
    let module = module.trim();
    if module.chars().all(|c| c == '.') {
        // `from . import utils`: each name is a module.
        for (local, original) in split_names(names) {
            if let Some(target) = resolve_python(path, &format!("{module}{original}"), exists) {
                found.push(Import {
                    binding: Binding::Namespace(local),
                    target,
                });
            }
        }
        return found;
    }
    let Some(target) = resolve_python(path, module, exists) else {
        return found;
    };
    for (local, original) in split_names(names) {
        found.push(Import {
            binding: Binding::Named { local, original },
            target: target.clone(),
        });
    }
    found
}

/// The name a JavaScript module's default export carries, if it is one
/// definition rather than an object literal.
fn default_export(lines: &[&str]) -> Option<String> {
    lines.iter().rev().find_map(|line| {
        let trimmed = line.trim();
        let rest = trimmed
            .strip_prefix("export default ")
            .or_else(|| trimmed.strip_prefix("module.exports = "))?;
        let rest = ["class ", "async function ", "function* ", "function "]
            .iter()
            .find_map(|word| rest.strip_prefix(word))
            .unwrap_or(rest);
        let end = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
        (end > 0).then(|| rest[..end].to_string())
    })
}

/// Top-level definitions in a file, by name: what a reference into it can
/// resolve to without searching.
fn top_level(lines: &[&str], language: &str) -> HashSet<String> {
    outline(lines, language, false)
        .into_iter()
        .filter(|number| {
            let line = lines[number - 1];
            !line.starts_with([' ', '\t'])
        })
        .filter_map(|number| declared_name(lines[number - 1]).map(str::to_string))
        .collect()
}

/// Where `name` is defined in a file: its definition, or the definition an
/// object-literal alias points at (`hasOwnProp: hasOwnProperty`).
fn locate(lines: &[&str], name: &str, language: &str) -> Option<(usize, usize, String)> {
    if let Some(body) = definitions(lines, name, language).into_iter().next() {
        return Some((body.start, body.end, name.to_string()));
    }
    let alias = regex::Regex::new(&format!(
        r"(?:^|[\s,{{]){}\s*:\s*([A-Za-z_$][\w$]*)",
        regex::escape(name)
    ))
    .ok()?;
    lines.iter().find_map(|line| {
        let target = alias.captures(line)?.get(1)?.as_str();
        let body = definitions(lines, target, language).into_iter().next()?;
        Some((body.start, body.end, target.to_string()))
    })
}

/// The helpers the lines in `shown` use, from this file and the files it
/// imports, followed up to two hops, within [`USES_CAP_CHARS`].
pub fn uses(
    path: &str,
    text: &str,
    shown: &[(usize, usize)],
    skip: &HashSet<(String, usize)>,
    cap: usize,
    read: &mut dyn FnMut(&str) -> Option<String>,
) -> Uses {
    let language = crate::filters::language_of(path).unwrap_or("");
    let mut result = Uses {
        cap,
        ..Uses::default()
    };
    // A test's imports are the code under test, which the reader is
    // already studying; adding it to every test read repeated the library.
    if shown.is_empty() || crate::outline::is_test_path(path) {
        return result;
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut cache: BTreeMap<String, Option<String>> = BTreeMap::new();
    let imported = {
        let mut exists = |candidate: &str| -> bool {
            cache
                .entry(candidate.to_string())
                .or_insert_with(|| read(candidate))
                .is_some()
        };
        if matches!(language, "javascript" | "typescript" | "python") {
            imports(path, &lines, language, &mut exists)
        } else {
            Vec::new()
        }
    };

    // (file, name, depth), in the order the code mentions them.
    let mut queue: Vec<(String, String, usize)> = Vec::new();
    let own_names = top_level(&lines, language);
    let shown_text: String = shown
        .iter()
        .flat_map(|&(start, end)| lines[start - 1..end].iter())
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let wanted =
        |file: &str, name: &str, depth: usize, queue: &mut Vec<(String, String, usize)>| {
            if !queue.iter().any(|(f, n, _)| f == file && n == name) {
                queue.push((file.to_string(), name.to_string(), depth));
            }
        };
    for (word, member) in identifiers(&shown_text) {
        for import in &imported {
            match (&import.binding, member) {
                (Binding::Default(local) | Binding::Namespace(local), Some(member))
                    if local == word =>
                {
                    wanted(&import.target, member, 1, &mut queue);
                }
                (Binding::Default(local), None) if local == word => {
                    let source = cache
                        .get(&import.target)
                        .cloned()
                        .flatten()
                        .unwrap_or_default();
                    let source_lines: Vec<&str> = source.lines().collect();
                    if let Some(name) = default_export(&source_lines) {
                        wanted(&import.target, &name, 1, &mut queue);
                    }
                }
                (Binding::Named { local, original }, _) if local == word => {
                    wanted(&import.target, original, 1, &mut queue);
                }
                _ => {}
            }
        }
        if own_names.contains(word) {
            wanted(path, word, 1, &mut queue);
        }
    }

    // Resolve everything first, then pack: the order the code names them in
    // put one-line type checks ahead of the helpers that carry the logic.
    struct Candidate {
        file: String,
        name: String,
        start: usize,
        end: usize,
        depth: usize,
        lines: Vec<usize>,
        whole: bool,
        cost: usize,
    }
    let mut seen: HashSet<(String, usize)> = HashSet::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut index = 0;
    while index < queue.len() && candidates.len() < MAX_CANDIDATES {
        let (file, name, depth) = queue[index].clone();
        index += 1;
        let source = if file == path {
            Some(text.to_string())
        } else {
            cache
                .entry(file.clone())
                .or_insert_with(|| read(&file))
                .clone()
        };
        let Some(source) = source else {
            continue;
        };
        let file_lines: Vec<&str> = source.lines().collect();
        let file_language = crate::filters::language_of(&file).unwrap_or(language);
        let Some((start, end, defined)) = locate(&file_lines, &name, file_language) else {
            continue;
        };
        // Already on screen, or already found under another name.
        let overlaps_shown = file == path && shown.iter().any(|&(a, b)| start <= b && a <= end);
        if overlaps_shown || !seen.insert((file.clone(), start)) {
            continue;
        }
        if skip.contains(&(file.clone(), start)) {
            result.earlier.push((file, defined, start));
            continue;
        }
        // Comments and blank lines go: these are reference, and the code
        // says what the reader needs; the numbering shows the gaps.
        let hidden = comment_lines(&file_lines, file_language);
        let code: Vec<usize> = (start..=end)
            .filter(|number| !hidden.contains(number) && !file_lines[number - 1].trim().is_empty())
            .collect();
        let whole = code.len() <= WHOLE_BODY_LINES;
        let printed: Vec<usize> = if whole {
            code
        } else {
            let mut members: Vec<usize> = outline(&file_lines, file_language, false)
                .into_iter()
                .filter(|number| (start..=end).contains(number))
                .take(OUTLINE_LINES)
                .collect();
            if members.first() != Some(&start) {
                members.insert(0, start);
            }
            members
        };
        let cost = file.len()
            + defined.len()
            + 20
            + printed
                .iter()
                .map(|number| numbered(*number, file_lines[number - 1]).len() + 1)
                .sum::<usize>();
        // The next hop: same-file helpers this body calls.
        if depth < MAX_DEPTH && whole {
            let names = top_level(&file_lines, file_language);
            let body = file_lines[start - 1..end].join("\n");
            for (word, _) in identifiers(&body) {
                if word != defined && names.contains(word) {
                    wanted(&file, word, depth + 1, &mut queue);
                }
            }
        }
        if file != path {
            result.files.insert(file.clone(), source.clone());
        }
        candidates.push(Candidate {
            file,
            name: defined,
            start,
            end,
            depth,
            lines: printed,
            whole,
            cost,
        });
    }
    // Helpers with logic first, then one-line ones (`isDate = typeOfTest(..)`
    // says what it does in its name), each group nearest hop first.
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&i| (candidates[i].lines.len() <= 2, candidates[i].depth, i));
    let mut used = 0usize;
    let mut chosen: Vec<usize> = Vec::new();
    for i in order {
        let candidate = &candidates[i];
        // An outline that is only the first line says nothing a name doesn't.
        let informative = candidate.whole || candidate.lines.len() > 1;
        if informative && used + candidate.cost <= cap {
            used += candidate.cost;
            chosen.push(i);
        } else {
            result.named.push((
                candidate.file.clone(),
                candidate.name.clone(),
                candidate.start,
                candidate.end,
            ));
        }
    }
    // Printed in the order found, which follows the code.
    chosen.sort_unstable();
    for i in chosen {
        let candidate = &candidates[i];
        result.shown.push(Use {
            path: candidate.file.clone(),
            name: candidate.name.clone(),
            start: candidate.start,
            end: candidate.end,
            lines: candidate.lines.clone(),
            whole: candidate.whole,
        });
    }
    result.files.insert(path.to_string(), text.to_string());
    result
}

impl Uses {
    pub fn is_empty(&self) -> bool {
        self.shown.is_empty() && self.named.is_empty() && self.earlier.is_empty()
    }

    /// The section appended to a text read.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n# uses: same-repo helpers the code above calls, so you need not look them up\n",
        );
        for helper in &self.shown {
            let source = self.files.get(&helper.path).map_or("", String::as_str);
            let lines: Vec<&str> = source.lines().collect();
            out.push_str(&format!(
                "\n## {} {} {}-{}{}\n",
                helper.path,
                helper.name,
                helper.start,
                helper.end,
                if helper.whole {
                    ""
                } else {
                    " (its outline; ask for it by name for the body)"
                }
            ));
            for number in &helper.lines {
                if let Some(line) = lines.get(number - 1) {
                    out.push_str(&numbered(*number, line));
                    out.push('\n');
                }
            }
        }
        if !self.earlier.is_empty() {
            let names: Vec<String> = self
                .earlier
                .iter()
                .map(|(path, name, start)| format!("{name} ({path}:{start})"))
                .collect();
            out.push_str(&format!(
                "\n# also uses, shown earlier: {}\n",
                names.join(", ")
            ));
        }
        if !self.named.is_empty() {
            let names: Vec<String> = self
                .named
                .iter()
                .map(|(path, name, start, end)| format!("{name} ({path}:{start}-{end})"))
                .collect();
            out.push_str(&format!(
                "\n# also uses, past the {}-character cap: {}\n",
                self.cap,
                names.join(", ")
            ));
        }
        out
    }

    /// The same, for `--json`.
    pub fn to_json(&self) -> serde_json::Value {
        let shown: Vec<serde_json::Value> = self
            .shown
            .iter()
            .map(|helper| {
                let source = self.files.get(&helper.path).map_or("", String::as_str);
                let lines: Vec<&str> = source.lines().collect();
                serde_json::json!({
                    "path": helper.path,
                    "name": helper.name,
                    "from": helper.start,
                    "to": helper.end,
                    "whole": helper.whole,
                    "lines": helper
                        .lines
                        .iter()
                        .filter_map(|number| lines.get(number - 1).map(|line| serde_json::json!([number, line])))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let named: Vec<serde_json::Value> = self
            .named
            .iter()
            .map(|(path, name, start, end)| serde_json::json!({"path": path, "name": name, "from": start, "to": end}))
            .collect();
        let earlier: Vec<serde_json::Value> = self
            .earlier
            .iter()
            .map(|(path, name, start)| serde_json::json!({"path": path, "name": name, "from": start}))
            .collect();
        serde_json::json!({ "shown": shown, "not_shown": named, "shown_earlier": earlier })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> BTreeMap<&'static str, &'static str> {
        BTreeMap::from([
            (
                "lib/helpers/toFormData.js",
                "import utils from '../utils.js';\nimport AxiosError from '../core/AxiosError.js';\nimport { DEFAULT as LIMIT } from './limits.js';\n\nfunction renderKey(path, key) {\n  return key;\n}\n\nfunction toFormData(obj) {\n  if (!utils.isObject(obj)) {\n    throw new AxiosError('bad', AxiosError.ERR_BAD);\n  }\n  utils.forEach(obj, (value, key) => renderKey(null, key));\n  return LIMIT;\n}\n\nexport default toFormData;\n",
            ),
            (
                "lib/utils.js",
                "const isObject = (thing) => thing !== null && typeof thing === 'object';\n\nfunction isHole(list, index) {\n  return !(index in list);\n}\n\nfunction forEach(obj, fn) {\n  for (const key in obj) {\n    if (isHole(obj, key)) continue;\n    fn(obj[key], key);\n  }\n}\n\nexport default { isObject, forEach, hasOwnProp: isHole };\n",
            ),
            (
                "lib/core/AxiosError.js",
                "class AxiosError extends Error {\n  constructor(message, code) {\n    super(message);\n    this.code = code;\n  }\n}\n\nAxiosError.ERR_BAD = 'ERR_BAD';\n\nexport default AxiosError;\n",
            ),
            ("lib/helpers/limits.js", "export const DEFAULT = 100;\n"),
        ])
    }

    fn run(path: &str, shown: &[(usize, usize)]) -> Uses {
        let files = repo();
        let text = files[path];
        uses(
            path,
            text,
            shown,
            &HashSet::new(),
            USES_CAP_CHARS,
            &mut |candidate| files.get(candidate).map(|s| s.to_string()),
        )
    }

    #[test]
    fn follows_imports_members_named_and_same_file_helpers() {
        let found = run("lib/helpers/toFormData.js", &[(9, 15)]);
        let names: Vec<(&str, &str)> = found
            .shown
            .iter()
            .map(|helper| (helper.path.as_str(), helper.name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("lib/utils.js", "isObject"),
                ("lib/core/AxiosError.js", "AxiosError"),
                ("lib/utils.js", "forEach"),
                ("lib/helpers/toFormData.js", "renderKey"),
                ("lib/helpers/limits.js", "DEFAULT"),
                // A second hop: forEach calls isHole in its own file.
                ("lib/utils.js", "isHole"),
            ]
        );
        let text = found.render();
        assert!(
            text.contains("## lib/utils.js forEach 7-12\n7\tfunction forEach(obj, fn) {"),
            "{text}"
        );
        assert!(
            !text.contains("toFormData(obj)"),
            "repeated the shown code: {text}"
        );
    }

    #[test]
    fn helpers_already_shown_are_named_not_repeated() {
        let files = repo();
        let path = "lib/helpers/toFormData.js";
        let skip = HashSet::from([("lib/utils.js".to_string(), 7usize)]);
        let found = uses(
            path,
            files[path],
            &[(9, 15)],
            &skip,
            USES_CAP_CHARS,
            &mut |candidate| files.get(candidate).map(|s| s.to_string()),
        );
        assert!(found.shown.iter().all(|helper| helper.name != "forEach"));
        assert!(
            found
                .render()
                .contains("# also uses, shown earlier: forEach (lib/utils.js:7)")
        );
        // Not followed through either: isHole came only via forEach.
        assert!(found.shown.iter().all(|helper| helper.name != "isHole"));
    }

    #[test]
    fn uses_budget_follows_the_size_of_the_read() {
        assert_eq!(uses_cap(300), USES_FLOOR_CHARS);
        assert_eq!(uses_cap(3_000), 3_000);
        assert_eq!(uses_cap(40_000), USES_CAP_CHARS);
    }

    #[test]
    fn test_files_get_no_uses() {
        let files = BTreeMap::from([(
            "tests/unit/a.test.js",
            "import utils from '../../lib/utils.js';\nit('x', () => utils.forEach([], () => {}));\n",
        )]);
        let found = uses(
            "tests/unit/a.test.js",
            files["tests/unit/a.test.js"],
            &[(1, 2)],
            &HashSet::new(),
            USES_CAP_CHARS,
            &mut |_| Some("function forEach() {}\n".to_string()),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn object_literal_aliases_resolve_to_their_definition() {
        let files = repo();
        let lines: Vec<&str> = files["lib/utils.js"].lines().collect();
        assert_eq!(
            locate(&lines, "hasOwnProp", "javascript").map(|(s, _, n)| (s, n)),
            Some((3, "isHole".to_string()))
        );
    }

    #[test]
    fn nothing_for_unresolvable_or_external_imports() {
        let files = BTreeMap::from([(
            "src/a.js",
            "import lodash from 'lodash';\nimport x from './missing.js';\nfunction go() {\n  return lodash.map(x.y);\n}\n",
        )]);
        let found = uses(
            "src/a.js",
            files["src/a.js"],
            &[(3, 5)],
            &HashSet::new(),
            USES_CAP_CHARS,
            &mut |candidate| files.get(candidate).map(|s| s.to_string()),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn python_relative_imports_resolve() {
        let files = BTreeMap::from([
            (
                "pkg/api.py",
                "from .sessions import Session as S\nfrom . import utils\n\ndef get(url):\n    return S().request(utils.clean(url))\n",
            ),
            (
                "pkg/sessions.py",
                "class Session:\n    def request(self, url):\n        return url\n",
            ),
            ("pkg/utils.py", "def clean(url):\n    return url.strip()\n"),
        ]);
        let found = uses(
            "pkg/api.py",
            files["pkg/api.py"],
            &[(4, 5)],
            &HashSet::new(),
            USES_CAP_CHARS,
            &mut |candidate| files.get(candidate).map(|s| s.to_string()),
        );
        let names: Vec<&str> = found
            .shown
            .iter()
            .map(|helper| helper.name.as_str())
            .collect();
        assert_eq!(names, vec!["Session", "clean"]);
    }

    #[test]
    fn default_export_is_found_above_trailing_lines() {
        let lines = ["class A {}", "export default A;", "", "// trailing note"];
        assert_eq!(default_export(&lines).as_deref(), Some("A"));
        assert_eq!(default_export(&["export default { a, b };"]), None);
    }

    #[test]
    fn identifiers_see_members_and_skip_member_names() {
        assert_eq!(
            identifiers("utils.forEach(obj, fn); a.b.c"),
            vec![
                ("utils", Some("forEach")),
                ("obj", None),
                ("fn", None),
                ("a", Some("b"))
            ]
        );
    }
}
