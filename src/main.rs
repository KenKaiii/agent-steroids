//! A local code corpus for coding agents.
//!
//! One binary, no runtime, no daemon: an agent calls `steroids search` the
//! same way it calls `grep`.

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

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use render::{render_empty, render_matches};
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
    /// Build the trigram index. Run after any add.
    Index,
    /// Re-fetch every indexed repository at its latest commit
    Update,
    /// Regex search across every indexed repository
    Search {
        pattern: String,
        #[arg(long)]
        repo: Option<String>,
        /// Only repositories carrying this label
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        language: Option<String>,
        /// Path glob, e.g. 'src/**/*.py'
        #[arg(long)]
        path: Option<String>,
        #[arg(short, long)]
        ignore_case: bool,
        /// Include hits inside comments and docstrings
        #[arg(long)]
        include_comments: bool,
        /// Lines of code shown either side of a match. Raise it when comparing
        /// how several projects implement the same thing.
        #[arg(short = 'C', long, default_value_t = search::DEFAULT_CONTEXT_LINES)]
        context: usize,
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
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Print a full source file from the corpus
    Show { repo: String, path: String },
    /// List the files indexed for one repository
    Files {
        repo: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
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
        /// Labels to apply. Omit to list every tag in use.
        #[arg(long, value_delimiter = ',')]
        add: Vec<String>,
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

fn human(bytes: f64) -> String {
    for (unit, scale) in [("GB", 1e9), ("MB", 1e6), ("KB", 1e3)] {
        if bytes >= scale {
            return format!("{:.1}{unit}", bytes / scale);
        }
    }
    format!("{bytes:.0}B")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let parallel = cli.parallel;
    let root = corpus_root(cli.root);

    let Some(command) = cli.command else {
        return tui::run(root.clone(), Store::open(&root)?);
    };

    // Commands that write must hold the corpus lock for their whole run, so a
    // second ingest waits instead of corrupting the shared dictionary.
    let writes = matches!(
        command,
        Command::Add { .. }
            | Command::Update
            | Command::Index
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
            )?;
            // Label whatever landed, so a partly failed batch is still tagged.
            if !tag.is_empty() {
                for name in &names {
                    if let Ok(repo) = fetch::normalize_repo(name) {
                        store.tag_repo(&repo, &tag)?;
                    }
                }
                eprintln!("  tagged: {}", tag.join(", "));
            }
            if failures > 0 {
                std::process::exit(1);
            }
            eprintln!("  next: steroids index");
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
            if ingest_all(&mut store, &names, false, parallel, &known)? > 0 {
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
                ) {
                    Ok(found) => {
                        let fresh: Vec<String> = found
                            .into_iter()
                            .map(|candidate| candidate.repo)
                            .filter(|repo| !existing.contains(repo))
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
            eprintln!("  next: steroids index");
        }

        Command::Index => {
            let stats = index::build(&mut store, &mut |done, total| {
                eprint!("\r  indexing {done}/{total}");
                let _ = std::io::stderr().flush();
            })?;
            eprintln!(
                "\r  {} documents, {} trigrams stored ({} too common)   ",
                stats.documents,
                stats.trigrams_stored,
                stats.trigrams_seen - stats.trigrams_stored
            );
        }

        Command::Search {
            pattern,
            repo,
            tag,
            language,
            path,
            ignore_case,
            include_comments,
            context,
            limit,
            json,
        } => {
            let query = Query {
                repo: repo.as_deref(),
                tag: tag.as_deref(),
                language: language.as_deref(),
                path_glob: path.as_deref(),
                ignore_case,
                skip_comments: !include_comments,
                // Capped: a huge value would paste whole files into a reply.
                context_lines: context.min(40),
                ..Query::new(limit)
            };
            // A filter that excludes everything is not a failed search, and
            // blaming the pattern sends the agent rewriting a query that was
            // never the problem. Check the filters before running it.
            if let Some(repo) = &repo
                && !store.list_repos()?.iter().any(|r| &r.name == repo)
            {
                let count = store.list_repos()?.len();
                println!(
                    "'{repo}' is not in this corpus, so no search can match it. \
                     {count} repositories are indexed; run `steroids repos` to see \
                     them, or `steroids add {repo}` to index this one."
                );
                return Ok(());
            }
            if let Some(language) = &language {
                // Every language in the indexed files, not just each
                // repository's main one: a Python project can still hold the
                // shell or SQL being searched for.
                let known: std::collections::BTreeSet<String> =
                    store.languages()?.into_iter().collect();
                if !known.contains(&language.to_lowercase()) {
                    println!(
                        "No {language} files are indexed, so no search can match. \
                         Languages present: {}. Drop --language, or index a \
                         {language} project with \
                         `steroids discover 'language:{language}' --add`.",
                        known.into_iter().collect::<Vec<_>>().join(", ")
                    );
                    return Ok(());
                }
            }
            if let Some(tag) = &tag
                && store.repos_tagged(Some(tag))?.is_empty()
            {
                let known = store.tag_counts()?;
                println!(
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
                return Ok(());
            }
            let matches = search::search(&mut store, &pattern, &query)?;
            if json {
                let facts;
                println!(
                    "{}",
                    if matches.is_empty() {
                        facts = search::diagnose(&mut store, &pattern)?;
                        render::render_empty_json(&facts, &pattern)
                    } else {
                        render::render_matches_json(&matches, &pattern)
                    }
                );
            } else if matches.is_empty() {
                println!("{}", render_empty(&search::diagnose(&mut store, &pattern)?));
            } else {
                print!(
                    "{}",
                    render_matches(
                        &matches,
                        &format!(
                            "{} match(es) for /{pattern}/{}",
                            matches.len(),
                            if matches.more_available {
                                ", more available (raise --limit or narrow the search)"
                            } else {
                                ""
                            }
                        )
                    )
                );
            }
        }

        Command::Define {
            symbol,
            tag,
            language,
            limit,
            json,
        } => {
            let escaped = regex::escape(&symbol);
            // Definition syntax across the indexed languages, widest first.
            // A definition is the keyword, then the name as a whole word. The
            // leading \b matters: without it `define ToolCallResult` also
            // matches `parseToolCallResult`, which is a use, not a definition.
            //
            // The second branch covers languages that assign instead of
            // declaring, like `const Foo = (` or `Bar := func(`. Both branches
            // anchor the name on the left as well as the right.
            let pattern = format!(
                r"\b(def|class|func|fn|type|struct|interface|impl|enum|trait|const|var|let|export|public)\s+(async\s+)?\b{escaped}\b|\b{escaped}\s*(=|:=)\s*(function|async|\(|\{{|class)"
            );
            let query = Query {
                tag: tag.as_deref(),
                language: language.as_deref(),
                ..Query::new(limit)
            };
            let matches = search::search(&mut store, &pattern, &query)?;
            if json {
                let facts;
                println!(
                    "{}",
                    if matches.is_empty() {
                        facts = search::diagnose(&mut store, &escaped)?;
                        render::render_empty_json(&facts, &symbol)
                    } else {
                        render::render_matches_json(&matches, &symbol)
                    }
                );
            } else if matches.is_empty() {
                println!("{}", render_empty(&search::diagnose(&mut store, &escaped)?));
            } else {
                print!(
                    "{}",
                    render_matches(
                        &matches,
                        &format!("{} definition(s) of '{symbol}'", matches.len())
                    )
                );
            }
        }

        Command::Show { repo, path } => match store.read_path(&repo, &path)? {
            Some(content) => {
                println!("# {repo}/{path}\n");
                print!("{}", String::from_utf8_lossy(&content));
            }
            None => {
                println!("Not in corpus: {repo}/{path}");
                std::process::exit(1);
            }
        },

        Command::Files { repo, limit } => {
            let paths = store.list_files(&repo, limit)?;
            if paths.is_empty() {
                println!("No files indexed for {repo}. Check `steroids repos`.");
                std::process::exit(1);
            }
            for (path, language, size) in &paths {
                println!("  {path:<70} {language:<12} {}", human(*size as f64));
            }
            println!("\n  {} files shown for {repo}", paths.len());
        }

        Command::Repos { tag, json } => {
            let rows = store.repos_tagged(tag.as_deref())?;
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
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if rows.is_empty() {
                println!("  no repositories yet: steroids add owner/name");
            } else {
                for summary in &rows {
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
                println!("\n  {} repositories in {}", rows.len(), root.display());
            }
        }

        Command::Remove { repo } => {
            if store.remove_repo(&repo)? {
                println!("  removed {repo}");
                eprintln!("  next: steroids index");
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

            let found = if trending {
                discover::trending(days, language.as_deref(), min_stars, limit)?
            } else {
                let mut text = query.unwrap_or_else(|| settings.discover_query.clone());
                if let Some(language) = &language {
                    text.push_str(&format!(" language:{language}"));
                }
                discover::search(&text, min_stars, settings.max_age_months, limit)?
            };

            // Never re-fetch what is already indexed.
            let existing: std::collections::HashSet<String> = store
                .list_repos()?
                .into_iter()
                .map(|summary| summary.name)
                .collect();
            let fresh: Vec<_> = found
                .into_iter()
                .filter(|candidate| !existing.contains(&candidate.repo))
                .collect();

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
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if fresh.is_empty() {
                println!("  nothing new found");
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
                    ingest_all(&mut store, &names, false, parallel, &Default::default())?;
                // Label what landed, so a discovery run is immediately
                // searchable as a group.
                if !tag.is_empty() {
                    for name in &names {
                        if let Ok(repo) = fetch::normalize_repo(name) {
                            store.tag_repo(&repo, &tag)?;
                        }
                    }
                    eprintln!("  tagged: {}", tag.join(", "));
                }
                eprintln!("  next: steroids index");
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
            eprintln!("  next: steroids index");
        }

        Command::Config { key, value } => {
            let mut settings = config::Config::load(&store)?;
            match (key, value) {
                (None, _) => {
                    for (key, help) in config::KEYS {
                        println!("  {key:<16} {:<28} {help}", settings.get(key));
                    }
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
            let repos: Vec<String> = match &repo {
                Some(one) => vec![fetch::normalize_repo(one)?],
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

        Command::Tag { add, repos } => {
            // A tag that is only whitespace stores nothing, so reporting
            // success would be a lie.
            let add: Vec<String> = add
                .into_iter()
                .filter(|tag| !tag.trim().is_empty())
                .collect();
            if add.is_empty() {
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
            let targets: Vec<String> = if repos.is_empty() {
                store.list_repos()?.into_iter().map(|r| r.name).collect()
            } else {
                repos
                    .iter()
                    .map(|r| fetch::normalize_repo(r))
                    .collect::<Result<_>>()?
            };
            let mut tagged = 0;
            for name in &targets {
                if store.tag_repo(name, &add)? {
                    tagged += 1;
                } else {
                    eprintln!("  not in corpus: {name}");
                }
            }
            println!("  tagged {tagged} repositories: {}", add.join(", "));
        }

        Command::Compact => {
            let reclaimed = store.compact()?;
            println!("  reclaimed {}", human(reclaimed as f64));
            eprintln!("  next: steroids index");
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
    }
    Ok(())
}

/// Fetch many repositories at once, reporting failures without aborting.
///
/// Ingest is network-bound, so downloads run across threads; the store is
/// still written from one. Returns how many failed.
fn ingest_all(
    store: &mut Store,
    names: &[String],
    include_tests: bool,
    parallel: usize,
    known: &std::collections::HashMap<String, String>,
) -> Result<usize> {
    // Per-repository lines are useful for a handful and noise for hundreds.
    let terse = names.len() > 8;
    let outcome = bulk::ingest_all(
        store,
        names,
        include_tests,
        parallel,
        known,
        &mut |name, result, done, total| match result {
            Ok(prepared) => {
                if terse {
                    eprint!("\r  {done}/{total} fetched…        ");
                    let _ = std::io::stderr().flush();
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
            Err(error) => eprintln!("\r  {name}: FAILED ({error})              "),
        },
    )?;

    // Refusing a file silently would leave the user believing they indexed
    // something they did not, so always say what was dropped and why.
    if !outcome.rejected.is_empty() {
        eprintln!(
            "\r  skipped {} file(s) containing hidden characters:",
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
        eprint!("\r                                   \r");
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
    Ok(outcome.failed.len())
}
