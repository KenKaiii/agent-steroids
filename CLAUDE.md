<!-- gg:init:start -->
# steroids

**Why it exists:** so a coding agent never writes from memory alone. Before coding, it pulls real implementations of the same problem from many maintained repos; after coding, it diffs its own output against them to catch what is missing, wrong, or skipped. The product is grounded code, and every design decision below serves that.

**What it is:** a single-binary local corpus , fetches GitHub repo tarballs, stores each file zstd-compressed, indexes trigrams in SQLite, serves regex search as JSON (agent) or ANSI/TUI (human). No daemon, no git history , snapshots only.

**What this implies when changing it:**
- Recall beats brevity: silently dropping matches is the worst failure mode, worse than a slow query. The scan/id caps are latency ceilings, not correctness licences , never tighten them to make something feel fast.
- Results must stay comparable across repos: identical filtering, context, and formatting regardless of source, or cross-repo divergence reads as a real difference when it is an artifact.
- Output is consumed inside a context window, so it is capped by tokens rather than rows; anything added to a result must earn its tokens.
- Freshness is the point (`update`, `decay`, `discover`): a stale corpus teaches outdated patterns.

## Module ownership

- `main.rs` , clap subcommands + dispatch; resolves corpus root and decides read vs. write Store.
- `store.rs` , on-disk truth: `corpus.db` (metadata, postings, dictionary) + `blobs.bin`, write locking, compaction.
- `index.rs` , trigram posting lists, incremental vs. full rebuild, stop-trigram list.
- `search.rs` , trigram narrowing → regex verification → filters → token-capped results.
- `fetch.rs` / `bulk.rs` , tarball download + decode; N fetch threads feeding **one** writer thread.
- `filters.rs` , what counts as source: extension, binary detection, language, comment stripping.
- `discover.rs` / `recent.rs` , GitHub search API; Atom commit feeds (unauthenticated, not rate-limited).
- `render.rs`, `tui/` , output formatting; ratatui browser with background jobs.

## Invariants (violating these corrupts corpora silently)

- **Take the write lock before reading the zstd dictionary**, not before the first write. Two writers that each see "no dictionary" both train one, and the loser's blobs become undecodable.
- **`dictless` is one-way.** Once anything is stored without a dictionary, a dictionary trained later cannot decode it; the flag persists in the DB.
- **Only writers may run compaction recovery.** A reader that "repairs" can delete a live writer's pending temp file and strand every committed offset.
- **Reopen the blob reader after compaction** , the cached handle points at the renamed-away file and reads fail silently.
- **`write.lock` is never truncated or deleted**, and is opened read+write (not append): Windows refuses `LockFileEx` on an append-only handle. The file holds no data; its existence is the point.
- **Incremental indexing relies on document ids only increasing.** Replacing a repo leaves dead postings; a full rebuild auto-triggers past 40% stale. Any change here must keep incremental output byte-identical to a full rebuild, or searches quietly stop matching stored files.
- **The stop-trigram list only accumulates.** Incremental runs merge historical drops , recomputing "too common" from one small batch would wrongly mark trigrams rare.
- **Replacing/removing a repo leaks blob bytes**; only `compact` reclaims them.
- Deliberate ceilings in `search.rs`: `SQL_ID_LIMIT = 50_000` (larger `IN` lists strain the SQL parser; above it ids are streamed), `MAX_CANDIDATES = 20_000` (spread round-robin across repos, never taken in id order, and reported as `more_available`) and `MAX_DOCUMENTS_SCANNED = 2_000` (latency over completeness).
- Rebuilds flush postings in batches (`FLUSH_THRESHOLD`) and commit `indexed_upto`, `stale_postings` and `doc_count` with each batch, so an interrupted rebuild resumes. Clearing the index must clear all three markers in the same transaction.
- **Run `cargo fmt --check` before pushing.** The `check` CI job fails on formatting alone; a pre-push hook in `.git/hooks` runs it, but the hook is not committed.
- `COMPRESSION_LEVEL = 6` is measured, not arbitrary , level 12 gives the same size for ~10× the CPU on the single writer thread.
- In `SCHEMA` (store.rs), statements are split on `;`, so **no trailing SQL comments** , they get carried into the next statement and break it.

## Cross-platform

CI runs the full matrix on Linux/macOS/Windows because file locking, home-dir resolution, and paths all differ. Tests must give each case its own never-reused scratch dir (nanos + atomic counter) and treat cleanup as best-effort , Windows cannot delete a directory whose lock file is still held.

## Workflows beyond plain cargo

- CLI surface tests (all commands + failure paths + exit codes) need a release build first:
  `cargo build --release && BIN=./target/release/steroids bash tests/commands.sh`
- Smoke path CI asserts on every OS: `add antirez/smallchat` → `index` → `search` → `repos`.
- Tests needing a populated corpus self-skip unless `STEROIDS_TEST_ROOT` is set; network tests self-skip unless `STEROIDS_NETWORK_TESTS` is set. A green local run may have run almost nothing.
- **Every write command indexes before it returns** (`add`/`update`/`remove`/`decay`). `steroids index` is only needed for `--rebuild` or after an interrupted run; search names any repos the index has not seen.
- Runtime env: `STEROIDS_ROOT` (or `--root`) overrides the corpus dir; `GITHUB_TOKEN` raises API limits when set.
- `corpus-data/` is generated and gitignored. `starter-repos.txt` is a hand-curated, committed input list , not generated.
- MSRV 1.88 is pinned by ratatui's `time` dependency, not by our own edition-2024 syntax.
<!-- gg:init:end -->
