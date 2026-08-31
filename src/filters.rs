//! What counts as a useful code sample. Everything else is dropped at ingest.

/// Only these extensions are indexed. Deliberately code-only: no docs, no data.
const CODE_EXTENSIONS: &[(&str, &str)] = &[
    ("py", "python"),
    ("pyi", "python"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("jsx", "javascript"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("go", "go"),
    ("rs", "rust"),
    ("java", "java"),
    ("kt", "kotlin"),
    ("scala", "scala"),
    ("c", "c"),
    ("h", "c"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("hpp", "cpp"),
    ("hh", "cpp"),
    ("cs", "csharp"),
    ("rb", "ruby"),
    ("php", "php"),
    ("swift", "swift"),
    ("sh", "shell"),
    ("bash", "shell"),
    ("sql", "sql"),
    ("ex", "elixir"),
    ("exs", "elixir"),
    ("lua", "lua"),
    ("zig", "zig"),
];

/// Directory names that never contain original work worth learning from.
/// `examples/` is deliberately absent: worked examples are some of the most
/// useful material for an agent learning how a library is meant to be used.
const SKIP_DIRECTORIES: &[&str] = &[
    "node_modules",
    "vendor",
    "third_party",
    "thirdparty",
    "dist",
    "build",
    "target",
    "out",
    ".git",
    ".github",
    "testdata",
    "fixtures",
    "__pycache__",
    "site-packages",
    "bower_components",
    "docs",
    "doc",
    "generated",
    "gen",
    ".venv",
    "venv",
    "migrations",
];

/// Substrings marking generated, minified or lock files.
const SKIP_NAME_MARKERS: &[&str] = &[
    ".min.",
    ".bundle.",
    "_pb2.",
    ".pb.",
    "_generated.",
    ".generated.",
    ".g.dart",
    "-lock.",
    ".lock.",
];

const TEST_MARKERS: &[&str] = &["test_", "_test.", ".test.", ".spec.", "conftest."];

/// Files above this are almost always generated, vendored or data blobs.
pub const MAX_FILE_BYTES: u64 = 200 * 1024;
/// Below this there is nothing to learn.
pub const MIN_FILE_BYTES: u64 = 64;

/// The language for a repo-relative path, or None if it is not code.
pub fn language_of(path: &str) -> Option<&'static str> {
    let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
    CODE_EXTENSIONS
        .iter()
        .find(|(candidate, _)| *candidate == extension)
        .map(|(_, language)| *language)
}

/// Whether a repo-relative path earns disk space.
pub fn should_index(path: &str, size: u64, include_tests: bool) -> bool {
    if !(MIN_FILE_BYTES..=MAX_FILE_BYTES).contains(&size) {
        return false;
    }
    if language_of(path).is_none() {
        return false;
    }

    let parts: Vec<&str> = path.split('/').collect();
    let (name, directories) = match parts.split_last() {
        Some(split) => split,
        None => return false,
    };
    if directories
        .iter()
        .any(|part| SKIP_DIRECTORIES.contains(part))
    {
        return false;
    }
    if parts.iter().any(|part| part.starts_with('.')) {
        return false;
    }

    let lowered = name.to_ascii_lowercase();
    if SKIP_NAME_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        return false;
    }
    if !include_tests {
        if TEST_MARKERS.iter().any(|marker| lowered.contains(marker)) {
            return false;
        }
        if directories
            .iter()
            .any(|part| *part == "tests" || *part == "test")
        {
            return false;
        }
    }
    true
}

/// A NUL byte in the first block is the standard binary heuristic.
pub fn looks_binary(chunk: &[u8]) -> bool {
    let head = &chunk[..chunk.len().min(8192)];
    memchr::memchr(0, head).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_source_and_drops_noise() {
        assert!(should_index("src/agent.py", 1000, false));
        assert!(should_index("examples/basic/retry.py", 1000, false));
        assert!(!should_index("README.md", 1000, false));
        assert!(!should_index("node_modules/x/index.js", 1000, false));
        assert!(!should_index("src/app.min.js", 1000, false));
        assert!(!should_index("tests/test_agent.py", 1000, false));
        assert!(should_index("tests/test_agent.py", 1000, true));
        assert!(!should_index("src/agent.py", 10, false), "too small");
        assert!(!should_index("src/agent.py", 999_999, false), "too large");
    }

    #[test]
    fn detects_languages() {
        assert_eq!(language_of("a/b.rs"), Some("rust"));
        assert_eq!(language_of("a/b.PY"), Some("python"));
        assert_eq!(language_of("Makefile"), None);
    }
}
