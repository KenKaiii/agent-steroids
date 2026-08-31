# steroids

A local corpus of open-source code. Index other people's repositories, then let
a coding agent search them for real implementations instead of guessing.

One binary. No runtime, no daemon, no MCP server — an agent calls `steroids
search` the same way it calls `grep`.

## Install

```bash
cargo install --path .
```

The corpus lives in `~/.steroids` by default, so the command works from any
directory. Override with `--root` or `$STEROIDS_ROOT`.

## Interactive

```bash
steroids
```

Opens a browser over the corpus: repositories, their files, and a live search
that updates as you type. Arrow keys move, `↵` opens, `esc` goes back, `q`
quits. From the repository list: `a` adds, `d` removes, `u` re-fetches
everything. Long jobs run in the background with progress, so the UI never
freezes.

```
 steroids   5 repositories · 2810 files · ~/.steroids
╭ Search ──────────────────────────────────────────────────────────────╮
│ asyncio.gather                                                       │
╰──────────────────────────────────────────────────────────────────────╯
╭ 40 results ────────────────────╮╭ Preview ───────────────────────────╮
│ crewAI  cleanup.py:261         ││ crewAIInc/crewAI/lib/…/cleanup.py  │
│   async with semaphore:        ││                                    │
│ langgraph  __init__.py:297     ││             )                      │
│   async def _aembed_search_qu… ││                                    │
│ autogen  scenario.py:369       ││ ▌  results = await asyncio.gather( │
╰────────────────────────────────╯╰────────────────────────────────────╯
 type to search   ↑↓ results   ↵ open   esc back
```

## Commands

```bash
steroids add openai/openai-agents-python  # owner/name
steroids add https://github.com/openai/openai-agents-python   # or a pasted URL
steroids add --from-file repos.txt        # bulk, one per line
steroids index                            # after any add

steroids search 'class \w+Agent\('        # regex, all repos
steroids define RunContext                # where a symbol is defined
steroids show <repo> <path>               # full file

steroids repos                            # what is indexed
steroids files <repo>                     # what was kept from one repo
steroids update                           # re-fetch everything at latest commit
steroids remove <repo>
steroids compact                          # reclaim space after updates/removals
steroids stats                            # disk usage
```

## Bulk ingest

```bash
steroids add --from-file repos.txt              # hundreds at a time
steroids add --from-file repos.txt --parallel 16
```

**`add` makes no GitHub API calls.** Code comes from codeload, which is not
rate limited, so a 500-repository ingest is bounded by your bandwidth rather
than by a request quota — no token needed, no 60/hour ceiling.

Measured: 40 repositories, 42,679 files, 382MB of source in **1m36s** at
`--parallel 16`, with the API quota fully exhausted. That source compresses to
94MB stored.

`--metadata` records stars and the last-commit date that `decay` needs, at the
cost of one rate-limited API call per repository. `discover` needs the search
API; `update` uses metadata but falls back to code-only if the quota runs out,
so a big refresh always completes.

## Filling the corpus fast

```bash
steroids discover 'topic:ai-agents language:python'   # preview
steroids discover 'topic:mcp' --add                   # index them
steroids discover --trending --days 7 --language rust --add
```

Any [GitHub search qualifiers](https://docs.github.com/search-github/searching-on-github/searching-for-repositories)
work. Already-indexed and archived repositories are skipped, and `min_stars`
filters out noise — though your own `stars:` or `archived:` qualifier wins if
you write one. `--trending` approximates GitHub's trending page (which has no
API) as "pushed recently, most stars first".

Discovery uses the search API, which allows 10 requests per minute without
`GITHUB_TOKEN`. Ingesting the results does not.

## Pruning what has gone stale

```bash
steroids config decay_months 6      # 0 = never, the default
steroids decay --dry-run            # see what would go
steroids decay
```

Decay is measured from the repository's **last upstream commit**, not when you
last indexed it. Repositories indexed before this feature existed have no
recorded commit date and are never removed — run `steroids update` to fill it
in. `decay_archived` also drops repositories the owner has archived.

## Settings

```bash
steroids config                     # show everything
steroids config decay_months 6      # change one
```

| Setting | Default | Meaning |
|---|---|---|
| `decay_months` | `0` | Remove repos with no commit in N months. 0 disables. |
| `decay_archived` | `false` | Also remove archived repos. |
| `auto_discover` | `false` | Top up from `discover_query` after each `update`. |
| `discover_query` | `topic:ai-agents` | Qualifiers for discovery. |
| `discover_limit` | `25` | Max repos one discovery run adds. |
| `min_stars` | `100` | Ignore repos below this. |

Settings live inside the corpus, so they travel with it.

Set `GITHUB_TOKEN` to raise the API rate limit from 60 to 5,000 requests/hour.
This affects `discover`, `update` and `add --metadata`; plain `add` needs no
token at any scale.

## Giving it to a coding agent

No integration needed — the agent runs the CLI. Tell it, in your agent config
or system prompt:

```
You have a local corpus of indexed open-source repositories:
  steroids search '<regex>' [--repo R] [--language L] [--limit N]
  steroids define <Symbol>
  steroids show <repo> <path>
  steroids repos
Use it to compare how other projects solved a problem before writing your own.
```

`search`, `define` and `repos` take `--json` for structured output. On an empty
result the JSON carries a `reason` (`topic_absent`, `near_miss`, …) and a
`suggestion`, so a caller can branch without parsing prose.

Results are spread across repositories so the agent sees several projects'
approaches side by side, each snippet labelled with its enclosing function or
class.

## What it returns

```
--- openai/openai-agents-python/examples/basic/retry.py:42  [async def policy(...)]
    if isinstance(decision, RetryDecision):
        if not decision.retry:
```

When nothing matches, the output says *why* — an empty corpus, a topic no
indexed project covers, a near-miss spelling, or a pattern with nothing
searchable in it — so the agent either retries usefully or tells you which
repositories to add.

## Measured (5 AI-agent repositories)

| | |
|---|---|
| Source indexed | 25.6 MB (2,810 files, after filtering) |
| On disk | **10.1 MB** (5.0 MB code + 5.1 MB index) |
| Per repository | ~1 MB stored |
| Search | **0–50 ms** (total process time) |
| Binary | 8 MB, no runtime |

Extrapolating: 50 repos ≈ 100 MB, 500 repos ≈ 1 GB.

## How it stays small

- **Code only.** No READMEs, images, docs, tests, lockfiles, vendored deps or
  generated files. Roughly 90% of files in a repo are dropped — see
  `src/filters.rs`. `examples/` is kept on purpose: worked examples are prime
  material for an agent.
- **Tarballs, not clones.** We want current code, not history.
- **Shared zstd dictionary.** Thousands of small source files compress ~5x
  against a dictionary trained on the corpus itself.
- **Non-positional trigram index.** Stores which files contain a trigram, not
  where. A fraction of the size of a positional index; queries narrow with the
  index and confirm with a real regex.

## Portable

The corpus is a directory: `corpus.db` plus `blobs.bin`. Copy it to an external
drive and point at it:

```bash
STEROIDS_ROOT=/Volumes/MyDrive/steroids steroids search 'retry'
```

## Layout

```
src/filters.rs  what earns disk space
src/fetch.rs    GitHub tarball ingest
src/store.rs    compressed content store (sqlite + blobs.bin)
src/index.rs    trigram index
src/search.rs   query: narrow by trigram, verify by regex
src/render.rs   output built for an agent to read
src/main.rs     command line
```

```bash
cargo test
```
