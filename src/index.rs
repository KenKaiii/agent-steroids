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
pub fn build(store: &mut Store, progress: &mut dyn FnMut(usize, usize)) -> Result<IndexStats> {
    store.db.execute("DELETE FROM postings", [])?;

    let doc_ids: Vec<i64> = store
        .db
        .prepare("SELECT id FROM documents WHERE offset >= 0 ORDER BY id")?
        .query_map([], |row| row.get(0))?
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

    let total = doc_ids.len();
    let cutoff = ((total as f64 * MAX_DOCUMENT_FRACTION) as usize).max(1);
    let mut stored = 0usize;
    let mut dropped: Vec<u8> = Vec::new();

    let transaction = store.db.unchecked_transaction()?;
    {
        let mut insert = transaction
            .prepare("INSERT OR REPLACE INTO postings (trigram, doc_ids) VALUES (?1, ?2)")?;
        for (gram, ids) in &postings {
            if ids.len() > cutoff {
                // Record trigrams dropped for being too common. Without this, a
                // lookup miss is ambiguous: absent from the corpus, or merely
                // not worth indexing.
                dropped.extend_from_slice(gram);
                continue;
            }
            insert.execute(params![gram.as_slice(), encode(ids)?])?;
            stored += 1;
        }
    }
    transaction.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('stop_trigrams', ?1)",
        params![dropped],
    )?;
    transaction.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('doc_count', ?1)",
        params![total.to_string().into_bytes()],
    )?;
    transaction.commit()?;
    // A Store queried before this rebuild holds the previous stop list.
    store.invalidate_caches();

    Ok(IndexStats {
        documents: total,
        trigrams_seen: postings.len(),
        trigrams_stored: stored,
    })
}

#[cfg(test)]
mod tests {
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
