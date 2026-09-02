//! What the filters let through.
//!
//! The filters are heuristics, and a heuristic is only as good as the last
//! convention someone noticed. `steroids audit` turns "look at kotlin and see
//! what is left" into one report over every stored path: the directory and
//! file-name words that still smell of tests, fixtures or generated code, how
//! unevenly the corpus is spread across repositories, and anything empty.
//! Drive the numbers down, then lock the result into the path fixtures under
//! `tests/fixtures/`.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;

use crate::store::Store;

/// Words that mark a directory or file as something the filters usually
/// drop, matched as whole words after splitting on `_`, `-`, `.` and
/// camelCase so `workbench` and `inspector` do not count. `example` is
/// absent on purpose: worked examples are kept by design.
const SUSPECT_WORDS: &[&str] = &[
    "test",
    "tests",
    "testing",
    "testdata",
    "testutil",
    "testutils",
    "spec",
    "specs",
    "mock",
    "mocks",
    "fixture",
    "fixtures",
    "sample",
    "samples",
    "demo",
    "demos",
    "generated",
    "gen",
    "vendor",
    "vendors",
    "vendored",
    "snapshot",
    "snapshots",
    "bench",
    "benches",
    "benchmark",
    "benchmarks",
    "stub",
    "stubs",
    "fake",
    "fakes",
    "dummy",
    "e2e",
    "dist",
];

/// Lower-cased words of a path segment: `SandboxFilesDist` gives
/// `sandbox`, `files`, `dist`; `test-helpers` gives `test`, `helpers`.
fn words(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in segment.chars() {
        let boundary = matches!(c, '_' | '-' | '.') || (c.is_uppercase() && previous_lower);
        if boundary && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if !matches!(c, '_' | '-' | '.') {
            current.extend(c.to_lowercase());
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn suspect_word(segment: &str) -> Option<&'static str> {
    words(segment)
        .into_iter()
        .find_map(|w| SUSPECT_WORDS.iter().copied().find(|s| *s == w))
}

/// One suspicious name and where it turns up.
#[derive(Serialize)]
pub struct Suspect {
    pub name: String,
    pub files: usize,
    pub repos: usize,
    pub example: String,
}

#[derive(Serialize)]
pub struct Report {
    pub repositories: usize,
    pub documents: usize,
    pub empty_repositories: Vec<String>,
    /// (repo, files), largest first.
    pub largest: Vec<(String, i64)>,
    pub median_files: i64,
    /// Directory segments that look like test or generated trees.
    pub suspect_directories: Vec<Suspect>,
    /// File names that look like tests or generated code, grouped by word.
    pub suspect_files: Vec<Suspect>,
}

pub fn run(store: &Store) -> Result<Report> {
    let repos = store.list_repos()?;
    let mut sizes: Vec<i64> = repos.iter().map(|r| r.files).collect();
    sizes.sort_unstable();
    let median_files = sizes.get(sizes.len() / 2).copied().unwrap_or(0);
    let mut largest: Vec<(String, i64)> = repos.iter().map(|r| (r.name.clone(), r.files)).collect();
    largest.sort_by_key(|(_, files)| std::cmp::Reverse(*files));
    largest.truncate(10);
    let empty_repositories = repos
        .iter()
        .filter(|r| r.files == 0)
        .map(|r| r.name.clone())
        .collect();

    let paths = store.every_path()?;
    let mut directories: BTreeMap<String, Tally> = BTreeMap::new();
    let mut files: BTreeMap<String, Tally> = BTreeMap::new();
    for (repo, path) in &paths {
        let Some((dirs, name)) = path.rsplit_once('/') else {
            tally_name(&mut files, repo, path, name_word(path));
            continue;
        };
        for segment in dirs.split('/') {
            if suspect_word(segment).is_some() {
                directories
                    .entry(segment.to_ascii_lowercase())
                    .or_default()
                    .add(repo, path);
            }
        }
        tally_name(&mut files, repo, path, name_word(name));
    }

    Ok(Report {
        repositories: repos.len(),
        documents: paths.len(),
        empty_repositories,
        largest,
        median_files,
        suspect_directories: ranked(directories),
        suspect_files: ranked(files),
    })
}

#[derive(Default)]
struct Tally {
    files: usize,
    repos: std::collections::BTreeSet<String>,
    example: String,
}

impl Tally {
    fn add(&mut self, repo: &str, path: &str) {
        self.files += 1;
        if self.repos.insert(repo.to_string()) && self.example.is_empty() {
            self.example = format!("{repo}/{path}");
        }
    }
}

/// The suspect word in a file name's stem, so `run_demo.sh` counts under
/// `demo` and `vitest.config.ts` under nothing.
fn name_word(name: &str) -> Option<&'static str> {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    suspect_word(stem)
}

fn tally_name(files: &mut BTreeMap<String, Tally>, repo: &str, path: &str, word: Option<&str>) {
    if let Some(word) = word {
        files.entry(word.to_string()).or_default().add(repo, path);
    }
}

/// Most files first, capped so the report stays readable.
fn ranked(tallies: BTreeMap<String, Tally>) -> Vec<Suspect> {
    let mut out: Vec<Suspect> = tallies
        .into_iter()
        .map(|(name, t)| Suspect {
            name,
            files: t.files,
            repos: t.repos.len(),
            example: t.example,
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.files));
    out.truncate(30);
    out
}

pub fn render(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  repositories : {}   documents : {}   median files/repo : {}\n",
        report.repositories, report.documents, report.median_files
    ));
    if !report.empty_repositories.is_empty() {
        out.push_str(&format!(
            "  empty ({}): {}   (steroids index --refilter removes them)\n",
            report.empty_repositories.len(),
            report.empty_repositories.join(", ")
        ));
    }
    out.push_str("\n  largest repositories\n");
    for (name, files) in &report.largest {
        out.push_str(&format!("    {files:>7}  {name}\n"));
    }
    for (title, list) in [
        (
            "directory names that look like tests or generated code",
            &report.suspect_directories,
        ),
        (
            "file-name words that look like tests or generated code",
            &report.suspect_files,
        ),
    ] {
        out.push_str(&format!("\n  {title}\n"));
        if list.is_empty() {
            out.push_str("    none\n");
        }
        for s in list {
            out.push_str(&format!(
                "    {:>7} files  {:>4} repos  {:<28} e.g. {}\n",
                s.files, s.repos, s.name, s.example
            ));
        }
    }
    out.push_str(
        "\n  Anything above that is noise wants a rule in src/filters.rs, then \
         `steroids index --refilter`. Words like `latest` also contain `test`: \
         read the example before adding one.\n",
    );
    out
}
