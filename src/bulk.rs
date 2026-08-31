//! Parallel ingest.
//!
//! Ingesting a repository is almost entirely network wait: downloading and
//! unpacking a tarball costs milliseconds of CPU against seconds of transfer.
//! So fetch many at once on worker threads, and hand the decoded files to a
//! single writer that owns the store.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::sync_channel;
use std::thread;

use anyhow::Result;

use crate::fetch::{self, PreparedRepo};
use crate::store::Store;

/// Concurrent downloads. GitHub's documented guidance is to avoid many
/// concurrent requests; this is brisk without being abusive, and the job is
/// bandwidth-bound well before it is connection-bound.
pub const DEFAULT_PARALLEL: usize = 8;
/// Prepared repositories held in memory awaiting the writer. Each is a decoded
/// repository's worth of source, so this bounds peak memory alongside the
/// worker count: roughly (parallel + QUEUE_DEPTH) repositories in flight.
const QUEUE_DEPTH: usize = 2;

/// Called as each repository lands: (name, result, done, total).
pub type Progress<'a> = &'a mut dyn FnMut(&str, Result<&PreparedRepo, &str>, usize, usize);

pub struct BulkOutcome {
    pub added: usize,
    pub failed: Vec<(String, String)>,
    pub files: usize,
    pub bytes: u64,
}

/// Fetch every repository, writing each as it arrives.
///
/// `report` is called on the calling thread as each repository lands, so the
/// caller decides how to display progress.
pub fn ingest_all(
    store: &mut Store,
    names: &[String],
    include_tests: bool,
    with_metadata: bool,
    parallel: usize,
    report: Progress<'_>,
) -> Result<BulkOutcome> {
    let total = names.len();
    if total == 0 {
        return Ok(BulkOutcome {
            added: 0,
            failed: Vec::new(),
            files: 0,
            bytes: 0,
        });
    }

    // A repeated name would download and write the same repository twice.
    let mut seen = std::collections::HashSet::new();
    let names: Vec<String> = names
        .iter()
        .filter(|name| seen.insert(name.as_str()))
        .cloned()
        .collect();
    let total = names.len();
    let names = &names;

    let parallel = parallel.clamp(1, 32).min(total);
    let cursor = AtomicUsize::new(0);
    // Bounded: without a cap, fast downloads would queue every repository's
    // decoded source in memory while the writer works through them.
    let (tx, rx) = sync_channel::<(String, Result<PreparedRepo, String>)>(QUEUE_DEPTH);

    let outcome = thread::scope(|scope| -> Result<BulkOutcome> {
        for _ in 0..parallel {
            let tx = tx.clone();
            let cursor = &cursor;
            scope.spawn(move || {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(name) = names.get(index) else {
                        return;
                    };
                    let prepared = fetch::prepare(name, include_tests, with_metadata)
                        .map_err(|error| error.to_string());
                    // A closed receiver means the writer is gone; stop early.
                    if tx.send((name.clone(), prepared)).is_err() {
                        return;
                    }
                }
            });
        }
        // The workers hold the only senders now, so the loop below ends when
        // the last of them finishes.
        drop(tx);

        let mut outcome = BulkOutcome {
            added: 0,
            failed: Vec::new(),
            files: 0,
            bytes: 0,
        };
        let mut done = 0usize;
        for (name, prepared) in rx {
            done += 1;
            match prepared {
                Ok(repo) => {
                    // Writing is serialised here: rusqlite connections are not
                    // Sync, and it is the cheap half of the work anyway.
                    match fetch::commit(&repo, store) {
                        Ok(()) => {
                            outcome.added += 1;
                            outcome.files += repo.files.len();
                            outcome.bytes += repo.bytes_kept;
                            report(&name, Ok(&repo), done, total);
                        }
                        // One repository failing to write must not discard the
                        // hundreds already stored in this batch.
                        Err(error) => {
                            let error = error.to_string();
                            report(&name, Err(&error), done, total);
                            outcome.failed.push((name, error));
                        }
                    }
                }
                Err(error) => {
                    report(&name, Err(&error), done, total);
                    outcome.failed.push((name, error));
                }
            }
        }
        Ok(outcome)
    })?;

    store.flush_pending()?;
    Ok(outcome)
}
