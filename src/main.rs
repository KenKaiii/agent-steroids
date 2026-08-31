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
        /// Also record stars and last-commit date, which `decay` needs.
        /// Costs one rate-limited API call per repository.
        #[arg(long)]
        metadata: bool,
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
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Find where a function, class or constant is defined
    Define {
        symbol: String,
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
    /// List indexed repositories
    Repos {
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
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".steroids"),
        None => PathBuf::from("./corpus-data"),
    }
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
    let mut store = Store::open(&root)?;

    let Some(command) = cli.command else {
        return tui::run(root, store);
    };

    match command {
        Command::Add {
            repos,
            from_file,
            include_tests,
            metadata,
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
            if ingest_all(&mut store, &names, include_tests, metadata, parallel)? > 0 {
                std::process::exit(1);
            }
            eprintln!("  next: steroids index");
        }

        Command::Update => {
            let names: Vec<String> = store
                .list_repos()?
                .into_iter()
                .map(|summary| summary.name)
                .collect();
            if names.is_empty() {
                println!("  nothing to update: steroids add owner/name");
                return Ok(());
            }
            // Re-adding replaces the previous copy, so this is just an ingest
            // of everything already tracked.
            if ingest_all(&mut store, &names, false, true, parallel)? > 0 {
                std::process::exit(1);
            }

            let settings = config::Config::load(&store)?;
            if settings.auto_discover {
                let existing: std::collections::HashSet<String> = names.into_iter().collect();
                match discover::search(
                    &settings.discover_query,
                    settings.min_stars,
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
                            ingest_all(&mut store, &fresh, false, false, parallel)?;
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
            language,
            path,
            ignore_case,
            include_comments,
            limit,
            json,
        } => {
            let query = Query {
                repo: repo.as_deref(),
                language: language.as_deref(),
                path_glob: path.as_deref(),
                ignore_case,
                skip_comments: !include_comments,
                ..Query::new(limit)
            };
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
                        &format!("{} match(es) for /{pattern}/", matches.len())
                    )
                );
            }
        }

        Command::Define {
            symbol,
            language,
            limit,
            json,
        } => {
            let escaped = regex::escape(&symbol);
            // Definition syntax across the indexed languages, widest first.
            let pattern = format!(
                r"(def|class|func|fn|type|struct|interface|impl|const|var|let)\s+{escaped}\b|{escaped}\s*(=|:=)\s*(function|async|\()"
            );
            let query = Query {
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

        Command::Repos { json } => {
            let rows = store.list_repos()?;
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
                        summary.name,
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
                discover::search(&text, min_stars, limit)?
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
                let failures = ingest_all(&mut store, &names, false, false, parallel)?;
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
            if months == 0 {
                println!("  decay is off. Enable with: steroids config decay_months 6");
                return Ok(());
            }

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // Months as 30 days: exact calendar months are not worth a date
            // dependency for a threshold the user picks in round numbers.
            let cutoff = discover::iso_date(now.saturating_sub(months as u64 * 30 * 86_400));
            let stale = store.stale_repos(&cutoff, settings.decay_archived)?;

            if stale.is_empty() {
                println!("  nothing older than {months} months (cutoff {cutoff})");
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
    with_metadata: bool,
    parallel: usize,
) -> Result<usize> {
    // Per-repository lines are useful for a handful and noise for hundreds.
    let terse = names.len() > 8;
    let outcome = bulk::ingest_all(
        store,
        names,
        include_tests,
        with_metadata,
        parallel,
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
            Err(error) => eprintln!("\r  {name}: FAILED ({error})              "),
        },
    )?;

    if terse {
        eprint!("\r                                   \r");
        println!(
            "  {} repositories, {} files, {}",
            outcome.added,
            outcome.files,
            human(outcome.bytes as f64)
        );
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
    Ok(outcome.failed.len())
}
