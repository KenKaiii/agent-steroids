//! Syntax colouring for the code panes.
//!
//! syntect does the parsing; the colours are ours. Its themes carry RGB values
//! tuned for one background, so a dark theme on a light terminal is unreadable.
//! Mapping scopes to named colours instead keeps the rule in `ui.rs`: the
//! terminal's palette decides what the colours look like.

use ratatui::prelude::*;
use syntect::easy::ScopeRangeIterator;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use crate::filters::language_of;

/// Innermost scope wins, so a keyword inside a comment stays comment-coloured.
/// Ordered by how specific the scope is; `Scope::is_prefix_of` does the rest.
const PALETTE: &[(&str, Color)] = &[
    ("comment", Color::DarkGray),
    ("string", Color::Green),
    ("constant.numeric", Color::Yellow),
    ("constant.language", Color::Yellow),
    ("constant.character", Color::Yellow),
    ("entity.name.function", Color::Blue),
    ("entity.name.type", Color::Cyan),
    ("entity.name.class", Color::Cyan),
    ("entity.name.struct", Color::Cyan),
    ("entity.name.enum", Color::Cyan),
    ("support.type", Color::Cyan),
    ("support.class", Color::Cyan),
    ("support.function", Color::Blue),
    ("storage.type", Color::Magenta),
    ("storage.modifier", Color::Magenta),
    ("keyword", Color::Magenta),
];

pub struct Highlighter {
    syntaxes: SyntaxSet,
    palette: Vec<(Scope, Color)>,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self {
            // bat's pack on top of syntect's: TypeScript, Kotlin, Swift, Elixir
            // and Zig are all missing from the defaults.
            syntaxes: two_face::syntax::extra_no_newlines(),
            palette: PALETTE
                .iter()
                .filter_map(|(selector, colour)| Some((Scope::new(selector).ok()?, *colour)))
                .collect(),
        }
    }
}

impl Highlighter {
    /// Colour each line of `path` and return spans that own their text.
    ///
    /// Parsing runs across the whole file, not per visible line: a block comment
    /// opened above the viewport must still colour the lines inside it. Files
    /// whose extension the syntax pack does not know come back uncoloured.
    pub fn lines(&self, path: &str, lines: &[String]) -> Vec<Line<'static>> {
        let Some(syntax) = self.syntax_for(path) else {
            return lines.iter().map(|line| Line::raw(line.clone())).collect();
        };
        let mut state = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        lines
            .iter()
            .map(|line| {
                // A parse error mid-file is a syntax-pack quirk, not a reason to
                // hide the code: fall back to plain text for that line.
                let Ok(ops) = state.parse_line(line, &self.syntaxes) else {
                    return Line::raw(line.clone());
                };
                let mut spans = Vec::new();
                for (range, op) in ScopeRangeIterator::new(&ops, line) {
                    if stack.apply(op).is_err() {
                        return Line::raw(line.clone());
                    }
                    if range.is_empty() {
                        continue;
                    }
                    spans.push(Span::styled(
                        line[range].to_string(),
                        Style::default().fg(self.colour(&stack)),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }

    /// By extension first; failing that, by the language `filters.rs` assigns
    /// (`.jsx` has no syntax of its own but is JavaScript).
    fn syntax_for(&self, path: &str) -> Option<&SyntaxReference> {
        let extension = path.rsplit_once('.').map_or("", |(_, ext)| ext);
        self.syntaxes
            .find_syntax_by_extension(extension)
            .or_else(|| self.syntaxes.find_syntax_by_token(language_of(path)?))
    }

    fn colour(&self, stack: &ScopeStack) -> Color {
        stack
            .as_slice()
            .iter()
            .rev()
            .find_map(|scope| {
                self.palette
                    .iter()
                    .find(|(prefix, _)| prefix.is_prefix_of(*scope))
                    .map(|(_, colour)| *colour)
            })
            .unwrap_or(Color::Reset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::CODE_EXTENSIONS;

    #[test]
    fn colours_comments_and_keywords() {
        let highlighter = Highlighter::default();
        let lines = vec![
            "fn main() { // hi".to_string(),
            "let x = \"s\";".to_string(),
        ];
        let out = highlighter.lines("src/main.rs", &lines);
        let colours = |line: &Line| -> Vec<(String, Color)> {
            line.spans
                .iter()
                .map(|s| (s.content.to_string(), s.style.fg.unwrap_or(Color::Reset)))
                .collect()
        };
        let first = colours(&out[0]);
        assert!(
            first.iter().any(|(t, c)| t == "fn" && *c == Color::Magenta),
            "{first:?}"
        );
        assert!(
            first
                .iter()
                .any(|(t, c)| t.contains("hi") && *c == Color::DarkGray),
            "{first:?}"
        );
        let second = colours(&out[1]);
        assert!(
            second.iter().any(|(t, c)| t == "\"" && *c == Color::Green),
            "{second:?}"
        );
        // Every byte of the input survives, in order.
        let joined: String = out[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, lines[1]);
    }

    /// Debug builds are ~10x slower here; the budget is for the binary users run.
    #[test]
    #[cfg(not(debug_assertions))]
    fn a_big_file_colours_quickly() {
        let highlighter = Highlighter::default();
        let lines: Vec<String> = (0..5000)
            .map(|i| format!("    let value_{i} = compute(\"{i}\", {i}); // note"))
            .collect();
        let started = std::time::Instant::now();
        let out = highlighter.lines("big.rs", &lines);
        let elapsed = started.elapsed();
        assert_eq!(out.len(), 5000);
        assert!(elapsed.as_millis() < 1500, "took {elapsed:?}");
    }

    #[test]
    fn unknown_extension_is_plain() {
        let out = Highlighter::default().lines("a.unknownext", &["x".to_string()]);
        assert_eq!(out[0].spans.len(), 1);
        assert_eq!(out[0].spans[0].style, Style::default());
    }

    #[test]
    fn every_indexed_language_has_a_syntax() {
        let highlighter = Highlighter::default();
        let missing: Vec<&str> = CODE_EXTENSIONS
            .iter()
            .map(|(ext, _)| *ext)
            .filter(|ext| highlighter.syntax_for(&format!("a.{ext}")).is_none())
            .collect();
        assert!(missing.is_empty(), "no syntax for {missing:?}");
    }
}
