//! Background work.
//!
//! Fetching a repository or rebuilding the index takes seconds. Both run on a
//! worker thread and report progress by channel, so the draw loop never blocks.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;

use crate::store::Store;
use crate::{bulk, index};

pub enum Msg {
    /// Human-readable progress for the status line.
    Progress(String),
    /// The job finished; the string is the outcome to show.
    Done(String),
    Failed(String),
}

/// Ingest repositories, then rebuild the index.
///
/// A worker cannot share the caller's `Store`: rusqlite connections are not
/// `Sync`, so it opens its own handle against the same directory.
pub fn add_repos(root: PathBuf, names: Vec<String>, tx: Sender<Msg>) {
    thread::spawn(move || {
        let mut store = match Store::open(&root) {
            Ok(store) => store,
            Err(error) => {
                let _ = tx.send(Msg::Failed(format!("cannot open corpus: {error}")));
                return;
            }
        };

        // One bad name must not strand the repositories already fetched: they
        // are on disk but invisible until the index is rebuilt.
        let progress = tx.clone();
        let outcome = bulk::ingest_all(
            &mut store,
            &names,
            false,
            true,
            bulk::DEFAULT_PARALLEL,
            &Default::default(),
            &mut |name, result, done, total| {
                let _ = progress.send(Msg::Progress(match result {
                    Ok(prepared) => {
                        format!("{done}/{total}  {name}: {} files", prepared.files.len())
                    }
                    Err(note) => format!("{done}/{total}  {name}: {note}"),
                }));
            },
        );
        let (added, failure) = match outcome {
            Ok(outcome) => (
                outcome.added,
                outcome
                    .failed
                    .first()
                    .map(|(name, error)| format!("{name}: {error}")),
            ),
            Err(error) => {
                let _ = tx.send(Msg::Failed(error.to_string()));
                return;
            }
        };

        if added > 0
            && let Err(error) = rebuild(&mut store, &tx)
        {
            let _ = tx.send(Msg::Failed(format!("indexing failed: {error}")));
            return;
        }
        let _ = match failure {
            Some(reason) if added == 0 => tx.send(Msg::Failed(reason)),
            Some(reason) => tx.send(Msg::Failed(format!("added {added}, but {reason}"))),
            None => tx.send(Msg::Done(format!(
                "added {added} repositor{}",
                if added == 1 { "y" } else { "ies" }
            ))),
        };
    });
}

/// Re-fetch every indexed repository, reclaim orphaned bytes, reindex.
pub fn update_all(root: PathBuf, tx: Sender<Msg>) {
    thread::spawn(move || {
        let mut store = match Store::open(&root) {
            Ok(store) => store,
            Err(error) => {
                let _ = tx.send(Msg::Failed(format!("cannot open corpus: {error}")));
                return;
            }
        };
        let summaries = match store.list_repos() {
            Ok(rows) => rows,
            Err(error) => {
                let _ = tx.send(Msg::Failed(error.to_string()));
                return;
            }
        };
        // Skip anything upstream has not moved, rather than re-downloading it.
        let known: std::collections::HashMap<String, String> = summaries
            .iter()
            .map(|s| (s.name.clone(), s.commit_sha.clone()))
            .collect();
        let names: Vec<String> = match Ok::<_, anyhow::Error>(summaries) {
            Ok(rows) => rows.into_iter().map(|summary| summary.name).collect(),
            Err(error) => {
                let _ = tx.send(Msg::Failed(error.to_string()));
                return;
            }
        };
        if names.is_empty() {
            let _ = tx.send(Msg::Done("nothing to update".into()));
            return;
        }

        // A repository that has been deleted or renamed upstream must not stop
        // the rest from updating.
        let progress = tx.clone();
        let failed: Vec<String> = match bulk::ingest_all(
            &mut store,
            &names,
            false,
            true,
            bulk::DEFAULT_PARALLEL,
            &known,
            &mut |name, _, done, total| {
                let _ = progress.send(Msg::Progress(format!("updating {done}/{total}  {name}")));
            },
        ) {
            Ok(outcome) => outcome.failed.into_iter().map(|(name, _)| name).collect(),
            Err(error) => {
                let _ = tx.send(Msg::Failed(error.to_string()));
                return;
            }
        };

        let _ = tx.send(Msg::Progress("reclaiming space…".into()));
        // Replaced content orphans its old bytes, so reclaim before reindexing.
        if let Err(error) = store.compact() {
            let _ = tx.send(Msg::Failed(format!("compaction failed: {error}")));
            return;
        }
        if let Err(error) = rebuild(&mut store, &tx) {
            let _ = tx.send(Msg::Failed(format!("indexing failed: {error}")));
            return;
        }
        let updated = names.len() - failed.len();
        let _ = if failed.is_empty() {
            tx.send(Msg::Done(format!("updated {updated} repositories")))
        } else {
            tx.send(Msg::Failed(format!(
                "updated {updated}, could not reach: {}",
                failed.join(", ")
            )))
        };
    });
}

/// Drop a repository, then reindex so its files stop appearing in results.
pub fn remove_repo(root: PathBuf, name: String, tx: Sender<Msg>) {
    thread::spawn(move || {
        let mut store = match Store::open(&root) {
            Ok(store) => store,
            Err(error) => {
                let _ = tx.send(Msg::Failed(format!("cannot open corpus: {error}")));
                return;
            }
        };
        let _ = tx.send(Msg::Progress(format!("removing {name}…")));
        match store.remove_repo(&name) {
            Ok(false) => {
                let _ = tx.send(Msg::Failed(format!("{name} is not in the corpus")));
                return;
            }
            Err(error) => {
                let _ = tx.send(Msg::Failed(error.to_string()));
                return;
            }
            Ok(true) => {}
        }
        if let Err(error) = store.compact() {
            let _ = tx.send(Msg::Failed(format!("compaction failed: {error}")));
            return;
        }
        if let Err(error) = rebuild(&mut store, &tx) {
            let _ = tx.send(Msg::Failed(format!("indexing failed: {error}")));
            return;
        }
        let _ = tx.send(Msg::Done(format!("removed {name}")));
    });
}

fn rebuild(store: &mut Store, tx: &Sender<Msg>) -> anyhow::Result<()> {
    index::build(store, &mut |done, total| {
        let _ = tx.send(Msg::Progress(format!("indexing {done}/{total}")));
    })?;
    Ok(())
}
