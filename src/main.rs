//! A local code corpus for coding agents.
//!
//! One binary, no runtime, no daemon: an agent calls `steroids search` the
//! same way it calls `grep`.

mod audit;
mod bulk;
mod config;
mod discover;
mod fetch;
mod filters;
mod index;
mod recent;
mod render;
mod search;
mod store;
mod tui;
mod upgrade;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use render::render_empty;
use search::Query;
use store::Store;

#[derive(Parser)]
#[command(
    name = "steroids",
    about = "A local corpus of open-source code, searchable by you or your coding agent",
    version
)]
struct Cli {
    /// Corpus location. Defaults to $STEROIDS_ROOT, else ~/.steroids
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// Concurrent downloads when ingesting. Ingest is network-bound, so this
    /// is the main lever on how fast a bulk add finishes.
    #[arg(long, global = true, default_value_t = bulk::DEFAULT_PARALLEL)]
    parallel: usize,
    /// Omit to open the interactive browser.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Ingest GitHub repositories (owner/name)
    Add {
        repos: Vec<String>,
        /// Also read repository names from a file, one per line
        #[arg(long)]
        from_file: Option<PathBuf>,
        /// Keep test files, which are excluded by default
        #[arg(long)]
        include_tests: bool,
        /// Label these repositories, e.g. --tag coding-agent
        #[arg(long, value_delimiter = ',')]
        tag: Vec<String>,
    },
    /// Bring the trigram index up to date. Runs by itself after every add, update and remove; use --rebuild to start it over.
    Index {
        /// Discard the existing index and rebuild from scratch. Only needed if
        /// the index is suspect; ordinary runs extend it in place.
        #[arg(long)]
        rebuild: bool,
        /// Drop stored files the current filters would no longer keep (tests,
        /// samples, generated code). No network; `update` skips unchanged
        /// repositories and so never re-filters them.
        #[arg(long)]
        refilter: bool,
        /// With --refilter: keep test files, as `add --include-tests` did.
        #[arg(long, requires = "refilter")]
        include_tests: bool,
    },
    /// Re-fetch every indexed repository at its latest commit
    Update,
    /// Report what the filters let through: test-like names still indexed,
    /// lopsided repositories, empty ones
    Audit {
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Regex search across every indexed repository
    Search {
        pattern: String,
        /// Only these repositories. Repeat the flag or separate with commas;
        /// any form `add` accepts works, case does not matter.
        #[arg(long, value_delimiter = ',')]
        repo: Vec<String>,
        /// Only repositories carrying this label
        #[arg(long)]
        tag: Option<String>,
        /// Language name or alias, e.g. typescript, ts, c++, golang
        #[arg(long)]
        language: Option<String>,
        /// Path glob, e.g. 'src/**/*.py'. A bare prefix like 'src' means 'src/**'
        #[arg(long)]
        path: Option<String>,
        #[arg(short, long)]
        ignore_case: bool,
        /// Match the pattern literally rather than as a regex
        #[arg(short = 'F', long)]
        fixed_strings: bool,
        /// Include hits inside comments and docstrings
        #[arg(long)]
        include_comments: bool,
        /// Lines of code shown either side of a match. Raise it when comparing
        /// how several projects implement the same thing.
        #[arg(short = 'C', long, default_value_t = search::DEFAULT_CONTEXT_LINES)]
        context: usize,
        /// Most results to take from any one repository. Use 1 to see how many
        /// different projects solve something, rather than one project many
        /// times.
        #[arg(long)]
        per_repo: Option<usize>,
        /// Stop adding results once the output would exceed this many tokens.
        /// A caller with a context window cares about tokens, not matches.
        #[arg(long, default_value_t = 6000)]
        max_tokens: usize,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Find where a function, class or constant is defined
    Define {
        symbol: String,
        /// Only repositories carrying this label
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Stop adding results once the output would exceed this many tokens.
        #[arg(long, default_value_t = 6000)]
        max_tokens: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Print a source file from the corpus
    Show {
        repo: String,
        path: String,
        /// First line to print, 1-based. Search results carry the line number,
        /// so an agent can read around a match without pulling the whole file.
        #[arg(long)]
        from: Option<usize>,
        /// Last line to print. Defaults to `from` plus 120.
        #[arg(long)]
        to: Option<usize>,
        /// Print at most this many lines. A large file otherwise costs tens of
        /// thousands of tokens to read.
        #[arg(long, default_value_t = 400)]
        limit: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// List the files indexed for one repository
    Files {
        repo: String,
        /// Most files to list; --json lists every file when unset
        #[arg(long)]
        limit: Option<usize>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Show what changed upstream recently
    Recent {
        /// Only repositories carrying this tag
        #[arg(long)]
        tag: Option<String>,
        /// Only this repository
        #[arg(long)]
        repo: Option<String>,
        /// How far back to look
        #[arg(long, default_value_t = 72)]
        hours: u32,
        #[arg(long, default_value_t = 40)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Label repositories so they can be grouped and filtered
    Tag {
        /// Labels to apply. Omit both flags to list every tag in use.
        #[arg(long, value_delimiter = ',')]
        add: Vec<String>,
        /// Labels to take off
        #[arg(long, value_delimiter = ',')]
        remove: Vec<String>,
        /// Repositories to label. Omit with --add to tag everything.
        repos: Vec<String>,
    },
    /// List indexed repositories
    Repos {
        /// Only repositories carrying this tag
        #[arg(long)]
        tag: Option<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
        /// Most repositories to list (text default 200; --json lists all).
        /// The rest are counted, not printed
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Remove a repository from the corpus
    Remove { repo: String },
    /// Find repositories on GitHub and optionally index them
    Discover {
        /// GitHub search qualifiers. Defaults to the configured query.
        query: Option<String>,
        /// Recently active repositories instead of a topic search
        #[arg(long)]
        trending: bool,
        /// With --trending, how far back activity counts
        #[arg(long, default_value_t = 7)]
        days: u32,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        min_stars: Option<u32>,
        #[arg(long)]
        limit: Option<usize>,
        /// Index everything found, rather than just listing it
        #[arg(long)]
        add: bool,
        /// Label whatever is indexed, e.g. --tag terraform
        #[arg(long, value_delimiter = ',')]
        tag: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove repositories with no recent upstream commits
    Decay {
        /// Override the configured decay_months
        #[arg(long)]
        months: Option<u32>,
        /// List what would be removed without removing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Show or change settings
    Config {
        /// Setting to change; omit to list everything
        key: Option<String>,
        /// New value; omit to show just this setting
        value: Option<String>,
    },
    /// Reclaim disk space left behind by updates and removals
    Compact,
    /// Disk usage report
    Stats,
    /// Replace this binary with the latest release. Also runs at the end of `update`.
    Upgrade {
        /// Report whether a newer release exists without installing it
        #[arg(long)]
        check: bool,
    },
}

/// Where the corpus lives.
///
/// Defaults under the home directory so the command works from any directory,
/// which is what a globally installed tool has to do.
fn corpus_root(flag: Option<PathBuf>) -> PathBuf {
    if let Some(path) = flag {
        return path;
    }
    if let Some(path) = std::env::var_os("STEROIDS_ROOT") {
        return PathBuf::from(path);
    }
    // Windows sets USERPROFILE rather than HOME, and without this the corpus
    // would land in whatever directory the command happened to run from.
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match home {
        Some(home) => PathBuf::from(home).join(".steroids"),
        None => PathBuf::from("./corpus-data"),
    }
}

/// A repository name as a terminal hyperlink to its GitHub page.
///
/// Uses the OSC 8 escape, which terminals that do not understand it ignore
/// entirely, so the text still lines up in a column either way. The padding is
/// applied to the visible name, since the escape sequence has no width.
fn link(repo: &str, width: usize) -> String {
    let padded = format!("{repo:<width$}");
    // Only a terminal understands the escape. Piped or redirected output must
    // stay plain text, or the sequence shows up as literal rubbish in a file.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return padded;
    }
    // A gitee: prefix is our own, not part of the URL.
    let Some(bare) = repo.strip_prefix("gitee:") else {
        return format!("\u{1b}]8;;https://github.com/{repo}\u{7}{padded}\u{1b}]8;;\u{7}");
    };
    format!("\u{1b}]8;;https://gitee.com/{bare}\u{7}{padded}\u{1b}]8;;\u{7}")
}

/// Shorten to `width`, so a column stays a column.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "\u{2026}"
}

/// Bring the trigram index up to date, reporting on stderr.
///
/// Every command that changes the corpus ends here rather than printing
/// "next: steroids index". That hint was the single worst trap for an agent:
/// one that ran `add` and searched straight away was told the topic was
/// absent from the corpus and sent off to discover more repositories, when
/// the code was sitting there unindexed. An incremental run is near instant
/// for one repository and a couple of minutes after a full starter ingest,
/// and either is cheaper than a wrong answer.
fn run_index(store: &mut Store, rebuild: bool) -> Result<()> {
    let builder = if rebuild {
        index::rebuild
    } else {
        index::build
    };
    // Carriage-return progress is for a terminal. Captured by an agent or a
    // log it is hundreds of lines saying nothing.
    let live = stderr_is_terminal();
    let stats = builder(store, &mut |done, total| {
        if live {
            eprint!("\r  indexing {done}/{total}");
            let _ = std::io::stderr().flush();
        }
    })?;
    // "N documents" alone after `add owner/name` reads as the size of what
    // was just added, when it is the corpus. Say both.
    eprintln!(
        "{}  indexed {} documents ({} in corpus), {} trigrams stored ({} too common)   ",
        overwrite(),
        stats.added,
        stats.documents,
        stats.trigrams_stored,
        stats.trigrams_seen - stats.trigrams_stored
    );
    Ok(())
}

fn stderr_is_terminal() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// The carriage return that erases a progress line on a terminal. In a log or
/// an agent's capture there is no line to erase and `\r` is just a stray byte
/// at the front of every message.
fn overwrite() -> &'static str {
    if stderr_is_terminal() { "\r" } else { "" }
}

fn human(bytes: f64) -> String {
    for (unit, scale) in [("GB", 1e9), ("MB", 1e6), ("KB", 1e3)] {
        if bytes >= scale {
            return format!("{:.1}{unit}", bytes / scale);
        }
    }
    format!("{bytes:.0}B")
}

/// Report a search filter nothing in the corpus satisfies.
///
/// Same shape as any other empty result, so a caller parsing `--json` sees
/// the usual keys with `reason: filter_excludes_all` rather than a stray
/// line of prose that fails to parse.
fn report_filter_miss(store: &Store, json: bool, pattern: &str, advice: String) -> Result<()> {
    let facts = search::facts(store, search::Diagnosis::FilterExcludesAll { advice })?;
    if json {
        let unindexed = search::unindexed(store)?;
        println!("{}", render::render_empty_json(&facts, pattern, &unindexed));
    } else {
        println!("{}", render_empty(&facts));
    }
    Ok(())
}

/// First line of a text result: honest about truncation, so "3 of 20 shown"
/// is read before the reader spends the missing seventeen elsewhere.
fn match_header(
    visible: usize,
    results: &search::SearchResults,
    noun: &str,
    label: &str,
) -> String {
    let total = results.len();
    let more = if results.more_available {
        ", more available (raise --limit or narrow the search)"
    } else {
        ""
    };
    if visible < total {
        format!(
            "{visible} of {total} {noun} shown for {label} ({} omitted: token budget){more}",
            total - visible
        )
    } else {
        format!("{total} {noun} for {label}{more}")
    }
}

/// A count flag set to zero can only produce nothing, and "no results" would
/// blame the query for the argument.
fn check_positive(flag: &str, value: Option<usize>) -> Result<()> {
    if value == Some(0) {
        bail!("{flag} must be at least 1");
    }
    Ok(())
}

/// Below this not even one match fits, and the budget silently means "one".
fn check_token_budget(max_tokens: usize) -> Result<()> {
    if max_tokens < 50 {
        bail!("--max-tokens must be at least 50");
    }
    Ok(())
}

/// `--tag ''` matches nothing and looks like no filter at all.
fn nonempty_tag(tag: Option<String>) -> Result<Option<String>> {
    match tag {
        Some(tag) if tag.trim().is_empty() => bail!("tag must not be empty"),
        other => Ok(other),
    }
}

/// The stored spelling for each repository the caller named, in any form
/// `add` accepts, plus the normalised names of those not in the corpus.
fn resolve_repos(store: &Store, names: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    for name in names {
        let normalised = fetch::normalize_repo(name)?;
        match store.find_repo(&normalised)? {
            Some(stored) => found.push(stored),
            None => missing.push(normalised),
        }
    }
    Ok((found, missing))
}

/// The one stored spelling for a repository the caller named, or a clear
/// "not in corpus" with the normalised name and exit 1.
fn resolve_repo(store: &Store, name: &str) -> Result<String> {
    let (found, missing) = resolve_repos(store, std::slice::from_ref(&name.to_string()))?;
    // An error rather than a print-and-exit, so `--json` callers get their
    // `{"error"}` object like every other failure.
    found
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("not in corpus: {}", missing[0]))
}

/// Let a closed pipe end the process quietly, as every Unix tool does.
///
/// Rust ignores SIGPIPE at startup so a write to a reader that has gone away
/// returns EPIPE, which `println!` turns into a panic. Agents pipe into `head`
/// constantly, and exit 101 with a stack trace is not what `head` means.
/// Declared directly, like `flock` in store.rs, rather than pulling in libc.
#[cfg(unix)]
fn restore_sigpipe() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    // SAFETY: resetting a signal disposition to its default has no
    // preconditions and no memory is shared with the handler.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

/// Whether the invocation asked for JSON, so an error can honour it too.
fn wants_json(command: &Option<Command>) -> bool {
    matches!(
        command,
        Some(
            Command::Search { json: true, .. }
                | Command::Define { json: true, .. }
                | Command::Show { json: true, .. }
                | Command::Files { json: true, .. }
                | Command::Recent { json: true, .. }
                | Command::Repos { json: true, .. }
                | Command::Discover { json: true, .. }
                | Command::Audit { json: true }
        )
    )
}

fn main() {
    restore_sigpipe();
    let cli = Cli::parse();
    let json = wants_json(&cli.command);
    if let Err(error) = run(cli) {
        // A caller parsing --json gets the failure in the shape it asked for,
        // on the stream it is reading, rather than prose on the other one.
        if json {
            println!("{}", serde_json::json!({ "error": format!("{error:#}") }));
        } else {
            eprintln!("Error: {error:?}");
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let parallel = cli.parallel;
    let root = corpus_root(cli.root);
    upgrade::cleanup_old();

    let Some(command) = cli.command else {
        // The browser needs a terminal on both ends. An agent has neither,
        // and ratatui's failure to set one up is a panic, not a message.
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            use clap::CommandFactory;
            eprintln!("{}", Cli::command().render_help());
            std::process::exit(2);
        }
        return tui::run(root.clone(), Store::open(&root)?);
    };
    // Before the corpus is opened: a corpus stamped by a newer binary refuses
    // to open, and this is the command that resolves that.
    if let Command::Upgrade { check } = command {
        report_upgrade(upgrade::upgrade(&root, check)?);
        return Ok(());
    }

    // Commands that write must hold the corpus lock for their whole run, so a
    // second ingest waits instead of corrupting the shared dictionary.
    let writes = matches!(
        command,
        Command::Add { .. }
            | Command::Update
            | Command::Index { .. }
            | Command::Decay { .. }
            | Command::Remove { .. }
            | Command::Compact
            | Command::Tag { .. }
            | Command::Config { .. }
            | Command::Discover { add: true, .. }
    );
    let mut store = if writes {
        Store::open_for_write(&root)?
    } else {
        Store::open(&root)?
    };

    match command {
        Command::Add {
            repos,
            from_file,
            include_tests,
            tag,
        } => {
            let mut names = repos;
            if let Some(path) = from_file {
                for line in std::fs::read_to_string(&path)?.lines() {
                    let name = line.split('#').next().unwrap_or("").trim();
                    if !name.is_empty() {
                        names.push(name.to_string());
                    }
                }
            }
            if names.is_empty() {
                eprintln!("No repositories given. Pass names or --from-file.");
                std::process::exit(2);
            }
            let failures = ingest_all(
                &mut store,
                &names,
                include_tests,
                parallel,
                &Default::default(),
            )?
            .failed
            .len();
            // Label whatever landed, so a partly failed batch is still tagged.
            if !tag.is_empty() {
                for name in &names {
                    if let Ok(repo) = fetch::normalize_repo(name)
                        && let Some(stored) = store.find_repo(&repo)?
                    {
                        store.tag_repo(&stored, &tag)?;
                    }
                }
                eprintln!("  tagged: {}", tag.join(", "));
            }
            // The repositories that did land are indexed even when others
            // failed, or a partial batch would leave what worked unsearchable.
            run_index(&mut store, false)?;
            if failures > 0 {
                std::process::exit(1);
            }
        }

        Command::Update => {
            let summaries = store.list_repos()?;
            if summaries.is_empty() {
                println!("  nothing to update: steroids add owner/name");
                return Ok(());
            }
            // Pass the commit we already hold so anything upstream has not
            // moved is skipped instead of re-downloaded.
            let known: std::collections::HashMap<String, String> = summaries
                .iter()
                .map(|s| (s.name.clone(), s.commit_sha.clone()))
                .collect();
            let names: Vec<String> = summaries.into_iter().map(|s| s.name).collect();

            // Re-adding replaces the previous copy, so this is just an ingest
            // of everything already tracked.
            let outcome = ingest_all(&mut store, &names, false, parallel, &known)?;
            println!(
                "  {} unchanged, {} updated, {} failed",
                outcome.unchanged,
                outcome.added,
                outcome.failed.len()
            );
            if !outcome.failed.is_empty() {
                std::process::exit(1);
            }

            let settings = config::Config::load(&store)?;
            if settings.auto_discover {
                let existing: std::collections::HashSet<String> = names.into_iter().collect();
                match discover::search(
                    &settings.discover_query,
                    settings.min_stars,
                    settings.max_age_months,
                    settings.discover_limit,
                    &existing,
                ) {
                    Ok(found) => {
                        let fresh: Vec<String> = found
                            .new
                            .into_iter()
                            .map(|candidate| candidate.repo)
                            .collect();
                        if !fresh.is_empty() {
                            eprintln!("  discovering {} new repositories…", fresh.len());
                            ingest_all(&mut store, &fresh, false, parallel, &Default::default())?;
                        }
                    }
                    // Discovery is a bonus; a search failure must not fail the
                    // update of repositories the user actually asked for.
                    Err(error) => eprintln!("  discovery skipped: {error}"),
                }
            }

            // Replaced content orphans its old bytes, so reclaim them here
            // rather than letting the corpus grow with every update.
            let reclaimed = store.compact()?;
            if reclaimed > 0 {
                eprintln!("  reclaimed {}", human(reclaimed as f64));
            }
            run_index(&mut store, false)?;

            // Last, so this process finishes on the binary it started with.
            // A failed upgrade is reported, not fatal: the corpus is fresh.
            if settings.auto_upgrade {
                match upgrade::upgrade(&root, false) {
                    Ok(outcome) => report_upgrade(outcome),
                    Err(error) => eprintln!("  upgrade skipped: {error:#}"),
                }
            }
            return Ok(());
        }

        Command::Index {
            rebuild,
            refilter,
            include_tests,
        } => {
            if refilter {
                let dropped = store
                    .drop_filtered(|path, size| filters::should_index(path, size, include_tests))?;
                eprintln!("  dropped {dropped} files the current filters reject");
                // A repository with nothing left is what `add` now refuses to
                // create; keeping one only makes `repos` lie about its size.
                let empty = store.remove_empty_repos()?;
                if !empty.is_empty() {
                    eprintln!(
                        "  removed {} repositories with no indexable files: {}",
                        empty.len(),
                        empty.join(", ")
                    );
                }
            }
            run_index(&mut store, rebuild)?
        }

        Command::Search {
            pattern,
            repo,
            tag,
            language,
            path,
            ignore_case,
            fixed_strings,
            include_comments,
            context,
            per_repo,
            max_tokens,
            limit,
            json,
        } => {
            check_positive("--per-repo", per_repo)?;
            check_token_budget(max_tokens)?;
            let tag = nonempty_tag(tag)?;
            let shown = pattern.clone();
            let pattern = if fixed_strings {
                regex::escape(&pattern)
            } else {
                pattern
            };
            let language = language.as_deref().map(filters::canonical_language);
            let path = path.as_deref().map(search::path_filter).transpose()?;
            // A filter that excludes everything is not a failed search, and
            // blaming the pattern sends the agent rewriting a query that was
            // never the problem. Check the filters before running it.
            let (repos, missing) = resolve_repos(&store, &repo)?;
            if !missing.is_empty() {
                let count = store.list_repos()?.len();
                let advice = format!(
                    "{} not in this corpus, so no search can match. \
                     {count} repositories are indexed; run `steroids repos` to see \
                     them, or `steroids add {}` to index {}.",
                    missing
                        .iter()
                        .map(|r| format!("'{r}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                        + if missing.len() == 1 { " is" } else { " are" },
                    missing.join(" "),
                    if missing.len() == 1 { "it" } else { "them" }
                );
                return report_filter_miss(&store, json, &shown, advice);
            }
            let query = Query {
                repos: &repos,
                tag: tag.as_deref(),
                language: language.as_deref(),
                path_glob: path.as_deref(),
                ignore_case,
                skip_comments: !include_comments,
                // Capped: a huge value would paste whole files into a reply.
                context_lines: context.min(40),
                per_repo,
                ..Query::new(limit)
            };
            if let Some(language) = &language {
                // Every language in the indexed files, not just each
                // repository's main one: a Python project can still hold the
                // shell or SQL being searched for.
                let known: std::collections::BTreeSet<String> =
                    store.languages()?.into_iter().collect();
                if !known.contains(language) {
                    let advice = format!(
                        "No {language} files are indexed, so no search can match. \
                         Languages present: {}. Drop --language, or index a \
                         {language} project with \
                         `steroids discover 'language:{language}' --add`.",
                        known.into_iter().collect::<Vec<_>>().join(", ")
                    );
                    return report_filter_miss(&store, json, &shown, advice);
                }
            }
            if let Some(tag) = &tag
                && store.repos_tagged(Some(tag))?.is_empty()
            {
                let known = store.tag_counts()?;
                let advice = format!(
                    "No repositories are tagged '{tag}'. {}",
                    if known.is_empty() {
                        "No tags exist yet: steroids tag --add <label> <repo>".to_string()
                    } else {
                        format!(
                            "Known tags: {}",
                            known
                                .iter()
                                .map(|(t, n)| format!("{t} ({n})"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                );
                return report_filter_miss(&store, json, &shown, advice);
            }
            let matches = search::search(&mut store, &pattern, &query)?;
            // Checked after the search rather than before: it scans every
            // path, and the common case of a glob that matches is not worth
            // 100ms on each query. Without it a glob that excludes everything
            // is diagnosed as a pattern problem and the agent loosens a
            // pattern that was never at fault.
            if matches.is_empty()
                && let Some(glob) = &path
                && !store.any_path_matches(glob)?
            {
                let advice = format!(
                    "No indexed file path matches '{glob}', so no search can match. \
                     Paths are relative to the repository root, like {}. Loosen the \
                     glob, e.g. '**/*.ext', or drop --path.",
                    store
                        .sample_paths(3)?
                        .iter()
                        .map(|p| format!("'{p}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return report_filter_miss(&store, json, &shown, advice);
            }
            let unindexed = search::unindexed(&store)?;
            if json {
                let facts;
                println!(
                    "{}",
                    if matches.is_empty() {
                        facts = search::diagnose(&mut store, &pattern)?;
                        render::render_empty_json(&facts, &shown, &unindexed)
                    } else {
                        render::render_matches_json(&matches, &shown, &unindexed, max_tokens)
                    }
                );
            } else if matches.is_empty() {
                print!("{}", render::unindexed_note(&unindexed));
                println!("{}", render_empty(&search::diagnose(&mut store, &pattern)?));
            } else {
                print!("{}", render::unindexed_note(&unindexed));
                let label = if fixed_strings {
                    format!("'{shown}'")
                } else {
                    format!("/{shown}/")
                };
                print!(
                    "{}",
                    render::render_matches_within(
                        &matches,
                        |visible| match_header(visible, &matches, "match(es)", &label),
                        max_tokens,
                    )
                );
            }
        }

        Command::Define {
            symbol,
            tag,
            language,
            limit,
            max_tokens,
            json,
        } => {
            let symbol = symbol.trim().to_string();
            if symbol.is_empty() {
                bail!("symbol must not be empty");
            }
            // One character matches a loop variable in every file; nobody
            // defines a type called `x`.
            if symbol.chars().count() < 2 {
                bail!("symbol must be at least 2 characters");
            }
            check_token_budget(max_tokens)?;
            let tag = nonempty_tag(tag)?;
            let language = language.as_deref().map(filters::canonical_language);
            let escaped = regex::escape(&symbol);
            let query = Query {
                tag: tag.as_deref(),
                language: language.as_deref(),
                ..Query::new(limit)
            };
            let mut matches =
                search::search(&mut store, &search::definition_pattern(&symbol), &query)?;
            matches.matches.retain(|hit| {
                let line = hit
                    .context
                    .get(hit.line_number.saturating_sub(hit.context_first_line))
                    .map(String::as_str)
                    .unwrap_or_default();
                !search::is_binding_not_definition(line, &symbol)
            });
            let unindexed = search::unindexed(&store)?;
            if json {
                let facts;
                println!(
                    "{}",
                    if matches.is_empty() {
                        facts = search::diagnose(&mut store, &escaped)?;
                        render::render_empty_json(&facts, &symbol, &unindexed)
                    } else {
                        render::render_matches_json(&matches, &symbol, &unindexed, max_tokens)
                    }
                );
            } else if matches.is_empty() {
                print!("{}", render::unindexed_note(&unindexed));
                // A name that is used but never defined here points at a
                // dependency; a name nobody mentions points at a typo. Saying
                // which saves the next query.
                let uses = search::search(
                    &mut store,
                    &format!(r"\b{escaped}\b"),
                    &Query {
                        tag: tag.as_deref(),
                        language: language.as_deref(),
                        ..Query::new(100)
                    },
                )?;
                if uses.is_empty() {
                    println!("{}", render_empty(&search::diagnose(&mut store, &escaped)?));
                } else {
                    println!(
                        "No definition of '{symbol}' in this corpus, but it is referenced {}{} \
                         time(s): it is probably defined in a dependency. \
                         `steroids search '\\b{escaped}\\b'` shows the uses.",
                        uses.len(),
                        if uses.more_available { "+" } else { "" }
                    );
                }
            } else {
                print!("{}", render::unindexed_note(&unindexed));
                let label = format!("'{symbol}'");
                print!(
                    "{}",
                    render::render_matches_within(
                        &matches,
                        |visible| match_header(visible, &matches, "definition(s)", &label),
                        max_tokens,
                    )
                );
            }
        }

        Command::Show {
            repo,
            path,
            from,
            to,
            limit,
            json,
        } => {
            let repo = resolve_repo(&store, &repo)?;
            match store.read_path(&repo, &path)? {
                Some(content) => {
                    let text = String::from_utf8_lossy(&content);
                    let lines: Vec<&str> = text.lines().collect();
                    let start = from.unwrap_or(1).max(1);
                    if limit == 0 {
                        bail!("--limit must be at least 1");
                    }
                    if to.is_some_and(|to| to < start) {
                        bail!("--to ({}) is before --from ({start})", to.unwrap_or(0));
                    }
                    // A range around a known line is the common case, so default
                    // to a window rather than the rest of the file.
                    let end = to
                        .unwrap_or(if from.is_some() {
                            start + 120
                        } else {
                            usize::MAX
                        })
                        .min(lines.len())
                        .min(start + limit - 1);

                    if json {
                        let slice = if start > lines.len() {
                            &lines[..0]
                        } else {
                            &lines[start - 1..end]
                        };
                        println!(
                            "{}",
                            serde_json::json!({
                                "repo": repo,
                                "path": path,
                                "from": start,
                                "to": if slice.is_empty() { 0 } else { end },
                                "total_lines": lines.len(),
                                "lines": slice,
                            })
                        );
                        return Ok(());
                    }
                    println!("# {repo}/{path}");
                    if start > lines.len() {
                        println!("\nFile has {} lines; nothing at line {start}.", lines.len());
                        return Ok(());
                    }
                    println!(
                        "# lines {start}-{end} of {}{}\n",
                        lines.len(),
                        if end < lines.len() {
                            "  (use --from/--to for the rest)"
                        } else {
                            ""
                        }
                    );
                    // Numbered, so a line an agent reads here matches the number a
                    // search result gave it.
                    let width = end.to_string().len();
                    for (offset, line) in lines[start - 1..end].iter().enumerate() {
                        println!("{:>width$} \u{2502} {line}", start + offset);
                    }
                }
                None => bail!("not in corpus: {repo}/{path}"),
            }
        }

        Command::Files { repo, limit, json } => {
            check_positive("--limit", limit)?;
            let repo = resolve_repo(&store, &repo)?;
            // Text is read by a person, so it is capped; JSON is read by a
            // program that asked for the list, so it gets the list.
            let cap = limit.unwrap_or(if json { usize::MAX } else { 200 });
            let paths = store.list_files(&repo, cap)?;
            if paths.is_empty() {
                bail!(
                    "no files indexed for {repo}; `steroids index --refilter` removes empty repositories"
                );
            }
            if json {
                let files: Vec<serde_json::Value> = paths
                    .iter()
                    .map(|(path, language, size)| {
                        serde_json::json!({ "path": path, "language": language, "size": size })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "repo": repo, "count": files.len(), "files": files })
                );
                return Ok(());
            }
            // Tab separated: `cut -f1` and `awk` read it without guessing at
            // column widths, and the summary goes to stderr so it never lands
            // in the pipe.
            for (path, language, size) in &paths {
                println!("{path}\t{language}\t{size}");
            }
            eprintln!(
                "{} files shown for {repo}{}",
                paths.len(),
                if paths.len() == cap {
                    " (raise --limit for more)"
                } else {
                    ""
                }
            );
        }

        Command::Repos { tag, json, limit } => {
            check_positive("--limit", limit)?;
            let tag = nonempty_tag(tag)?;
            let all = store.repos_tagged(tag.as_deref())?;
            // A listing is read inside a context window. 444 repositories are
            // 12,000 tokens; 50,000 would be a million, for a command an
            // agent runs to get its bearings. A program parsing JSON asked
            // for the list and gets all of it unless it says otherwise.
            let limit = limit.unwrap_or(if json { usize::MAX } else { 200 });
            let rows = &all[..all.len().min(limit)];
            let hidden = all.len() - rows.len();
            if json {
                let items: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|summary| {
                        serde_json::json!({
                            "repo": summary.name,
                            "commit": summary.commit_sha,
                            "indexed_at": summary.indexed_at,
                            "files": summary.files,
                            "language": summary.language,
                            "disk_bytes": summary.disk_bytes,
                            "source_bytes": summary.source_bytes,
                            "last_commit": summary.pushed_at,
                            "tags": summary.tags.split(',').filter(|t| !t.is_empty())
                                .collect::<Vec<_>>(),
                            "url": format!("https://github.com/{}", summary.name),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "count": all.len(),
                        "shown": rows.len(),
                        "repositories": items,
                    }))?
                );
            } else if rows.is_empty() {
                println!("  no repositories yet: steroids add owner/name");
            } else {
                for summary in rows {
                    println!(
                        "  {:<40} {:<12} {:>5} files  {:>8}  {}  {}",
                        link(&summary.name, 40),
                        summary.language,
                        summary.files,
                        human(summary.disk_bytes as f64),
                        &summary.commit_sha[..8.min(summary.commit_sha.len())],
                        summary.indexed_at
                    );
                }
                if hidden > 0 {
                    println!(
                        "\n  {} of {} repositories shown in {}; narrow with --tag or raise --limit",
                        rows.len(),
                        all.len(),
                        root.display()
                    );
                } else {
                    println!("\n  {} repositories in {}", rows.len(), root.display());
                }
            }
        }

        Command::Remove { repo } => {
            let repo = resolve_repo(&store, &repo)?;
            if store.remove_repo(&repo)? {
                println!("  removed {repo}");
                run_index(&mut store, false)?;
            } else {
                println!("  not in corpus: {repo}");
                std::process::exit(1);
            }
        }

        Command::Discover {
            query,
            trending,
            days,
            language,
            min_stars,
            limit,
            add,
            tag,
            json,
        } => {
            let settings = config::Config::load(&store)?;
            let min_stars = min_stars.unwrap_or(settings.min_stars);
            let limit = limit.unwrap_or(settings.discover_limit);
            check_positive("--limit", Some(limit))?;

            // Never re-fetch what is already indexed.
            let existing: std::collections::HashSet<String> = store
                .list_repos()?
                .into_iter()
                .map(|summary| summary.name)
                .collect();
            let found = if trending {
                discover::trending(days, language.as_deref(), min_stars, limit, &existing)?
            } else {
                let mut text = query.unwrap_or_else(|| settings.discover_query.clone());
                if let Some(language) = &language {
                    text.push_str(&format!(" language:{language}"));
                }
                discover::search(&text, min_stars, settings.max_age_months, limit, &existing)?
            };
            let fresh = found.new;

            if json {
                let items: Vec<serde_json::Value> = fresh
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "repo": c.repo,
                            "stars": c.stars,
                            "pushed_at": c.pushed_at,
                            "language": c.language,
                            "description": c.description,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "found": found.found,
                        "already_indexed": found.already_indexed,
                        "new": items,
                    }))?
                );
            } else if fresh.is_empty() {
                println!(
                    "  nothing new found ({} results, {} already indexed)",
                    found.found, found.already_indexed
                );
            } else {
                for c in &fresh {
                    println!(
                        "  {:<44} {:>7}★  {:<12} {}",
                        c.repo,
                        c.stars,
                        c.language,
                        c.pushed_at.split('T').next().unwrap_or("")
                    );
                }
                println!("\n  {} new repositories", fresh.len());
            }

            if add && !fresh.is_empty() {
                let names: Vec<String> = fresh.into_iter().map(|c| c.repo).collect();
                eprintln!();
                // A repository that fails to fetch must not discard the ones
                // that succeeded: they are on disk and need indexing.
                let failures =
                    ingest_all(&mut store, &names, false, parallel, &Default::default())?
                        .failed
                        .len();
                // Label what landed, so a discovery run is immediately
                // searchable as a group.
                if !tag.is_empty() {
                    for name in &names {
                        if let Some(stored) = store.find_repo(name)? {
                            store.tag_repo(&stored, &tag)?;
                        }
                    }
                    eprintln!("  tagged: {}", tag.join(", "));
                }
                run_index(&mut store, false)?;
                if failures == names.len() {
                    std::process::exit(1);
                }
            } else if !add && !fresh.is_empty() {
                eprintln!("  add them with: steroids discover --add");
            }
        }

        Command::Decay { months, dry_run } => {
            let settings = config::Config::load(&store)?;
            let months = months.unwrap_or(settings.decay_months);
            // Age and archived are separate rules. Removing repositories the
            // owner has frozen should not require opting into an age limit,
            // since an archive will never improve no matter how recent it is.
            if months == 0 && !settings.decay_archived {
                println!(
                    "  decay is off. Enable with: steroids config decay_months 6 \
                     (archived repos are removed by default)"
                );
                return Ok(());
            }

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // Months as 30 days: exact calendar months are not worth a date
            // dependency for a threshold the user picks in round numbers.
            // An age of zero means no age rule at all, so use a cutoff no
            // stored date can precede.
            let cutoff = if months == 0 {
                "0000-00-00".to_string()
            } else {
                discover::iso_date(now.saturating_sub(months as u64 * 30 * 86_400))
            };
            let stale = store.stale_repos(&cutoff, settings.decay_archived)?;

            if stale.is_empty() {
                if months == 0 {
                    println!("  no archived repositories");
                } else {
                    println!("  nothing older than {months} months (cutoff {cutoff})");
                }
                return Ok(());
            }
            for (name, pushed, archived) in &stale {
                println!(
                    "  {name:<44} last commit {}{}",
                    pushed.split('T').next().unwrap_or(""),
                    if *archived { "  (archived)" } else { "" }
                );
            }
            if dry_run {
                println!("\n  {} would be removed (dry run)", stale.len());
                return Ok(());
            }
            for (name, ..) in &stale {
                store.remove_repo(name)?;
            }
            let reclaimed = store.compact()?;
            println!(
                "\n  removed {}, reclaimed {}",
                stale.len(),
                human(reclaimed as f64)
            );
            run_index(&mut store, false)?;
        }

        Command::Config { key, value } => {
            let mut settings = config::Config::load(&store)?;
            match (key, value) {
                (None, _) => {
                    for (key, help) in config::KEYS {
                        println!("  {key:<16} {:<28} {help}", settings.get(key));
                    }
                }
                // Reading an unknown key prints an empty line and exits 0,
                // which a script reads as "unset" rather than "misspelt".
                (Some(key), None) if !config::KEYS.iter().any(|(k, _)| *k == key) => {
                    bail!(
                        "unknown setting {key:?}; known: {}",
                        config::KEYS
                            .iter()
                            .map(|(k, _)| *k)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
                (Some(key), None) => println!("{}", settings.get(&key)),
                (Some(key), Some(value)) => {
                    settings.set(&key, &value)?;
                    settings.save(&store)?;
                    println!("  {key} = {}", settings.get(&key));
                }
            }
        }

        Command::Recent {
            tag,
            repo,
            hours,
            limit,
            json,
        } => {
            if hours == 0 {
                bail!("--hours must be at least 1");
            }
            let tag = nonempty_tag(tag)?;
            // A repository not in the corpus has no commits to report, and
            // saying "no commits" invites the caller to conclude the project
            // is quiet rather than absent.
            let repos: Vec<String> = match &repo {
                Some(one) => {
                    let (found, missing) = resolve_repos(&store, std::slice::from_ref(one))?;
                    if let Some(name) = missing.first() {
                        println!(
                            "'{name}' is not in this corpus. Index it first with \
                             `steroids add {name}`, or drop --repo to check everything \
                             that is."
                        );
                        return Ok(());
                    }
                    found
                }
                None => store
                    .repos_tagged(tag.as_deref())?
                    .into_iter()
                    .map(|r| r.name)
                    .collect(),
            };
            if repos.is_empty() {
                println!("  no repositories match. Try: steroids tag");
                return Ok(());
            }

            let mut commits = recent::for_repos(&repos, hours, parallel);
            // Newest first across every repository, so the answer to "what
            // moved this week" reads as one timeline.
            commits.sort_by(|a, b| b.when.cmp(&a.when));
            commits.truncate(limit);

            if json {
                let items: Vec<serde_json::Value> = commits
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "repo": c.repo,
                            "when": c.when,
                            "author": c.author,
                            "title": c.title,
                            "url": c.url,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if commits.is_empty() {
                println!(
                    "  no commits in the last {hours} hours across {} repositories",
                    repos.len()
                );
            } else {
                // Sorted by time across every repository, so one project
                // recurs. Name it on each line rather than heading a block a
                // later commit would break up anyway.
                for c in &commits {
                    println!(
                        "  {}  {:<28} {:<14} {}",
                        &c.when[..16],
                        truncate(&c.repo, 28),
                        truncate(&c.author, 14),
                        c.title
                    );
                }
                println!(
                    "\n  {} commits in the last {hours} hours, {} repositories checked",
                    commits.len(),
                    repos.len()
                );
            }
        }

        Command::Tag { add, remove, repos } => {
            // A tag is a single token: it is passed back through --tag on the
            // shell, and a space or comma inside one would either split it
            // there or need quoting nobody will remember. Whitespace-only is
            // the common accident (`--add ""`), and storing nothing while
            // reporting success would be a lie.
            for tag in add.iter().chain(&remove) {
                if tag.trim().is_empty() {
                    bail!("tag must not be empty");
                }
                if tag.chars().any(|c| c.is_whitespace() || c == ',') {
                    bail!("tag {tag:?} must not contain spaces or commas");
                }
            }
            if add.is_empty() && remove.is_empty() {
                let counts = store.tag_counts()?;
                if counts.is_empty() {
                    println!("  no tags yet. Add some: steroids tag --add coding-agent owner/name");
                } else {
                    for (tag, count) in counts {
                        println!("  {tag:<24} {count} repositories");
                    }
                }
                return Ok(());
            }
            let (targets, missing) = if repos.is_empty() {
                (
                    store.list_repos()?.into_iter().map(|r| r.name).collect(),
                    Vec::new(),
                )
            } else {
                resolve_repos(&store, &repos)?
            };
            for name in &missing {
                eprintln!("  not in corpus: {name}");
            }
            for name in &targets {
                if !add.is_empty() {
                    store.tag_repo(name, &add)?;
                }
                if !remove.is_empty() {
                    store.untag_repo(name, &remove)?;
                }
            }
            if !add.is_empty() {
                println!(
                    "  tagged {} repositories: {}",
                    targets.len(),
                    add.join(", ")
                );
            }
            if !remove.is_empty() {
                println!(
                    "  untagged {} repositories: {}",
                    targets.len(),
                    remove.join(", ")
                );
            }
            // A caller that named a repository this could not reach did not
            // get what it asked for, and exit 0 would say it had.
            if !missing.is_empty() {
                std::process::exit(1);
            }
        }

        Command::Compact => {
            // Compaction moves bytes, not document ids, so the index stays
            // valid and nothing needs re-running.
            let reclaimed = store.compact()?;
            println!("  reclaimed {}", human(reclaimed as f64));
        }

        Command::Audit { json } => {
            let report = audit::run(&store)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", audit::render(&report));
            }
        }

        Command::Stats => {
            let (repos, documents, raw) = store.totals()?;
            let blobs = std::fs::metadata(root.join("blobs.bin"))
                .map(|m| m.len())
                .unwrap_or(0);
            let db = std::fs::metadata(root.join("corpus.db"))
                .map(|m| m.len())
                .unwrap_or(0);
            let total = (blobs + db) as f64;
            println!("  location     : {}", root.display());
            println!("  repositories : {repos}");
            println!("  documents    : {documents}");
            println!("  source code  : {} raw", human(raw as f64));
            println!("  blobs.bin    : {} compressed", human(blobs as f64));
            println!(
                "  corpus.db    : {} (metadata + trigram index)",
                human(db as f64)
            );
            println!("  TOTAL ON DISK: {}", human(total));
            if repos > 0 {
                println!("  per repo     : {}", human(total / repos as f64));
            }
            if raw > 0 {
                println!("  ratio        : {:.2}x of raw source", total / raw as f64);
            }
        }

        Command::Upgrade { .. } => unreachable!("handled before the corpus is opened"),
    }
    // Every other command mentions a newer release once a day, on stderr so
    // --json output stays parseable.
    if config::Config::load(&store)?.auto_upgrade {
        upgrade::nudge(&root);
    }
    Ok(())
}

/// Stderr only: stdout belongs to the command that ran.
fn report_upgrade(outcome: upgrade::Outcome) {
    use upgrade::Outcome;
    match outcome {
        Outcome::UpToDate => eprintln!(
            "  steroids {} is the latest release",
            env!("CARGO_PKG_VERSION")
        ),
        Outcome::Available(version) => {
            eprintln!("  new version {version} available: steroids upgrade")
        }
        Outcome::Upgraded { from, to } => eprintln!("  steroids {from} → {to}"),
        Outcome::Skipped(reason) => eprintln!("  upgrade skipped: {reason}"),
    }
}

/// Fetch many repositories at once, reporting failures without aborting.
///
/// Ingest is network-bound, so downloads run across threads; the store is
/// still written from one.
fn ingest_all(
    store: &mut Store,
    names: &[String],
    include_tests: bool,
    parallel: usize,
    known: &std::collections::HashMap<String, String>,
) -> Result<bulk::BulkOutcome> {
    // Per-repository lines are useful for a handful and noise for hundreds.
    let terse = names.len() > 8;
    let live = stderr_is_terminal();
    let outcome = bulk::ingest_all(
        store,
        names,
        include_tests,
        parallel,
        known,
        &mut |name, result, done, total| match result {
            Ok(prepared) => {
                if terse {
                    if live {
                        eprint!("\r  {done}/{total} fetched…        ");
                        let _ = std::io::stderr().flush();
                    } else if done % 50 == 0 || done == total {
                        // A log still wants a pulse, one line per fifty rather
                        // than one per repository.
                        eprintln!("  {done}/{total} fetched");
                    }
                } else {
                    let sha = &prepared.upstream.commit_sha;
                    println!(
                        "  {name}: {} files kept of {} ({}) @ {}",
                        prepared.files.len(),
                        prepared.files_seen,
                        human(prepared.bytes_kept as f64),
                        &sha[..8.min(sha.len())]
                    );
                }
            }
            // "unchanged" is a skip, not a failure.
            Err("unchanged") => {}
            Err(error) => eprintln!("{}  {name}: FAILED ({error})              ", overwrite()),
        },
    )?;

    // Refusing a file silently would leave the user believing they indexed
    // something they did not, so always say what was dropped and why.
    if !outcome.rejected.is_empty() {
        eprintln!(
            "{}  skipped {} file(s) containing hidden characters:",
            overwrite(),
            outcome.rejected.len()
        );
        for path in outcome.rejected.iter().take(5) {
            eprintln!("    {path}");
        }
        if outcome.rejected.len() > 5 {
            eprintln!("    ...and {} more", outcome.rejected.len() - 5);
        }
    }

    if terse {
        if live {
            eprint!("\r                                   \r");
        }
        println!(
            "  {} repositories, {} files, {}",
            outcome.added,
            outcome.files,
            human(outcome.bytes as f64)
        );
        if outcome.unchanged > 0 {
            println!("  {} already up to date", outcome.unchanged);
        }
        // In terse mode the per-repository failures scrolled past, so summarise
        // them rather than leaving a bare "0 repositories" unexplained.
        if !outcome.failed.is_empty() {
            eprintln!("  {} failed:", outcome.failed.len());
            for (name, error) in outcome.failed.iter().take(5) {
                eprintln!("    {name}: {error}");
            }
            if outcome.failed.len() > 5 {
                eprintln!("    …and {} more", outcome.failed.len() - 5);
            }
        }
    }
    if !terse && outcome.unchanged > 0 {
        println!("  {} already up to date", outcome.unchanged);
    }
    Ok(outcome)
}
