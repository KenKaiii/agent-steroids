//! A local code corpus for coding agents.
//!
//! One binary, no runtime, no daemon: an agent calls `steroids search` the
//! same way it calls `grep`.

mod audit;
mod bulk;
mod changelog;
mod config;
mod deps;
mod discover;
mod fetch;
mod filters;
mod index;
mod outline;
mod recent;
mod render;
mod root;
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
        /// Lines of code shown either side of a match (default 2). Without it,
        /// more than 10 matches come back one line each
        #[arg(short = 'C', long)]
        context: Option<usize>,
        /// Most results to take from any one repository. Use 1 to see how many
        /// different projects solve something, rather than one project many
        /// times.
        #[arg(long)]
        per_repo: Option<usize>,
        /// Stop adding results once the output would exceed this many tokens.
        /// A caller with a context window cares about tokens, not matches.
        /// The default is the read cap: about 6,000 characters.
        #[arg(long, default_value_t = 1500)]
        max_tokens: usize,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Find where a function, class or constant is defined
    Define {
        /// One name, or several separated by commas: forEach,isPlainObject
        symbol: String,
        /// Only these repositories; each definition is then printed whole,
        /// up to 150 lines, instead of a few lines around it
        #[arg(long, value_delimiter = ',')]
        repo: Vec<String>,
        /// Path glob or prefix, e.g. 'lib/utils.js' or 'src'
        #[arg(long)]
        path: Option<String>,
        /// Only repositories carrying this label
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Stop adding results once the output would exceed this many tokens
        /// (about 6,000 characters by default); the rest are named
        #[arg(long, default_value_t = 1500)]
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
        /// thousands of tokens to read. Defaults to 400 unless --to is given.
        #[arg(long)]
        limit: Option<usize>,
        /// Stop adding lines once the output would exceed this many tokens
        #[arg(long)]
        max_tokens: Option<usize>,
        /// Print only these definitions' bodies, e.g. --symbols forEach,isPlainObject
        #[arg(long, value_delimiter = ',', conflicts_with_all = ["from", "to", "outline"])]
        symbols: Vec<String>,
        /// Print only what the file (or the --from/--to range) declares, with
        /// line numbers; in a test file, its test cases
        #[arg(long)]
        outline: bool,
        /// Leave out comments, docstrings and blank lines; numbering is kept
        #[arg(long)]
        no_comments: bool,
        /// Leave out the same-repo helpers the shown code calls
        #[arg(long)]
        no_uses: bool,
        /// Helpers already shown, as path:line (the line their definition
        /// starts on): named instead of repeated
        #[arg(long, value_delimiter = ',')]
        skip_uses: Vec<String>,
        /// Read a long file from the top instead of printing its outline.
        /// Like every read, it stops at 6,000 characters and says where to
        /// continue
        #[arg(long, conflicts_with_all = ["symbols", "outline"])]
        full: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// List the files indexed for one repository
    Files {
        repo: String,
        /// Only paths under this prefix or matching this glob, e.g. lib/helpers
        #[arg(long)]
        path: Option<String>,
        /// Most files to list; --json lists every file when unset
        #[arg(long)]
        limit: Option<usize>,
        /// One file per line with language and size, tab separated, instead
        /// of one line per directory
        #[arg(long)]
        long: bool,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// What a release changed, from the repository's own changelog
    Changes {
        repo: String,
        /// Release to show, e.g. 1.21.0; the latest when unset
        #[arg(long)]
        version: Option<String>,
        /// List the releases instead of showing one
        #[arg(long, conflicts_with = "version")]
        list: bool,
        /// The whole entry; without it, the opening words of each item
        #[arg(long, conflicts_with = "list")]
        full: bool,
        /// Only the items this regex matches (case-insensitive), whole
        #[arg(long, conflicts_with_all = ["list", "full"])]
        pattern: Option<String>,
        /// Stop adding lines once the output would exceed this many tokens
        #[arg(long, default_value_t = 3000)]
        max_tokens: usize,
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
        /// Only repositories whose name contains this text, e.g. axios
        filter: Option<String>,
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

/// `steroids config root [path|default]`. Handled before any corpus opens,
/// since it decides which corpus that would be.
fn config_root(value: Option<&str>) -> Result<()> {
    let default = root::default();
    let Some(value) = value else {
        println!("{}", root::stored(&default).unwrap_or(default).display());
        return Ok(());
    };
    let changed = root::set(&default, value)?;
    println!("  root = {}", changed.root.display());
    if let Some(from) = changed.left_behind {
        println!("  {}", root::move_hint(&from, &changed.root));
    }
    Ok(())
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

/// Lines `show` prints by default, so reading a whole large file stays affordable.
const SHOW_LIMIT: usize = 400;

/// Last line `show` prints. An explicit --to is a request for exactly that
/// range: capping it at the default silently returned less than was asked, and
/// agents re-read overlapping windows to find the rest.
fn show_end(
    total: usize,
    start: usize,
    from: Option<usize>,
    to: Option<usize>,
    limit: Option<usize>,
) -> usize {
    // A range around a known line is the common case, so default to a window
    // rather than the rest of the file.
    let wanted = to.unwrap_or(if from.is_some() {
        start.saturating_add(120)
    } else {
        usize::MAX
    });
    let cap = limit.unwrap_or(if to.is_some() { usize::MAX } else { SHOW_LIMIT });
    wanted.min(total).min(start.saturating_add(cap - 1))
}

/// Replace a definition hit's context with the definition itself, so one
/// `define --repo` answers what a follow-up `show` used to.
fn expand_to_body(store: &mut Store, hit: &mut search::Match) -> Result<()> {
    let Some(content) = store.read_path(&hit.repo, &hit.path)? else {
        return Ok(());
    };
    let text = String::from_utf8_lossy(&content);
    let lines: Vec<&str> = text.lines().collect();
    let language = filters::language_of(&hit.path).unwrap_or("");
    let start = hit.line_number;
    if start == 0 || start > lines.len() {
        return Ok(());
    }
    let end = outline::block_end(&lines, start, language);
    let shown = end.min(start + outline::MAX_BODY_LINES - 1);
    hit.context = lines[start - 1..shown]
        .iter()
        .map(|line| line.to_string())
        .collect();
    hit.context_first_line = start;
    hit.body_end = (shown < end).then_some(end);
    Ok(())
}

/// What snapshot of a repository is indexed: the release its manifest
/// declared, and the commit, so "is this the version I am porting?" is
/// answered without grepping for it.
fn snapshot_label(summary: &store::RepoSummary) -> String {
    let commit = &summary.commit_sha[..8.min(summary.commit_sha.len())];
    match (summary.version.is_empty(), commit.is_empty()) {
        (false, false) => format!("v{} ({commit})", summary.version),
        (false, true) => format!("v{}", summary.version),
        _ => commit.to_string(),
    }
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
                | Command::Changes { json: true, .. }
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
    if let Some(Command::Config { key, value }) = &cli.command
        && key.as_deref() == Some("root")
    {
        return config_root(value.as_deref());
    }
    let root = root::resolve(cli.root)?;
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
                context_lines: context.unwrap_or(search::DEFAULT_CONTEXT_LINES).min(40),
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
            // Each filter passed on its own, but together they hid every hit.
            // Diagnosing the pattern now would say "it does appear in the
            // corpus, loosen the pattern" and send the agent rewriting a query
            // that was fine; say where the hits are and which flags hide them.
            let filtered =
                !repos.is_empty() || tag.is_some() || language.is_some() || path.is_some();
            if matches.is_empty() && filtered {
                let unfiltered = search::search(
                    &mut store,
                    &pattern,
                    &Query {
                        repos: &[],
                        tag: None,
                        language: None,
                        path_glob: None,
                        per_repo: Some(1),
                        ..query
                    },
                )?;
                if !unfiltered.is_empty() {
                    let flags: Vec<&str> = [
                        (!repos.is_empty(), "--repo"),
                        (tag.is_some(), "--tag"),
                        (language.is_some(), "--language"),
                        (path.is_some(), "--path"),
                    ]
                    .into_iter()
                    .filter_map(|(set, flag)| set.then_some(flag))
                    .collect();
                    let mut where_: Vec<String> = unfiltered
                        .iter()
                        .take(5)
                        .map(|m| format!("{} {}", m.repo, m.path))
                        .collect();
                    where_.dedup();
                    let advice = format!(
                        "No matches within {}, but the pattern does match elsewhere in \
                         the corpus, e.g. {}. The pattern is fine; widen or drop the filter.",
                        flags.join("/"),
                        where_.join(", ")
                    );
                    return report_filter_miss(&store, json, &shown, advice);
                }
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
                        context.is_none() && matches.len() > search::COMPACT_ABOVE_MATCHES,
                    )
                );
            }
        }

        Command::Define {
            symbol,
            repo,
            path,
            tag,
            language,
            limit,
            max_tokens,
            json,
        } => {
            let names: Vec<String> = symbol
                .split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect();
            if names.is_empty() {
                bail!("symbol must not be empty");
            }
            // One character matches a loop variable in every file; nobody
            // defines a type called `x`.
            if names.iter().any(|name| name.chars().count() < 2) {
                bail!("symbol must be at least 2 characters");
            }
            let symbol = names.join(",");
            check_token_budget(max_tokens)?;
            let tag = nonempty_tag(tag)?;
            let language = language.as_deref().map(filters::canonical_language);
            let escaped = regex::escape(&names[0]);
            let glob = path.as_deref().map(search::path_filter).transpose()?;
            let (repos, missing) = resolve_repos(&store, &repo)?;
            if !missing.is_empty() {
                bail!(
                    "not in corpus: {}; `steroids repos <name>` finds the stored spelling",
                    missing.join(", ")
                );
            }
            // Scoped to known repositories, the reader is about to use the
            // code, not compare projects: the whole body answers that in one
            // call, where five lines of context sent it to read the file.
            // Across the corpus it is choosing one first, and the signature
            // line is enough to choose by.
            let scoped = !repos.is_empty() || glob.is_some();
            let query = Query {
                repos: &repos,
                tag: tag.as_deref(),
                language: language.as_deref(),
                path_glob: glob.as_deref(),
                context_lines: if scoped { 5 } else { 0 },
                ..Query::new(limit)
            };
            let mut matches = search::SearchResults {
                matches: Vec::new(),
                more_available: false,
            };
            let mut not_found = Vec::new();
            for name in &names {
                let mut found =
                    search::search(&mut store, &search::definition_pattern(name), &query)?;
                found.matches.retain(|hit| {
                    let line = hit
                        .context
                        .get(hit.line_number.saturating_sub(hit.context_first_line))
                        .map(String::as_str)
                        .unwrap_or_default();
                    !search::is_binding_not_definition(line, name)
                });
                // A variable bound inside a function shares the name but is
                // not what `define` is for; keep such hits only if nothing
                // better exists.
                if found
                    .matches
                    .iter()
                    .any(|hit| !search::is_local_binding(hit))
                {
                    found.matches.retain(|hit| !search::is_local_binding(hit));
                }
                if found.matches.is_empty() {
                    not_found.push(name.as_str());
                }
                matches.more_available |= found.more_available;
                matches.matches.extend(found.matches);
            }
            if scoped {
                for hit in matches.matches.iter_mut() {
                    expand_to_body(&mut store, hit)?;
                }
            }
            if names.len() > 1 && matches.is_empty() {
                bail!("no definition of {} found", names.join(", "));
            }
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
                        repos: &repos,
                        tag: tag.as_deref(),
                        language: language.as_deref(),
                        path_glob: glob.as_deref(),
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
                        false,
                    )
                );
                if !not_found.is_empty() && names.len() > 1 {
                    println!("\nnot found: {}", not_found.join(", "));
                }
                if !scoped {
                    println!("\nwhole body: steroids define {symbol} --repo <repo>");
                }
            }
        }

        Command::Show {
            repo,
            path,
            from,
            to,
            limit,
            max_tokens,
            symbols,
            outline,
            no_comments,
            full,
            no_uses,
            skip_uses,
            json,
        } => {
            check_positive("--limit", limit)?;
            if let Some(budget) = max_tokens {
                check_token_budget(budget)?;
            }
            let repo = resolve_repo(&store, &repo)?;
            let Some(content) = store.read_path(&repo, &path)? else {
                // A directory reads as the outline of every file under it:
                // what an agent looping `show` over each file was after.
                let directory = match path.trim_end_matches('/') {
                    "." => "",
                    directory => directory,
                };
                let under = format!("{directory}/");
                let files: Vec<String> = store
                    .list_files(&repo, usize::MAX)?
                    .into_iter()
                    .map(|(file, _, _)| file)
                    .filter(|file| directory.is_empty() || file.starts_with(&under))
                    .collect();
                let reading = !symbols.is_empty() || from.is_some() || to.is_some() || full;
                if files.is_empty() || reading {
                    bail!(
                        "not in corpus: {repo}/{path}; the repo's file list shows what is indexed"
                    );
                }
                let mut read = Vec::new();
                for file in files {
                    if let Some(bytes) = store.read_path(&repo, &file)? {
                        read.push((file, String::from_utf8_lossy(&bytes).into_owned()));
                    }
                }
                println!(
                    "{}",
                    outline::render_folder(&repo, directory, &read, max_tokens, json).trim_end()
                );
                return Ok(());
            };
            let text = String::from_utf8_lossy(&content);
            let lines: Vec<&str> = text.lines().collect();
            let start = from.unwrap_or(1).max(1);
            if to.is_some_and(|to| to < start) {
                bail!("--to ({}) is before --from ({start})", to.unwrap_or(0));
            }
            let symbols: Vec<String> = symbols
                .into_iter()
                .filter(|symbol| !symbol.trim().is_empty())
                .collect();
            let skip: std::collections::HashSet<(String, usize)> = skip_uses
                .iter()
                .filter_map(|entry| {
                    let (file, line) = entry.trim().rsplit_once(':')?;
                    Some((file.to_string(), line.parse().ok()?))
                })
                .collect();
            let mut lookup = |shown: &[(usize, usize)], cap: usize| {
                deps::uses(&path, &text, shown, &skip, cap, &mut |candidate| {
                    store
                        .read_path(&repo, candidate)
                        .ok()
                        .flatten()
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                })
            };
            print!(
                "{}",
                outline::render_show(
                    &lines,
                    &outline::ShowRequest {
                        repo: &repo,
                        path: &path,
                        range: (
                            start,
                            show_end(
                                lines.len(),
                                start,
                                from,
                                to,
                                // --full is a request for all of it, and a
                                // plain read is the whole file: past the
                                // read cap it becomes the file's outline.
                                limit.or((full || (from.is_none() && to.is_none()))
                                    .then_some(usize::MAX)),
                            ),
                        ),
                        ranged: from.is_some() || to.is_some(),
                        symbols: &symbols,
                        outline,
                        full,
                        no_comments,
                        max_tokens,
                        uses: !no_uses,
                        json,
                    },
                    &mut lookup,
                )?
            );
            if json {
                println!();
            }
        }

        Command::Files {
            repo,
            path,
            limit,
            long,
            json,
        } => {
            check_positive("--limit", limit)?;
            let repo = resolve_repo(&store, &repo)?;
            let glob = path.as_deref().map(search::path_filter).transpose()?;
            // Text is read by a person, so it is capped; JSON is read by a
            // program that asked for the list, so it gets the list. A --path
            // already narrows the list, so it lifts the cap.
            let cap = limit.unwrap_or(if json || glob.is_some() {
                usize::MAX
            } else if long {
                200
            } else {
                1000
            });
            let mut paths = store.list_files(&repo, usize::MAX)?;
            if paths.is_empty() {
                bail!(
                    "no files indexed for {repo}; `steroids index --refilter` removes empty repositories"
                );
            }
            if let Some(glob) = &glob {
                paths.retain(|(path, _, _)| search::glob_matches(glob, path));
                if paths.is_empty() {
                    bail!(
                        "no indexed file in {repo} matches --path {}; drop it to see every file",
                        path.as_deref().unwrap_or_default()
                    );
                }
            }
            let total = paths.len();
            paths.truncate(cap);
            let version = store
                .list_repos()?
                .into_iter()
                .find(|summary| summary.name == repo)
                .map(|summary| snapshot_label(&summary))
                .unwrap_or_default();
            if json {
                let files: Vec<serde_json::Value> = paths
                    .iter()
                    .map(|(path, language, size)| {
                        serde_json::json!({ "path": path, "language": language, "size": size })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "repo": repo,
                        "snapshot": version,
                        "count": files.len(),
                        "total": total,
                        "files": files,
                    })
                );
                return Ok(());
            }
            // --long is tab separated: `cut -f1` and `awk` read it without
            // guessing at column widths. The default names each directory
            // once, a third of the tokens. The summary goes to stderr so it
            // never lands in a pipe.
            if long {
                for (path, language, size) in &paths {
                    println!("{path}\t{language}\t{size}");
                }
            } else {
                let listing = render::grouped_paths(paths.iter().map(|(path, _, _)| path.as_str()));
                // Past the read cap, the directories are the map: each
                // with its count, and the reader asks for the one it needs.
                if listing.len() > outline::READ_CAP_CHARS && limit.is_none() {
                    print!(
                        "{}",
                        render::directory_counts(paths.iter().map(|(path, _, _)| path.as_str()))
                    );
                    eprintln!(
                        "{total} files in {repo} at {version}: too many to list, so these are its directories; ask for one directory's files (path)"
                    );
                    return Ok(());
                }
                print!("{listing}");
            }
            eprintln!(
                "{} of {total} files shown for {repo} at {version}{}",
                paths.len(),
                if paths.len() < total {
                    " (narrow by path, or raise the limit)"
                } else {
                    ""
                }
            );
        }

        Command::Repos {
            filter,
            tag,
            json,
            limit,
        } => {
            check_positive("--limit", limit)?;
            let tag = nonempty_tag(tag)?;
            let mut all = store.repos_tagged(tag.as_deref())?;
            if let Some(filter) = filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
                let needle = filter.to_lowercase();
                all.retain(|summary| summary.name.to_lowercase().contains(&needle));
                if all.is_empty() {
                    bail!(
                        "no indexed repository name contains '{filter}'; `steroids add owner/name` indexes one"
                    );
                }
            }
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
                            "version": summary.version,
                            "commit": summary.commit_sha,
                            "indexed_at": summary.indexed_at,
                            "files": summary.files,
                            "language": summary.language,
                            "disk_bytes": summary.disk_bytes,
                            "source_bytes": summary.source_bytes,
                            "last_commit": summary.pushed_at,
                            "tags": summary.tags.split(',').filter(|t| !t.is_empty())
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "count": all.len(),
                        "shown": rows.len(),
                        "repositories": items,
                    })
                );
            } else if rows.is_empty() {
                println!("  no repositories yet: steroids add owner/name");
            } else {
                for summary in rows {
                    println!(
                        "  {:<40} {:<12} {:>5} files  {:>8}  {:<20}  {}",
                        link(&summary.name, 40),
                        summary.language,
                        summary.files,
                        human(summary.disk_bytes as f64),
                        snapshot_label(summary),
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
                    println!(
                        "  {:<16} {:<28} where the corpus lives (absolute path, or 'default')",
                        "root",
                        root.display()
                    );
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

        Command::Changes {
            repo,
            version,
            list,
            full,
            pattern,
            max_tokens,
            json,
        } => {
            check_token_budget(max_tokens)?;
            let matching = pattern
                .as_deref()
                .map(|pattern| {
                    regex::RegexBuilder::new(pattern)
                        .case_insensitive(true)
                        .size_limit(1 << 20)
                        .build()
                })
                .transpose()
                .map_err(|error| anyhow::anyhow!("--pattern is not a valid regex: {error}"))?;
            let repo = resolve_repo(&store, &repo)?;
            let Some((path, text)) = store.changelog(&repo)? else {
                bail!(
                    "no changelog stored for {repo}: it has none at its root, or it was indexed \
                     before steroids kept them (`steroids add {repo}` re-fetches it)"
                );
            };
            println!(
                "{}",
                changelog::render(
                    &text,
                    &changelog::Request {
                        repo: &repo,
                        path: &path,
                        version: version.as_deref(),
                        list,
                        full,
                        matching,
                        max_tokens,
                        json,
                    },
                )?
                .trim_end()
            );
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
        bulk::never(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_to_is_not_capped_by_the_default_limit() {
        // The benchmark case: 1..560 of a 1201-line file used to stop at 400.
        assert_eq!(show_end(1201, 1, Some(1), Some(560), None), 560);
        assert_eq!(show_end(1201, 1, Some(1), Some(560), Some(100)), 100);
        assert_eq!(show_end(1201, 1, None, None, None), SHOW_LIMIT);
        assert_eq!(show_end(1201, 10, Some(10), None, None), 130);
        assert_eq!(show_end(50, 1, Some(1), Some(560), None), 50);
        assert_eq!(show_end(50, usize::MAX, Some(usize::MAX), None, None), 50);
    }

    #[test]
    fn snapshot_names_the_release_when_known() {
        let mut summary = store::RepoSummary {
            name: "axios/axios".into(),
            commit_sha: "fb88a1b133c1a2d774713070b8ca25f7bcd382ac".into(),
            indexed_at: String::new(),
            files: 0,
            source_bytes: 0,
            disk_bytes: 0,
            pushed_at: String::new(),
            tags: String::new(),
            language: String::new(),
            version: "1.21.0".into(),
        };
        assert_eq!(snapshot_label(&summary), "v1.21.0 (fb88a1b1)");
        summary.version.clear();
        assert_eq!(snapshot_label(&summary), "fb88a1b1");
    }
}
