//! Content store: many small source files, compressed individually against a
//! shared zstd dictionary so each stays independently seekable.
//!
//! Layout on disk:
//!   <root>/corpus.db    sqlite metadata + trigram postings + the zstd dictionary
//!   <root>/blobs.bin    concatenated compressed file contents

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

// Measured on a 76MB, 12-repository ingest: level 6 produces the same 21MB on
// disk as level 12 but costs 2.9s of CPU against 34s, and level 19 costs more
// again for under 1% further saving. This CPU sits on the single writer thread,
// so anything higher makes bulk ingest CPU-bound instead of network-bound.
const COMPRESSION_LEVEL: i32 = 6;
/// Samples buffered before training the shared dictionary. More samples give a
/// better dictionary but delay the first flush.
const DICT_TRAINING_SAMPLES: usize = 2048;
/// Cap on the bytes buffered while waiting for enough dictionary samples.
/// Without it, a corpus of large files could hold 2,048 x 200KB in memory
/// before the first flush.
const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;
const DICT_SIZE_BYTES: usize = 110 * 1024;
/// Below this, zstd cannot derive a useful dictionary and fails.
const MIN_DICT_SAMPLES: usize = 8;
/// On-disk format generation, stamped into `meta.schema_version` by every
/// writer. Bump it with any change an older binary could misread, and add
/// the upgrade in `open_inner` where the stored version is compared.
pub const SCHEMA_VERSION: u32 = 1;

/// Note on `postings.doc_count`: it records how many documents a posting list
/// holds, so a query can order its intersections without decompressing
/// anything. Compressed length only correlates with the count, and two lists of
/// the same size can differ several fold in entries.
///
/// Comments live here rather than inline: statements are split on `;`, so a
/// trailing SQL comment is carried into the next statement and breaks it.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value BLOB);
CREATE TABLE IF NOT EXISTS repos (
    id INTEGER PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    commit_sha TEXT,
    indexed_at TEXT
);
-- Upstream facts, refreshed on every ingest. pushed_at drives decay: it is the
-- date of the last commit, which is what abandoned actually means, unlike
-- indexed_at, which only says when we last looked.
ALTER TABLE repos ADD COLUMN pushed_at TEXT;
ALTER TABLE repos ADD COLUMN stars INTEGER;
ALTER TABLE repos ADD COLUMN archived INTEGER;
-- Free-form labels, comma separated, so a corpus can be sliced by what a
-- repository is for, such as coding-agent or rust, not only by name.
ALTER TABLE repos ADD COLUMN tags TEXT;
CREATE TABLE IF NOT EXISTS documents (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id),
    path TEXT NOT NULL,
    language TEXT NOT NULL,
    raw_size INTEGER NOT NULL,
    offset INTEGER NOT NULL,
    length INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS documents_repo ON documents(repo_id);
CREATE INDEX IF NOT EXISTS documents_unflushed ON documents(id) WHERE offset < 0;
CREATE TABLE IF NOT EXISTS postings (
    trigram BLOB PRIMARY KEY,
    doc_ids BLOB NOT NULL
);
ALTER TABLE postings ADD COLUMN doc_count INTEGER;
ALTER TABLE repos ADD COLUMN language TEXT;
ALTER TABLE repos ADD COLUMN files INTEGER;
ALTER TABLE repos ADD COLUMN source_bytes INTEGER;
ALTER TABLE repos ADD COLUMN disk_bytes INTEGER;
";

/// What the corpus knows about one indexed repository.
#[derive(Clone)]
pub struct RepoSummary {
    pub name: String,
    pub commit_sha: String,
    pub indexed_at: String,
    pub files: i64,
    /// Uncompressed size of the indexed source.
    pub source_bytes: i64,
    /// Bytes this repository actually occupies in blobs.bin. This is the
    /// number that adds up to what the corpus costs on disk; source_bytes is
    /// several times larger and would not reconcile with the total.
    pub disk_bytes: i64,
    pub pushed_at: String,
    /// Comma-separated labels, empty when untagged.
    pub tags: String,
    /// Language holding the most indexed bytes. Derived from what was actually
    /// kept, not GitHub's label, so it reflects the code in the corpus after
    /// filtering.
    pub language: String,
}

pub struct Store {
    /// Held for the lifetime of a write session and never read: closing the
    /// handle on drop is what releases the lock.
    #[allow(dead_code)]
    lock: Option<File>,
    pub db: Connection,
    blob_path: PathBuf,
    writer: Option<File>,
    reader: Option<File>,
    dictionary: Option<Vec<u8>>,
    /// Pinned once anything is written without a dictionary: a dictionary
    /// trained later could not decode those bytes.
    dictless: bool,
    pending: Vec<(i64, Vec<u8>)>,
    /// Bytes held in `pending`, to bound memory before the first flush.
    pending_bytes: usize,
    stop_trigrams: Option<Option<HashSet<[u8; 3]>>>,
}

impl Store {
    /// Open for reading. Never blocks, even while an ingest is running.
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_inner(root, false)
    }

    /// Open for writing, waiting for any other writer to finish first.
    ///
    /// The lock must be held before the shared zstd dictionary is read, not
    /// just before the first write: two writers that each read "no dictionary
    /// yet" will each train their own, and whichever saves last leaves the
    /// other's documents undecodable.
    pub fn open_for_write(root: &Path) -> Result<Self> {
        Self::open_inner(root, true)
    }

    fn open_inner(root: &Path, for_write: bool) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("creating corpus directory {}", root.display()))?;
        // Taken before anything is read, and released when the Store drops.
        let lock = if for_write {
            // Read and write, not append: Windows refuses LockFileEx on an
            // append-only handle with "Access is denied", and the file is
            // never written to anyway. Its existence is the whole point.
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                // Never truncated: another process may be holding a lock on
                // this very file, and emptying it under them is pointless
                // since nothing is ever stored in it.
                .truncate(false)
                .open(root.join("write.lock"))?;
            // Try without waiting first, so a second writer can say what it is
            // waiting for. Blocking silently looked like a hang: an agent that
            // runs `add` while an index is rebuilding sees nothing for
            // minutes and has no way to tell a wait from a crash.
            if !lock_exclusive(&file, false)? {
                eprintln!(
                    "  another steroids process is writing to {}; waiting for it to finish",
                    root.display()
                );
                lock_exclusive(&file, true)?;
            }
            Some(file)
        } else {
            None
        };

        let path = root.join("corpus.db");
        // A WAL database needs to write sidecar files even to read, so a corpus
        // on a read-only mount or drive cannot be opened normally. Searching one
        // is legitimate, so retry read-only before giving up.
        let db = match Connection::open(&path) {
            Ok(db) if for_write || db.pragma_update(None, "user_version", 0).is_ok() => db,
            _ => Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?,
        };
        // The ALTER statements fail on an existing corpus that already has the
        // columns; SQLite has no ADD COLUMN IF NOT EXISTS, so run them
        // individually and let a duplicate-column error pass.
        // Schema setup writes, so skip it when the database is not writable.
        let writable = db.pragma_update(None, "user_version", 0).is_ok();
        for statement in SCHEMA.split(';').filter(|_| writable) {
            let statement = statement.trim();
            if statement.is_empty() {
                continue;
            }
            if let Err(error) = db.execute_batch(statement)
                && !error.to_string().contains("duplicate column")
            {
                return Err(error.into());
            }
        }
        // A corpus stamped by a newer binary is refused outright rather than
        // half-read: the self-updater's `.old` rollback and a second machine
        // on an older install both land here. Absent means pre-versioning,
        // which is format 1.
        let stored: u32 = db
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok()?.trim().parse().ok())
            .unwrap_or(1);
        if stored > SCHEMA_VERSION {
            bail!(
                "{} was written by a newer steroids (format {stored}, this binary reads {SCHEMA_VERSION}); \
                 run: steroids upgrade",
                root.display()
            );
        }
        // Migrations for `stored < SCHEMA_VERSION` go here, before the stamp,
        // and only under the write lock so two processes never race one.
        if for_write && writable {
            db.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
                params![SCHEMA_VERSION.to_string().into_bytes()],
            )?;
        }
        // Durability matters more than ingest speed here, but the default
        // rollback journal is slow for the many small writes an ingest makes.
        // WAL needs to create sidecar files, so it fails on a corpus the user
        // cannot write to: a shared read-only mount, or one on a locked drive.
        // Searching such a corpus is legitimate, so fall back rather than
        // refusing to open it at all.
        let _ = db.pragma_update(None, "journal_mode", "WAL");
        // Bound the page cache. SQLite's default grows with use; a negative
        // value means kibibytes rather than pages, so this caps the cache at
        // 8MB no matter how large the corpus becomes.
        let _ = db.pragma_update(None, "cache_size", -8_000);
        // Wait rather than failing when another process holds a write lock.
        db.busy_timeout(std::time::Duration::from_secs(30))?;

        let dictionary = db
            .query_row("SELECT value FROM meta WHERE key='zstd_dict'", [], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .ok();
        let dictless = db
            .query_row("SELECT 1 FROM meta WHERE key='dictless'", [], |_| Ok(()))
            .is_ok();

        // Writers append to blobs.bin and share one trained dictionary, so two
        // at once would leave each other's documents undecodable. Readers are
        // unaffected: this lock is only taken when writing.
        let mut store = Self {
            lock,
            db,
            blob_path: root.join("blobs.bin"),
            writer: None,
            reader: None,
            dictionary,
            dictless,
            pending: Vec::new(),
            pending_bytes: 0,
            stop_trigrams: None,
        };
        // Only a writer may repair the corpus. A reader that did so could
        // delete the temporary file of a compaction running in another
        // process, stranding its committed offsets against the old blob file
        // and destroying every document.
        if store.lock.is_some() {
            store.recover_compaction()?;
        }
        store.check_blob_integrity()?;
        store.discard_unflushed()?;
        Ok(store)
    }

    // -- writing ------------------------------------------------------------

    /// Register a repo, discarding any documents from a previous ingest.
    ///
    /// Re-adding a repo must replace it, not append a second copy. Stale blob
    /// bytes stay in blobs.bin; nothing references them.
    ///
    /// Upstream facts are only overwritten when supplied: an ingest without
    /// `--metadata` carries none, and must not erase the last-commit date that
    /// decay depends on.
    pub fn add_repo(&mut self, name: &str, upstream: &crate::fetch::Upstream) -> Result<i64> {
        self.db.execute(
            "DELETE FROM documents WHERE repo_id = (SELECT id FROM repos WHERE name = ?1)",
            params![name],
        )?;
        self.db.execute(
            "INSERT INTO repos (name, commit_sha, indexed_at, pushed_at, archived) \
             VALUES (?1, ?2, datetime('now'), ?3, ?4) \
             ON CONFLICT(name) DO UPDATE SET commit_sha = excluded.commit_sha, \
             indexed_at = excluded.indexed_at, \
             files = NULL, source_bytes = NULL, disk_bytes = NULL, \
             pushed_at = COALESCE(NULLIF(excluded.pushed_at, ''), pushed_at), \
             archived = CASE WHEN excluded.pushed_at <> '' \
                             THEN excluded.archived ELSE archived END",
            params![
                name,
                upstream.commit_sha,
                upstream.pushed_at,
                upstream.archived as i64
            ],
        )?;
        Ok(self.db.query_row(
            "SELECT id FROM repos WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )?)
    }

    /// Queue a document. Content is written once the dictionary exists.
    pub fn add_document(
        &mut self,
        repo_id: i64,
        path: &str,
        language: &str,
        content: Vec<u8>,
    ) -> Result<i64> {
        self.db.execute(
            "INSERT INTO documents (repo_id, path, language, raw_size, offset, length) \
             VALUES (?1, ?2, ?3, ?4, -1, -1)",
            params![repo_id, path, language, content.len() as i64],
        )?;
        let doc_id = self.db.last_insert_rowid();
        self.pending_bytes += content.len();
        self.pending.push((doc_id, content));

        // Train once there is either a good spread of samples or enough bytes
        // buffered that holding more would cost real memory.
        if self.dictionary.is_none()
            && !self.dictless
            && (self.pending.len() >= DICT_TRAINING_SAMPLES
                || self.pending_bytes >= MAX_PENDING_BYTES)
        {
            self.train_dictionary()?;
        }
        if self.dictionary.is_some() {
            self.flush_pending()?;
        }
        Ok(doc_id)
    }

    /// Train once on real corpus content; small source files compress far
    /// better against a shared dictionary than alone.
    ///
    /// Training needs a spread of samples. A handful of files (a tiny
    /// repository, or the tail of an ingest) cannot produce one, so those are
    /// stored without a dictionary rather than failing the ingest.
    fn train_dictionary(&mut self) -> Result<()> {
        let samples: Vec<&[u8]> = self
            .pending
            .iter()
            .map(|(_, content)| content.as_slice())
            .filter(|content| !content.is_empty())
            .collect();
        if samples.len() < MIN_DICT_SAMPLES {
            return Ok(());
        }
        let sizes: Vec<usize> = samples.iter().map(|s| s.len()).collect();
        let flat: Vec<u8> = samples.concat();
        let trained = match zstd::dict::from_continuous(&flat, &sizes, DICT_SIZE_BYTES) {
            Ok(dictionary) => dictionary,
            // Samples too small or too uniform; plain compression still works.
            Err(_) => return Ok(()),
        };
        self.db.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('zstd_dict', ?1)",
            params![trained],
        )?;
        self.dictionary = Some(trained);
        Ok(())
    }

    /// Compress and append everything queued.
    pub fn flush_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.dictionary.is_none() && !self.dictless {
            self.train_dictionary()?;
        }
        if self.dictionary.is_none() && !self.dictless {
            // These bytes compress without a dictionary, and one trained later
            // could not decode them. Pin the store so every document stays
            // readable with the same settings.
            self.dictless = true;
            self.db.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('dictless', ?1)",
                params![b"1".to_vec()],
            )?;
        }

        if self.writer.is_none() {
            self.writer = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.blob_path)
                    .with_context(|| format!("opening {}", self.blob_path.display()))?,
            );
        }
        let writer = self.writer.as_mut().expect("writer opened above");
        let mut offset = writer.seek(SeekFrom::End(0))?;

        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        // One compressor for the whole batch: building it re-loads the
        // dictionary, which costs more than compressing a small source file.
        let mut compressor = match &self.dictionary {
            Some(dictionary) => {
                zstd::bulk::Compressor::with_dictionary(COMPRESSION_LEVEL, dictionary)?
            }
            None => zstd::bulk::Compressor::new(COMPRESSION_LEVEL)?,
        };
        let transaction = self.db.unchecked_transaction()?;
        for (doc_id, content) in &pending {
            let packed = compressor.compress(content)?;
            writer.write_all(&packed)?;
            transaction.execute(
                "UPDATE documents SET offset = ?1, length = ?2 WHERE id = ?3",
                params![offset as i64, packed.len() as i64, doc_id],
            )?;
            offset += packed.len() as u64;
        }
        transaction.commit()?;
        // read_document uses a separate handle, so buffered bytes must reach
        // the file or a document written this session would read back empty.
        writer.flush()?;
        Ok(())
    }

    // -- reading ------------------------------------------------------------

    /// Content of one document.
    ///
    /// A missing id is an error rather than empty content: the caller asked
    /// for something specific. Callers walking the index should use
    /// `try_read_document`, since the index legitimately outlives the
    /// documents it points at until the next rebuild.
    ///
    /// Seek and read rather than mmap. Tantivy maps its index into memory,
    /// which measures 7% faster here (34ms vs 36ms for 3000 documents) and
    /// only on a warm page cache. That is not worth what it costs us:
    /// `compact` renames blobs.bin under live readers, so every mapping would
    /// need remapping, and a reader holding a mapping over a file that shrank
    /// takes SIGBUS instead of an error we can report.
    pub fn read_document(&mut self, doc_id: i64) -> Result<Vec<u8>> {
        let (offset, length): (i64, i64) = self
            .db
            .query_row(
                "SELECT offset, length FROM documents WHERE id = ?1",
                params![doc_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .with_context(|| format!("document {doc_id} is not in the corpus"))?;
        if offset < 0 {
            bail!("document {doc_id} not stored");
        }
        if self.reader.is_none() {
            self.reader = Some(File::open(&self.blob_path)?);
        }
        let reader = self.reader.as_mut().expect("reader opened above");
        reader.seek(SeekFrom::Start(offset as u64))?;
        let mut packed = vec![0u8; length as usize];
        reader.read_exact(&mut packed)?;

        let mut out = Vec::new();
        match &self.dictionary {
            Some(dictionary) => {
                zstd::Decoder::with_dictionary(&packed[..], dictionary)?.read_to_end(&mut out)?
            }
            None => zstd::Decoder::new(&packed[..])?.read_to_end(&mut out)?,
        };
        Ok(out)
    }

    /// Content of one document, or None if it is no longer stored.
    ///
    /// The trigram index references document ids, and an update replaces a
    /// repository's documents with new rows. Until the index is rebuilt it
    /// therefore points at ids that no longer exist, which is expected rather
    /// than an error: skip them instead of failing the whole query.
    pub fn try_read_document(&mut self, doc_id: i64) -> Result<Option<Vec<u8>>> {
        let found: Option<(i64, i64)> = self
            .db
            .query_row(
                "SELECT offset, length FROM documents WHERE id = ?1",
                params![doc_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        match found {
            Some((offset, _)) if offset < 0 => Ok(None),
            Some(_) => self.read_document(doc_id).map(Some),
            None => Ok(None),
        }
    }

    /// Trigrams dropped from the index for being too common.
    ///
    /// None when the index predates stop-list tracking, which means a lookup
    /// miss carries no information. Parsed once: the blob holds thousands of
    /// entries and diagnosis probes it for every trigram of a fragment.
    pub fn stop_trigrams(&mut self) -> Option<&HashSet<[u8; 3]>> {
        if self.stop_trigrams.is_none() {
            let blob: Option<Vec<u8>> = self
                .db
                .query_row(
                    "SELECT value FROM meta WHERE key='stop_trigrams'",
                    [],
                    |r| r.get(0),
                )
                .ok();
            self.stop_trigrams = Some(blob.map(|bytes| {
                // as_chunks yields fixed-size arrays directly, so no
                // per-element rebuild is needed.
                let (grams, _remainder) = bytes.as_chunks::<3>();
                grams.iter().copied().collect()
            }));
        }
        self.stop_trigrams.as_ref().and_then(|inner| inner.as_ref())
    }

    /// Drop derived state after the index is rebuilt underneath us.
    pub fn invalidate_caches(&mut self) {
        self.stop_trigrams = None;
    }

    /// Record each repository's language, file count and sizes.
    ///
    /// The listing reads these rather than deriving them: deriving meant
    /// grouping every document in the corpus for a command as ordinary as
    /// `repos`, 50ms here and seconds at 50,000 repositories. Ingest clears a
    /// repository's numbers and refreshes only those (`missing_only`) once its
    /// documents are flushed; an index run refreshes everything, which also
    /// fills a corpus from before the columns existed.
    pub fn refresh_repo_stats(&mut self, missing_only: bool) -> Result<()> {
        self.db.execute(
            &format!(
                "UPDATE repos SET \
                 language = ( \
                     SELECT language FROM documents \
                     WHERE repo_id = repos.id AND offset >= 0 \
                     GROUP BY language ORDER BY SUM(raw_size) DESC LIMIT 1 \
                 ), \
                 files = (SELECT COUNT(*) FROM documents \
                          WHERE repo_id = repos.id AND offset >= 0), \
                 source_bytes = (SELECT COALESCE(SUM(raw_size), 0) FROM documents \
                                 WHERE repo_id = repos.id AND offset >= 0), \
                 disk_bytes = (SELECT COALESCE(SUM(length), 0) FROM documents \
                               WHERE repo_id = repos.id AND offset >= 0) \
                 {}",
                if missing_only {
                    "WHERE files IS NULL"
                } else {
                    ""
                }
            ),
            [],
        )?;
        Ok(())
    }

    /// Rewrite blobs.bin with only the bytes still referenced.
    ///
    /// Updating a repository appends new content and orphans the old, so the
    /// file grows without bound across updates. Returns bytes reclaimed.
    pub fn compact(&mut self) -> Result<u64> {
        self.flush_pending()?;
        // Open the source explicitly rather than reusing the cached handle.
        // That handle may already refer to a file this process renamed away in
        // an earlier compaction, in which case the reads below fail after the
        // transaction has committed, leaving offsets that describe a file that
        // no longer exists. That is what corrupts a corpus beyond repair.
        self.reader = None;
        self.writer = None;
        let mut source = match File::open(&self.blob_path) {
            Ok(file) => file,
            // Nothing stored yet, so nothing to reclaim.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let before = std::fs::metadata(&self.blob_path)
            .map(|m| m.len())
            .unwrap_or(0);

        let live: Vec<i64> = self
            .db
            .prepare("SELECT id FROM documents WHERE offset >= 0 ORDER BY offset")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;

        // Write beside the original and rename. The new offsets and the new
        // file must land together: the generation number is committed with the
        // offsets, so open() can tell a finished compaction that failed to
        // rename from one whose transaction rolled back.
        let generation = self.blob_generation()? + 1;
        let temporary = self
            .blob_path
            .with_extension(format!("compacting.{generation}"));
        let mut output = std::io::BufWriter::with_capacity(1 << 20, File::create(&temporary)?);
        let mut moves: Vec<(i64, u64, usize)> = Vec::with_capacity(live.len());
        let mut offset = 0u64;
        for doc_id in live {
            let (old_offset, length): (i64, i64) = self.db.query_row(
                "SELECT offset, length FROM documents WHERE id = ?1",
                params![doc_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            source.seek(SeekFrom::Start(old_offset as u64))?;
            let mut packed = vec![0u8; length as usize];
            source.read_exact(&mut packed).with_context(|| {
                format!(
                    "reading document {doc_id} at offset {old_offset} while compacting; \
                     the corpus was left untouched"
                )
            })?;
            // A full disk fails here, before the database has been touched.
            // The partial file is removed by the next writer to open.
            output
                .write_all(&packed)
                .context("writing the compacted blob file; the corpus was left untouched")?;
            moves.push((doc_id, offset, packed.len()));
            offset += packed.len() as u64;
        }
        let output = output
            .into_inner()
            .map_err(|error| error.into_error())
            .context("writing the compacted blob file; the corpus was left untouched")?;
        output.sync_all()?;
        drop(output);
        // Every byte is now in the new file. Only past this point does the
        // database start describing it.
        drop(source);

        let transaction = self.db.unchecked_transaction()?;
        for (doc_id, new_offset, length) in moves {
            transaction.execute(
                "UPDATE documents SET offset = ?1, length = ?2 WHERE id = ?3",
                params![new_offset as i64, length as i64, doc_id],
            )?;
        }
        transaction.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('blob_generation', ?1)",
            params![generation.to_string().into_bytes()],
        )?;
        transaction.commit()?;

        std::fs::rename(&temporary, &self.blob_path)?;
        self.db.execute_batch("VACUUM")?;
        Ok(before.saturating_sub(offset))
    }

    fn blob_generation(&self) -> Result<u64> {
        let raw: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT value FROM meta WHERE key='blob_generation'",
                [],
                |row| row.get(0),
            )
            .ok();
        Ok(raw
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| text.parse().ok())
            .unwrap_or(0))
    }

    /// Drop document rows whose content never reached the blob file.
    ///
    /// A row is inserted with offset -1 and filled in when the batch flushes,
    /// so an ingest killed mid-run leaves rows pointing at nothing. They are
    /// invisible to search but still counted, which makes a repository look
    /// indexed when none of its code is there. Discarding them lets a re-run
    /// fetch the repository properly.
    fn discard_unflushed(&mut self) -> Result<()> {
        if self.lock.is_none() {
            // Read-only session: nothing to clean, and nothing may be written.
            return Ok(());
        }
        // Answered from the partial index on `offset < 0`, which is empty
        // whenever the last writer finished. Without it this was a full scan
        // of the documents table, 22ms of every writer's startup.
        let removed = self
            .db
            .execute("DELETE FROM documents WHERE offset < 0", [])?;
        if removed > 0 {
            // A repository left with no content at all was never really
            // ingested, so forget it rather than reporting an empty one.
            self.db.execute(
                "DELETE FROM repos WHERE id NOT IN (SELECT DISTINCT repo_id FROM documents)",
                [],
            )?;
        }
        Ok(())
    }

    /// Refuse to open a corpus whose offsets do not describe its blob file.
    ///
    /// Checked by decoding one document rather than comparing sizes, because a
    /// blob file is legitimately longer than the bytes in use: an update
    /// orphans the old copy until the next compaction. Only an actual read
    /// distinguishes "has slack" from "describes a different file".
    ///
    /// Without this the mismatch surfaces one document at a time as an opaque
    /// decompression error, which reads like corrupt data rather than the
    /// recoverable bookkeeping failure it is.
    fn check_blob_integrity(&mut self) -> Result<()> {
        let probe: Option<i64> = self
            .db
            .query_row(
                "SELECT id FROM documents WHERE offset >= 0 ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();
        let Some(doc_id) = probe else {
            return Ok(());
        };
        if self.read_document(doc_id).is_ok() {
            return Ok(());
        }
        bail!(
            "corpus is inconsistent: its stored offsets do not match blobs.bin, so \
             no document can be read. This happens when a compaction commits \
             without replacing the file. Recover with `steroids update` then \
             `steroids index`, or delete the corpus directory and re-add."
        );
    }

    /// Finish or discard a compaction that was interrupted before its rename.
    ///
    /// The committed generation decides: if it matches a leftover file, the
    /// database already refers to that file's layout and the rename must be
    /// completed. Any other leftover belongs to a rolled-back attempt.
    fn recover_compaction(&mut self) -> Result<()> {
        let Some(directory) = self.blob_path.parent() else {
            return Ok(());
        };
        let expected = self.blob_generation()?;
        let prefix = format!(
            "{}.compacting.",
            self.blob_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("blobs")
        );
        for entry in std::fs::read_dir(directory)?.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(suffix) = name.strip_prefix(&prefix) else {
                continue;
            };
            match suffix.parse::<u64>() {
                // The committed generation names the file the database now
                // describes, so finishing the rename is the repair.
                Ok(generation) if generation == expected && expected > 0 => {
                    std::fs::rename(entry.path(), &self.blob_path)?;
                }
                // Anything older is a rolled-back attempt and is safe to drop.
                // A newer number belongs to a compaction that has not committed
                // yet, which may still be running: leaving it costs disk, while
                // deleting it would destroy that run.
                Ok(generation) if generation < expected => {
                    let _ = std::fs::remove_file(entry.path());
                }
                _ => {}
            }
        }
        Ok(())
    }

    // -- browsing -----------------------------------------------------------

    pub fn list_repos(&self) -> Result<Vec<RepoSummary>> {
        // Language, file count and sizes are stored on the repository, not
        // derived here: see `refresh_repo_stats`. The subqueries are fallbacks
        // for a row nothing has recorded yet, a corpus from before the columns
        // existed or a repository whose ingest was interrupted, and COALESCE
        // skips them entirely once the columns are set.
        let mut statement = self.db.prepare(
            "SELECT r.name, COALESCE(r.commit_sha, ''), COALESCE(r.indexed_at, ''), \
             COALESCE(r.files, (SELECT COUNT(*) FROM documents \
                                WHERE repo_id = r.id AND offset >= 0)), \
             COALESCE(r.source_bytes, (SELECT COALESCE(SUM(raw_size), 0) FROM documents \
                                       WHERE repo_id = r.id AND offset >= 0)), \
             COALESCE(r.disk_bytes, (SELECT COALESCE(SUM(length), 0) FROM documents \
                                     WHERE repo_id = r.id AND offset >= 0)), \
             COALESCE(r.pushed_at, ''), COALESCE(r.tags, ''), \
             COALESCE(r.language, ( \
                 SELECT language FROM documents \
                 WHERE repo_id = r.id AND offset >= 0 \
                 GROUP BY language ORDER BY SUM(raw_size) DESC LIMIT 1 \
             ), '-') \
             FROM repos r ORDER BY r.name",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(RepoSummary {
                    name: row.get(0)?,
                    commit_sha: row.get(1)?,
                    indexed_at: row.get(2)?,
                    files: row.get(3)?,
                    source_bytes: row.get(4)?,
                    disk_bytes: row.get(5)?,
                    pushed_at: row.get(6)?,
                    tags: row.get(7)?,
                    language: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// (path, language, raw size) for the files indexed from one repository.
    pub fn list_files(&self, repo: &str, limit: usize) -> Result<Vec<(String, String, i64)>> {
        let mut statement = self.db.prepare(
            "SELECT d.path, d.language, d.raw_size FROM documents d \
             JOIN repos r ON r.id = d.repo_id \
             WHERE r.name = ?1 AND d.offset >= 0 ORDER BY d.path LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![repo, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Full content of one indexed file, or None if it is not in the corpus.
    pub fn read_path(&mut self, repo: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let doc_id: Option<i64> = self
            .db
            .query_row(
                "SELECT d.id FROM documents d JOIN repos r ON r.id = d.repo_id \
                 WHERE r.name = ?1 AND d.path = ?2 AND d.offset >= 0",
                params![repo, path],
                |row| row.get(0),
            )
            .ok();
        match doc_id {
            Some(id) => Ok(Some(self.read_document(id)?)),
            None => Ok(None),
        }
    }

    /// Attach labels to a repository, keeping any it already had.
    ///
    /// Tags are stored as a comma-wrapped string (`,a,b,`) so a LIKE query can
    /// match a whole tag without matching a prefix of a longer one.
    pub fn tag_repo(&mut self, repo: &str, tags: &[String]) -> Result<bool> {
        let existing: String = self
            .db
            .query_row(
                "SELECT COALESCE(tags, '') FROM repos WHERE name = ?1",
                params![repo],
                |row| row.get(0),
            )
            .unwrap_or_default();
        let mut all: Vec<String> = existing
            .split(',')
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        for tag in tags {
            let tag = tag.trim().to_lowercase();
            if !tag.is_empty() && !all.contains(&tag) {
                all.push(tag);
            }
        }
        let updated = self.db.execute(
            "UPDATE repos SET tags = ?1 WHERE name = ?2",
            params![all.join(","), repo],
        )?;
        Ok(updated > 0)
    }

    /// Every language present in the indexed files.
    ///
    /// Taken from the documents rather than each repository's main language: a
    /// Python project can still hold the SQL or shell someone is looking for.
    pub fn languages(&self) -> Result<Vec<String>> {
        let mut statement = self
            .db
            .prepare("SELECT DISTINCT language FROM documents WHERE offset >= 0")?;
        let rows = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    /// Whether any indexed file path matches a glob. `None` when the glob
    /// holds a `[`, which SQLite reads as a class and the search reads
    /// literally, so this cannot answer for it.
    pub fn any_path_matches(&self, glob: &str) -> Result<Option<bool>> {
        if glob.contains('[') {
            return Ok(None);
        }
        let found: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM documents WHERE offset >= 0 AND path GLOB ?1)",
            params![glob],
            |row| row.get(0),
        )?;
        Ok(Some(found))
    }

    /// A few indexed file paths, to show what a path glob has to match.
    pub fn sample_paths(&self, count: usize) -> Result<Vec<String>> {
        let mut statement = self
            .db
            .prepare("SELECT path FROM documents WHERE offset >= 0 ORDER BY id LIMIT ?1")?;
        let rows = statement
            .query_map(params![count as i64], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    /// Repositories carrying a tag, or all of them when `tag` is None.
    pub fn repos_tagged(&self, tag: Option<&str>) -> Result<Vec<RepoSummary>> {
        let all = self.list_repos()?;
        let Some(tag) = tag else { return Ok(all) };
        let tag = tag.trim().to_lowercase();
        Ok(all
            .into_iter()
            .filter(|r| r.tags.split(',').any(|t| t == tag))
            .collect())
    }

    /// Every tag in use, with how many repositories carry it.
    pub fn tag_counts(&self) -> Result<Vec<(String, usize)>> {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for repo in self.list_repos()? {
            for tag in repo.tags.split(',').filter(|t| !t.is_empty()) {
                *counts.entry(tag.to_string()).or_default() += 1;
            }
        }
        Ok(counts.into_iter().collect())
    }

    /// Forget a repository. Its blob bytes stay until the corpus is rebuilt,
    /// but nothing references them and the index drops them on the next build.
    pub fn remove_repo(&mut self, repo: &str) -> Result<bool> {
        let removed = self.db.execute(
            "DELETE FROM documents WHERE repo_id = (SELECT id FROM repos WHERE name = ?1)",
            params![repo],
        )?;
        let repos = self
            .db
            .execute("DELETE FROM repos WHERE name = ?1", params![repo])?;
        Ok(removed > 0 || repos > 0)
    }

    /// Repositories whose newest upstream commit predates `cutoff`, an ISO
    /// date, plus archived ones when `include_archived` is set.
    ///
    /// Rows with no `pushed_at` are never returned: those predate freshness
    /// tracking, and deleting data on missing information would be wrong.
    pub fn stale_repos(
        &self,
        cutoff: &str,
        include_archived: bool,
    ) -> Result<Vec<(String, String, bool)>> {
        // Two independent reasons to drop a repository, so they are ORed with
        // their own guards. Age needs a known commit date, since deleting on
        // missing information would be wrong. Being archived needs no date at
        // all: the upstream is frozen whatever its history says.
        let mut statement = self.db.prepare(
            "SELECT name, COALESCE(pushed_at, ''), COALESCE(archived, 0) FROM repos \
             WHERE (pushed_at IS NOT NULL AND pushed_at <> '' \
                    AND substr(pushed_at, 1, 10) < ?1) \
                OR (?2 AND archived = 1) \
             ORDER BY pushed_at",
        )?;
        let rows = statement
            .query_map(params![cutoff, include_archived], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? == 1,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// (repositories, documents, raw source bytes)
    pub fn totals(&self) -> Result<(i64, i64, i64)> {
        Ok(self.db.query_row(
            "SELECT (SELECT COUNT(*) FROM repos), COUNT(*), COALESCE(SUM(raw_size), 0) \
             FROM documents WHERE offset >= 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    }
}

/// Take an exclusive advisory lock, blocking until it is available.
///
/// `flock` is used directly because the standard library has no file locking
/// on stable, and pulling in a dependency for two lines of libc is not worth
/// it. The lock releases automatically when the file handle closes, including
/// on a crash, so a killed ingest cannot leave the corpus permanently locked.
#[cfg(unix)]
fn lock_exclusive(file: &File, wait: bool) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const EWOULDBLOCK: i32 = 11;
    let operation = if wait { LOCK_EX } else { LOCK_EX | LOCK_NB };
    // SAFETY: the descriptor is owned by `file` and valid for this call.
    if unsafe { flock(file.as_raw_fd(), operation) } != 0 {
        let error = std::io::Error::last_os_error();
        // EAGAIN on Linux, EWOULDBLOCK (35) on the BSDs and macOS.
        if !wait && matches!(error.raw_os_error(), Some(EWOULDBLOCK) | Some(35)) {
            return Ok(false);
        }
        bail!("could not lock the corpus for writing: {error}");
    }
    Ok(true)
}

/// Windows equivalent, via `LockFileEx`.
///
/// Not a no-op: two writers that each train a zstd dictionary leave the
/// other's documents undecodable, and that has already destroyed a corpus
/// once. A platform without the lock is a platform where that happens
/// silently, so this blocks the same way `flock` does.
#[cfg(windows)]
fn lock_exclusive(file: &File, wait: bool) -> Result<bool> {
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut core::ffi::c_void,
    }

    unsafe extern "system" {
        fn LockFileEx(
            handle: *mut core::ffi::c_void,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }
    const EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    const FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    let flags = if wait {
        EXCLUSIVE_LOCK
    } else {
        EXCLUSIVE_LOCK | FAIL_IMMEDIATELY
    };

    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    // SAFETY: the handle is owned by `file` and valid for this call, and the
    // overlapped structure lives until the call returns.
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            flags,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if locked == 0 {
        let error = std::io::Error::last_os_error();
        if !wait && error.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
            return Ok(false);
        }
        bail!("could not lock the corpus for writing: {error}");
    }
    Ok(true)
}

/// Anything that is neither Unix nor Windows. Refuses rather than pretending,
/// because a silent no-op here is corruption waiting to happen.
#[cfg(not(any(unix, windows)))]
fn lock_exclusive(_file: &File, _wait: bool) -> Result<bool> {
    bail!("this platform has no file locking, so writing a corpus is unsafe")
}

/// A temporary directory unique to one test run.
///
/// Every test in this process shares a pid, and cargo runs them on parallel
/// threads, so a name built from the pid alone collides. On Unix the collision
/// is usually harmless; on Windows deleting a directory another thread still
/// has open fails outright.
#[cfg(test)]
pub fn scratch_dir(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Cleanup at the end of a test is best effort for the same reason: a
    // Store may still hold the lock file when it runs.
    //
    // Nanoseconds as well as a counter, and no attempt to clear the path:
    // Windows cannot delete a directory whose lock file a previous run still
    // holds open, so never reuse a name rather than trying to empty one.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "steroids-{label}-{}-{unique}-{stamp}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal upstream facts for tests that only care about content.
    fn upstream(sha: &str) -> crate::fetch::Upstream {
        crate::fetch::Upstream {
            commit_sha: sha.into(),
            pushed_at: "2026-01-01T00:00:00Z".into(),
            ..Default::default()
        }
    }

    /// Compaction rewrites the blob file and every offset with it. Losing or
    /// misplacing a single document would corrupt the corpus silently, so the
    /// content must survive byte for byte.
    #[test]
    fn compaction_preserves_every_document() -> Result<()> {
        let directory = crate::store::scratch_dir("compact");
        let _ = std::fs::remove_dir_all(&directory);
        let mut store = Store::open(&directory)?;

        let repo_id = store.add_repo("a/b", &upstream("sha"))?;
        let mut expected = Vec::new();
        for i in 0..30u8 {
            let content = format!("fn item_{i}() {{ let x = {i}; }}\n").repeat(20);
            store.add_document(
                repo_id,
                &format!("src/f{i}.rs"),
                "rust",
                content.clone().into_bytes(),
            )?;
            expected.push(content);
        }
        store.flush_pending()?;

        // Replacing the repository orphans the first copy's bytes.
        let repo_id = store.add_repo("a/b", &upstream("sha2"))?;
        for (i, content) in expected.iter().enumerate() {
            store.add_document(
                repo_id,
                &format!("src/f{i}.rs"),
                "rust",
                content.clone().into_bytes(),
            )?;
        }
        store.flush_pending()?;

        let reclaimed = store.compact()?;
        assert!(reclaimed > 0, "compaction reclaimed nothing");

        let files = store.list_files("a/b", 100)?;
        assert_eq!(files.len(), expected.len());
        for (i, content) in expected.iter().enumerate() {
            let stored = store
                .read_path("a/b", &format!("src/f{i}.rs"))?
                .expect("document missing after compaction");
            assert_eq!(stored, content.as_bytes(), "document {i} corrupted");
        }

        let _ = std::fs::remove_dir_all(&directory);
        Ok(())
    }

    /// An ingest killed mid-run leaves document rows whose content never
    /// reached the blob file. They are invisible to search but still counted,
    /// so a repository looks indexed when none of its code is there.
    #[test]
    fn interrupted_ingest_leaves_no_phantom_rows() -> Result<()> {
        let dir = crate::store::scratch_dir("phantom");
        let _ = std::fs::remove_dir_all(&dir);

        {
            let mut store = Store::open_for_write(&dir)?;
            let repo = store.add_repo("ghost/repo", &crate::fetch::Upstream::default())?;
            // Queued but never flushed, exactly as a killed process leaves it.
            store.add_document(repo, "a.rs", "rust", b"fn main() {}".to_vec())?;
            // Drop the queue so the flush on close has nothing to write, which
            // reproduces the killed-process state. The Store still drops
            // normally, releasing its lock.
            store.pending.clear();
            store.pending_bytes = 0;
        }

        // Reopening for write must clear the wreckage.
        let store = Store::open_for_write(&dir)?;
        let orphans: i64 = store.db.query_row(
            "SELECT COUNT(*) FROM documents WHERE offset < 0",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(orphans, 0, "unflushed rows survived");
        assert!(
            store.list_repos()?.is_empty(),
            "a repository with no content was still listed"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// The trigram index outlives the documents it points at: an update
    /// replaces a repository's rows, so index entries reference ids that no
    /// longer exist until the next rebuild. Reading one must skip, not fail,
    /// or every search between an update and a reindex errors out.
    #[test]
    fn a_stale_index_entry_does_not_fail_a_search() -> Result<()> {
        let dir = crate::store::scratch_dir("staleindex");
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::open_for_write(&dir)?;
        let repo = store.add_repo("a/b", &crate::fetch::Upstream::default())?;
        let id = store.add_document(repo, "a.rs", "rust", b"fn main() {}".to_vec())?;
        store.flush_pending()?;

        assert!(store.try_read_document(id)?.is_some());
        // Replacing the repository drops its documents, as an update does.
        store.add_repo("a/b", &crate::fetch::Upstream::default())?;
        assert!(
            store.try_read_document(id)?.is_none(),
            "a replaced document was still readable"
        );
        assert!(
            store.read_document(id).is_err(),
            "asking for a specific missing document should still be an error"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Compaction must never commit new offsets it cannot back with data.
    ///
    /// The real failure: the source handle was cached from before an earlier
    /// compaction renamed the file away, so reads failed with "No such file or
    /// directory" after the transaction had committed. The database then
    /// described a file that did not exist and every document became
    /// unreadable.
    #[test]
    fn compaction_leaves_the_corpus_readable() -> Result<()> {
        let dir = crate::store::scratch_dir("compact2");
        let _ = std::fs::remove_dir_all(&dir);

        let payload = b"fn main() { println!(\"hi\"); }\n".repeat(30);
        let mut store = Store::open_for_write(&dir)?;
        let repo = store.add_repo("a/b", &crate::fetch::Upstream::default())?;
        let ids: Vec<i64> = (0..15)
            .map(|i| store.add_document(repo, &format!("f{i}.rs"), "rust", payload.clone()))
            .collect::<Result<_>>()?;
        store.flush_pending()?;

        // Repeated compaction in one process is what exposed the stale handle.
        for round in 0..3 {
            store.compact()?;
            for id in &ids {
                let content = store.read_document(*id).with_context(|| {
                    format!("document {id} unreadable after compaction {round}")
                })?;
                assert_eq!(content, payload, "document {id} changed under compaction");
            }
        }

        // And still readable to a fresh process.
        let mut reopened = Store::open(&dir)?;
        assert_eq!(reopened.read_document(ids[0])?, payload);

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// A reader must never repair the corpus. Recovery deletes stale
    /// compaction files, and a reader doing that can delete the temporary file
    /// of a compaction running in another process, stranding its committed
    /// offsets against the old blob file and destroying every document.
    #[test]
    fn a_reader_never_deletes_a_pending_compaction() -> Result<()> {
        let dir = crate::store::scratch_dir("pending");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut store = Store::open_for_write(&dir)?;
            let repo = store.add_repo("a/b", &crate::fetch::Upstream::default())?;
            store.add_document(repo, "a.rs", "rust", b"fn main() {}".repeat(20))?;
            store.flush_pending()?;
        }

        // A compaction in flight: its file exists, its generation is not
        // committed yet.
        let pending = dir.join("blobs.compacting.9");
        std::fs::write(&pending, b"in progress")?;

        // Opening to read must leave it alone.
        let _reader = Store::open(&dir)?;
        assert!(
            pending.exists(),
            "a read-only open deleted a pending compaction"
        );

        // A writer must leave a newer generation alone too, since it may
        // belong to a run that has not committed.
        let _writer = Store::open_for_write(&dir)?;
        assert!(
            pending.exists(),
            "a writer deleted a compaction newer than the committed generation"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Two writers must not both train a zstd dictionary: whichever saves last
    /// leaves the other's documents undecodable. Real failure: a concurrent
    /// ingest left 34 of 54 documents unreadable.
    #[test]
    fn concurrent_writers_do_not_corrupt_documents() -> Result<()> {
        let dir = crate::store::scratch_dir("lock");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;

        let payload = b"fn main() { println!(\"hello\"); }\n".repeat(20);
        let write = |tag: &str| -> Result<Vec<i64>> {
            let mut store = Store::open_for_write(&dir)?;
            let repo = store.add_repo(tag, &crate::fetch::Upstream::default())?;
            let ids = (0..12)
                .map(|i| {
                    store.add_document(repo, &format!("{tag}/f{i}.rs"), "rust", payload.clone())
                })
                .collect::<Result<Vec<_>>>()?;
            store.flush_pending()?;
            Ok(ids)
        };

        let (first, second) = std::thread::scope(|scope| {
            let a = scope.spawn(|| write("a/one"));
            let b = scope.spawn(|| write("b/two"));
            (a.join().expect("thread"), b.join().expect("thread"))
        });
        let mut ids = first?;
        ids.extend(second?);

        // Every document written by either party must still decode.
        let mut store = Store::open(&dir)?;
        for id in ids {
            let content = store.read_document(id)?;
            assert!(
                content.starts_with(b"fn main()"),
                "document {id} came back corrupted"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Decay must never delete on missing information: a repository indexed
    /// before freshness tracking has no pushed_at and must be left alone.
    #[test]
    fn stale_detection_ignores_unknown_dates() -> Result<()> {
        let directory = crate::store::scratch_dir("stale");
        let _ = std::fs::remove_dir_all(&directory);
        let mut store = Store::open(&directory)?;

        store.add_repo(
            "old/repo",
            &crate::fetch::Upstream {
                commit_sha: "a".into(),
                pushed_at: "2019-01-05T00:00:00Z".into(),
                ..Default::default()
            },
        )?;
        store.add_repo(
            "fresh/repo",
            &crate::fetch::Upstream {
                commit_sha: "b".into(),
                pushed_at: "2026-08-01T00:00:00Z".into(),
                ..Default::default()
            },
        )?;
        store.add_repo(
            "archived/repo",
            &crate::fetch::Upstream {
                commit_sha: "c".into(),
                pushed_at: "2026-08-01T00:00:00Z".into(),
                archived: true,
                ..Default::default()
            },
        )?;
        // Predates tracking: no upstream date recorded at all.
        store.db.execute(
            "INSERT INTO repos (name, commit_sha, indexed_at) VALUES ('legacy/repo', 'd', datetime('now'))",
            [],
        )?;

        // Archived removal off: only the age rule should fire.
        let stale: Vec<String> = store
            .stale_repos("2026-01-01", false)?
            .into_iter()
            .map(|(name, ..)| name)
            .collect();
        assert_eq!(
            stale,
            vec!["old/repo".to_string()],
            "wrong set without archived"
        );

        // An archived repository with no commit date must still be caught:
        // frozen upstream is reason enough, no date required.
        store.db.execute(
            "INSERT INTO repos (name, commit_sha, indexed_at, archived) \
             VALUES ('dead/repo', 'e', datetime('now'), 1)",
            [],
        )?;

        let with_archived: Vec<String> = store
            .stale_repos("2026-01-01", true)?
            .into_iter()
            .map(|(name, ..)| name)
            .collect();
        assert!(with_archived.contains(&"archived/repo".to_string()));
        assert!(
            with_archived.contains(&"dead/repo".to_string()),
            "archived repo with no commit date was kept"
        );
        assert!(
            !with_archived.contains(&"legacy/repo".to_string()),
            "deleted on unknown date"
        );
        assert!(!with_archived.contains(&"fresh/repo".to_string()));

        let _ = std::fs::remove_dir_all(&directory);
        Ok(())
    }

    /// Re-ingesting without --metadata carries no upstream facts, and must not
    /// wipe the last-commit date that decay depends on.
    #[test]
    fn reingest_without_metadata_keeps_decay_data() -> Result<()> {
        let directory = crate::store::scratch_dir("keep");
        let _ = std::fs::remove_dir_all(&directory);
        let mut store = Store::open(&directory)?;

        store.add_repo(
            "a/b",
            &crate::fetch::Upstream {
                commit_sha: "sha1".into(),
                pushed_at: "2020-01-01T00:00:00Z".into(),
                archived: true,
                ..Default::default()
            },
        )?;

        // A metadata-free ingest: only the commit sha is known.
        store.add_repo(
            "a/b",
            &crate::fetch::Upstream {
                commit_sha: "sha2".into(),
                ..Default::default()
            },
        )?;

        let (pushed, archived): (String, i64) = store.db.query_row(
            "SELECT pushed_at, archived FROM repos WHERE name = 'a/b'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(pushed, "2020-01-01T00:00:00Z", "last-commit date erased");
        assert_eq!(archived, 1, "archived flag erased");

        let _ = std::fs::remove_dir_all(&directory);
        Ok(())
    }

    /// A repository with only a handful of files must still store and read
    /// back: zstd cannot train a dictionary from very few samples.
    #[test]
    fn small_corpus_round_trips() -> Result<()> {
        let dir = crate::store::scratch_dir("corpustest");
        let _ = std::fs::remove_dir_all(&dir);
        let payload = b"def hello():\n    return 42\n".repeat(40);

        let mut store = Store::open(&dir)?;
        let repo = store.add_repo("a/b", &upstream("sha"))?;
        let mut ids = Vec::new();
        for index in 0..3u8 {
            let mut content = payload.clone();
            content.push(index);
            ids.push(store.add_document(repo, &format!("x{index}.py"), "python", content)?);
        }
        store.flush_pending()?;
        for id in &ids {
            assert!(store.read_document(*id)?.starts_with(b"def hello"));
        }
        drop(store);

        // Reopening and appending must not orphan the dictionary-less files.
        let mut store = Store::open(&dir)?;
        let repo = store.add_repo("a/b", &upstream("sha"))?;
        let mut more = Vec::new();
        for index in 0..20u8 {
            let mut content = payload.clone();
            content.push(index);
            more.push(store.add_document(repo, &format!("y{index}.py"), "python", content)?);
        }
        store.flush_pending()?;
        for id in &more {
            assert!(store.read_document(*id)?.starts_with(b"def hello"));
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// An older binary must refuse a corpus a newer one has stamped, for both
    /// readers and writers: half-reading a format it does not know is how a
    /// rollback corrupts data silently.
    #[test]
    fn refuses_a_corpus_from_a_newer_binary() -> Result<()> {
        let dir = crate::store::scratch_dir("schema");
        let store = Store::open_for_write(&dir)?;
        let stamped: Vec<u8> = store.db.query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(stamped, SCHEMA_VERSION.to_string().into_bytes());

        store.db.execute(
            "UPDATE meta SET value = ?1 WHERE key='schema_version'",
            params![(SCHEMA_VERSION + 1).to_string().into_bytes()],
        )?;
        drop(store);

        let refused = Store::open(&dir).err().map(|e| e.to_string());
        assert!(
            refused
                .as_deref()
                .is_some_and(|m| m.contains("steroids upgrade")),
            "reader accepted a newer corpus: {refused:?}"
        );
        assert!(Store::open_for_write(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
