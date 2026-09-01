//! Trigram index.
//!
//! Non-positional on purpose: storing only "which documents contain this
//! trigram" costs a few percent of corpus size, where a positional index costs
//! several times it. Queries use the index to pick candidate documents, then
//! verify each with a real regex.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::params;

use crate::store::Store;

/// A trigram present in most documents excludes nothing, so storing it is pure
/// overhead at query time and on disk.
const MAX_DOCUMENT_FRACTION: f64 = 0.35;

/// Every 3-byte window of non-NUL content.
pub fn trigrams(content: &[u8]) -> HashSet<[u8; 3]> {
    content
        .windows(3)
        .filter(|window| !window.contains(&0))
        .map(|window| [window[0], window[1], window[2]])
        .collect()
}

/// Delta + varint, then zstd. Sorted ids delta down to tiny numbers.
fn encode(doc_ids: &[i64]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(doc_ids.len() * 2);
    let mut previous = 0i64;
    for doc_id in doc_ids {
        let mut delta = (doc_id - previous) as u64;
        previous = *doc_id;
        while delta >= 0x80 {
            out.push((delta as u8 & 0x7F) | 0x80);
            delta >>= 7;
        }
        out.push(delta as u8);
    }
    Ok(zstd::bulk::compress(&out, 10)?)
}

pub fn decode(blob: &[u8]) -> Result<Vec<i64>> {
    let raw = zstd::decode_all(blob)?;
    let mut doc_ids = Vec::new();
    let (mut current, mut shift, mut previous) = (0u64, 0u32, 0i64);
    for byte in raw {
        current |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 != 0 {
            shift += 7;
            continue;
        }
        previous += current as i64;
        doc_ids.push(previous);
        current = 0;
        shift = 0;
    }
    Ok(doc_ids)
}

pub struct IndexStats {
    pub documents: usize,
    pub trigrams_seen: usize,
    pub trigrams_stored: usize,
}

/// (Re)build postings for every stored document.
/// Build or extend the trigram index.
///
/// Incremental by default. Document ids only ever increase, so anything added
/// since the last run has a higher id than the last one indexed, and merging
/// those in avoids re-reading documents whose trigrams cannot have changed.
/// Adding two repositories to a corpus of 600,000 documents otherwise costs a
/// full rebuild, which is nearly two minutes to index one thousand new files.
///
/// Replacing a repository leaves its old ids in the postings. Those are
/// skipped at query time, but they accumulate, so a full rebuild runs
/// automatically once enough of the index is stale.
pub fn build(store: &mut Store, progress: &mut dyn FnMut(usize, usize)) -> Result<IndexStats> {
    let indexed_upto = read_marker(store, "indexed_upto")?;
    let stale = read_marker(store, "stale_postings")?;
    let live: i64 = store.db.query_row(
        "SELECT COUNT(*) FROM documents WHERE offset >= 0",
        [],
        |row| row.get(0),
    )?;

    // Beyond this share of dead entries the index is mostly bookkeeping for
    // documents that no longer exist, and rebuilding is cheaper than carrying
    // them through every query.
    let too_stale = live > 0 && stale * 100 / live.max(1) > 40;
    let full = indexed_upto == 0 || too_stale;
    build_from(store, if full { 0 } else { indexed_upto }, full, progress)
}

/// Discard the existing index and rebuild it from every stored document.
pub fn rebuild(store: &mut Store, progress: &mut dyn FnMut(usize, usize)) -> Result<IndexStats> {
    build_from(store, 0, true, progress)
}

fn read_marker(store: &Store, key: &str) -> Result<i64> {
    let raw: Option<Vec<u8>> = store
        .db
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .ok();
    Ok(raw
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.parse().ok())
        .unwrap_or(0))
}

fn build_from(
    store: &mut Store,
    after: i64,
    full: bool,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<IndexStats> {
    // Read before the run overwrites doc_count, since the stale count compares
    // what was live at the last index against what still is.
    let stale_before = read_marker(store, "stale_postings")?;
    if full {
        store.db.execute("DELETE FROM postings", [])?;
    }

    let doc_ids: Vec<i64> = store
        .db
        .prepare("SELECT id FROM documents WHERE offset >= 0 AND id > ?1 ORDER BY id")?
        .query_map(params![after], |row| row.get(0))?
        .collect::<Result<_, _>>()?;

    let mut postings: HashMap<[u8; 3], Vec<i64>> = HashMap::new();
    for (count, doc_id) in doc_ids.iter().enumerate() {
        let content = store.read_document(*doc_id)?;
        for gram in trigrams(&content) {
            postings.entry(gram).or_default().push(*doc_id);
        }
        if count % 500 == 0 {
            progress(count, doc_ids.len());
        }
    }

    let total: i64 = store.db.query_row(
        "SELECT COUNT(*) FROM documents WHERE offset >= 0",
        [],
        |row| row.get(0),
    )?;
    let cutoff = ((total as f64 * MAX_DOCUMENT_FRACTION) as usize).max(1);
    let mut stored = 0usize;
    let mut dropped: Vec<u8> = Vec::new();

    // Trigrams already dropped for being too common stay dropped: re-adding
    // them from a small batch would claim a term is rare when the corpus knows
    // otherwise.
    let stop: std::collections::HashSet<[u8; 3]> = if full {
        Default::default()
    } else {
        read_stop_trigrams(store)?
    };

    let transaction = store.db.unchecked_transaction()?;
    {
        let mut existing =
            transaction.prepare("SELECT doc_ids FROM postings WHERE trigram = ?1")?;
        let mut insert = transaction.prepare(
            "INSERT OR REPLACE INTO postings (trigram, doc_ids, doc_count) \
             VALUES (?1, ?2, ?3)",
        )?;
        for (gram, ids) in &postings {
            if stop.contains(gram) {
                dropped.extend_from_slice(gram);
                continue;
            }
            // Merge rather than replace, so earlier documents keep their entry.
            let mut merged = if full {
                Vec::new()
            } else {
                existing
                    .query_row(params![gram.as_slice()], |row| row.get::<_, Vec<u8>>(0))
                    .ok()
                    .map(|blob| decode(&blob))
                    .transpose()?
                    .unwrap_or_default()
            };
            merged.extend_from_slice(ids);
            merged.sort_unstable();
            merged.dedup();

            if merged.len() > cutoff {
                dropped.extend_from_slice(gram);
                // A trigram that has become too common must not keep its old
                // entry, or queries would narrow on a fraction of the corpus.
                transaction.execute(
                    "DELETE FROM postings WHERE trigram = ?1",
                    params![gram.as_slice()],
                )?;
                continue;
            }
            insert.execute(params![
                gram.as_slice(),
                encode(&merged)?,
                merged.len() as i64
            ])?;
            stored += 1;
        }
    }
    if !full {
        // Keep the trigrams dropped by earlier runs alongside this run's.
        let mut all = read_stop_trigrams(store)?;
        for &gram in dropped.as_chunks::<3>().0 {
            all.insert(gram);
        }
        dropped = all.into_iter().flatten().collect();
    }
    transaction.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('stop_trigrams', ?1)",
        params![dropped],
    )?;
    let highest: i64 = store
        .db
        .query_row("SELECT COALESCE(MAX(id), 0) FROM documents", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);
    transaction.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('indexed_upto', ?1)",
        params![highest.to_string().into_bytes()],
    )?;
    transaction.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('stale_postings', ?1)",
        params![if full {
            b"0".to_vec()
        } else {
            count_stale(store, after, stale_before)?
        }],
    )?;
    transaction.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('doc_count', ?1)",
        params![total.to_string().into_bytes()],
    )?;
    transaction.commit()?;
    store.invalidate_caches();

    let total = total as usize;
    Ok(IndexStats {
        documents: total,
        trigrams_seen: postings.len(),
        trigrams_stored: stored,
    })
}

/// Trigrams the index has already rejected for being too common.
fn read_stop_trigrams(store: &Store) -> Result<std::collections::HashSet<[u8; 3]>> {
    let raw: Option<Vec<u8>> = store
        .db
        .query_row(
            "SELECT value FROM meta WHERE key = 'stop_trigrams'",
            [],
            |row| row.get(0),
        )
        .ok();
    let Some(bytes) = raw else {
        return Ok(Default::default());
    };
    let (grams, _remainder) = bytes.as_chunks::<3>();
    Ok(grams.iter().copied().collect())
}

/// Documents that were indexed but have since been replaced.
///
/// Counted rather than removed: deleting their entries means rewriting every
/// posting list that mentions them, which is the full rebuild this exists to
/// avoid. The count decides when that rebuild is finally worth doing.
///
/// Derived by comparing how many documents were live at the last index against
/// how many of those still are. Document ids are not dense, so the id itself
/// says nothing about how many exist.
fn count_stale(store: &Store, indexed_upto: i64, previous_stale: i64) -> Result<Vec<u8>> {
    let indexed_then = read_marker(store, "doc_count")?;
    let survivors: i64 = store.db.query_row(
        "SELECT COUNT(*) FROM documents WHERE offset >= 0 AND id <= ?1",
        params![indexed_upto],
        |row| row.get(0),
    )?;
    let newly_stale = (indexed_then - survivors).max(0);
    Ok((previous_stale + newly_stale).to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    /// An incremental index must find exactly what a full rebuild finds.
    /// Divergence here is silent: searches quietly stop matching documents
    /// that are present, with nothing to indicate anything is wrong.
    #[test]
    fn incremental_matches_a_full_rebuild() -> Result<()> {
        let dir = crate::store::scratch_dir("incr");
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = crate::store::Store::open_for_write(&dir)?;
        let mut noop = |_: usize, _: usize| {};

        let add = |store: &mut crate::store::Store, repo: &str, n: usize| -> Result<()> {
            let id = store.add_repo(repo, &crate::fetch::Upstream::default())?;
            for i in 0..n {
                let body = format!(
                    "fn handler_{i}() {{\n    let retry_count = {i};\n    process(retry_count);\n}}\n"
                );
                store.add_document(id, &format!("{repo}/f{i}.rs"), "rust", body.into_bytes())?;
            }
            store.flush_pending()
        };

        add(&mut store, "a/one", 40)?;
        super::build(&mut store, &mut noop)?;
        // Two more batches, each extending the index rather than rebuilding.
        add(&mut store, "b/two", 30)?;
        super::build(&mut store, &mut noop)?;
        add(&mut store, "c/three", 25)?;
        super::build(&mut store, &mut noop)?;

        let query = |store: &mut crate::store::Store, pattern: &str| -> Result<Vec<String>> {
            let mut found: Vec<String> =
                crate::search::search(store, pattern, &crate::search::Query::new(10_000))?
                    .matches
                    .iter()
                    .map(|hit| format!("{}/{}:{}", hit.repo, hit.path, hit.line_number))
                    .collect();
            found.sort();
            Ok(found)
        };

        let patterns = ["retry_count", "fn handler_1", "process\\("];
        let incremental: Vec<Vec<String>> = patterns
            .iter()
            .map(|p| query(&mut store, p))
            .collect::<Result<_>>()?;

        super::rebuild(&mut store, &mut noop)?;
        let rebuilt: Vec<Vec<String>> = patterns
            .iter()
            .map(|p| query(&mut store, p))
            .collect::<Result<_>>()?;

        for (i, pattern) in patterns.iter().enumerate() {
            assert_eq!(
                incremental[i], rebuilt[i],
                "incremental and rebuilt indexes disagree on {pattern}"
            );
            assert!(!rebuilt[i].is_empty(), "{pattern} matched nothing at all");
        }

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    use super::*;

    #[test]
    fn varint_round_trips() -> Result<()> {
        let ids = vec![1i64, 2, 300, 70_000, 70_001, 5_000_000];
        assert_eq!(decode(&encode(&ids)?)?, ids);
        Ok(())
    }

    #[test]
    fn trigrams_cover_every_window() {
        let grams = trigrams(b"abcd");
        assert_eq!(grams.len(), 2);
        assert!(grams.contains(b"abc"));
        assert!(grams.contains(b"bcd"));
        assert!(trigrams(b"ab").is_empty());
    }
}
