# 💉 Agent Steroids

<p align="center">
  <img src="assets/agent-steroids.png" alt="Agent Steroids">
</p>

<p align="center">
  <strong>Live code from real repos, on your machine, for your coding agent.</strong>
</p>

<p align="center">
  <a href="https://github.com/KenKaiii/agent-steroids"><img src="https://img.shields.io/github/stars/KenKaiii/agent-steroids?style=for-the-badge&label=Stars&color=yellow" alt="Star Agent Steroids on GitHub"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Built%20with-Rust-000000?style=for-the-badge&logo=rust&logoColor=white" alt="Built with Rust"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg?style=for-the-badge" alt="MIT License"></a>
  <a href="https://youtube.com/@kenkaidoesai"><img src="https://img.shields.io/badge/YouTube-FF0000?style=for-the-badge&logo=youtube&logoColor=white" alt="YouTube"></a>
  <a href="https://skool.com/kenkai"><img src="https://img.shields.io/badge/Skool-Community-7C3AED?style=for-the-badge" alt="Skool"></a>
</p>

<p align="center">
  macOS · Linux · Windows. One command to <a href="#-get-it">install</a>. No runtime, no daemon, no MCP server.
</p>

---

## 🧠 The problem nobody talks about

Your coding agent learned to code from a snapshot of the internet. That snapshot is old.

It does not matter which model you use. Claude, GPT, Gemini, all of them got trained on a pile of GitHub repos, and then that pile stopped moving. Meanwhile the actual libraries kept shipping. New APIs, new patterns, breaking changes, better ways of doing the thing.

So your agent writes code the way it was written a year ago, and sounds completely confident about it.

You can paste in docs. That helps a bit. But docs tell you what a thing is supposed to do, not how good engineers actually use it in a real project with real edge cases.

## 🚧 And the usual workaround is worse

There are tools that let an agent search public code through an API. They work, right up until you actually lean on them.

- **Rate limits.** Pull a few dozen samples and you are cut off.
- **Slow.** Every single search is a round trip to somebody else's server.
- **Not yours.** They pick what is searchable. The service goes down, so does your agent.

That is fine for looking something up once. It falls apart when your agent wants to check thirty different implementations before writing a function.

## ✅ What this does instead

You build your own library of code. Locally.

Pick the repos you care about. Building an AI framework? Grab fifty AI framework repos. Writing a payments integration? Grab the ones that already did it well. Then your agent searches all of it instantly, as many times as it likes, forever.

No rate limits. No waiting. No API keys. It is a file on your disk.

And because these are live repos, one command pulls the newest code down whenever you want. Your agent stops guessing from memory and starts reading what people are shipping right now.

## 🤯 The part that surprises people

It is tiny.

| | |
|---|---|
| 5 repos | **10 MB** |
| 40 repos | **145 MB** |
| Roughly | **2 to 4 MB per repo** |

We throw away about 90% of every repo before storing it. No images, no docs, no READMEs, no lock files, no bundled dependencies. Just the code worth learning from, squeezed down hard.

Those 40 repos were 382 MB of source. The code stores as 94 MB, and the rest is the search index that makes lookups instant.

Searches come back in **10 to 40 milliseconds** using about **8 MB of memory**. You could keep the whole thing on a USB stick and carry it between machines.

## 🚀 Get it

You need [Rust](https://rustup.rs) installed. Then:

```bash
cargo install --git https://github.com/KenKaiii/agent-steroids
```

That gives you a `steroids` command that works from anywhere. Your library lives in `~/.steroids`.

## 🤖 Just let your agent do it

Honestly, the easiest way to use this is to not use it yourself. Paste this to Claude Code, Cursor, Codex, whatever you run:

```
Install Agent Steroids for me: https://github.com/KenKaiii/agent-steroids

Then look at what I am building, pick around 20 open source repos that solve
similar problems well, and index them. Once that is done, tell me what you
added and how much space it used.
```

That is it. It reads the README, installs it, works out what is relevant to your project, and fills your library.

Then add this to your agent's permanent instructions (`CLAUDE.md`, `.cursorrules`, or wherever your tool keeps them) so it actually keeps using it:

```
I have a local corpus of real open source code at ~/.steroids. Search it
before writing anything non-trivial, to see how other projects solved the
same problem.

  steroids search '<regex>' [--repo R] [--language L] [--limit N]
  steroids define <Symbol>       where something is defined
  steroids show <repo> <path>    read a full file
  steroids repos                 what is indexed

Add --json to search, define or repos for structured output.

If a search says the topic is not covered, tell me which repos to add
instead of guessing. Add them with: steroids add owner/name && steroids index
```

Now when you ask for a rate limiter, it goes and reads four real implementations first instead of recalling one from training.

There is no plugin and no server to run. Your agent just calls the command, same as it calls `grep`.

Results are spread across different repos on purpose, so the agent sees four projects' takes side by side instead of four files from one project. Every snippet is labelled with the function it came from, so it can skip the irrelevant ones without opening anything.

When nothing matches, it says why. Sometimes that answer is "none of your repos cover this, go add some" instead of letting the agent spin.

`search`, `define` and `repos` all take `--json` if you want machine-readable output.

## 📚 Filling it yourself

If you would rather drive it manually:

```bash
steroids add openai/openai-agents-python crewAIInc/crewAI
steroids index
```

Pasting a GitHub URL works too. And you can hand it a text file of hundreds of repos:

```bash
steroids add --from-file repos.txt
```

Not sure what to add? Let it go find things:

```bash
steroids discover 'topic:ai-agents language:python' --add
steroids discover --trending --days 7 --language rust --add
```

## 🖥️ Have a look around

```bash
steroids
```

Run it with nothing after it and you get a proper interface. Browse your repos, open any file, search as you type with results appearing live. Arrow keys to move, `esc` to go back, `q` to quit. Press `a` to add a repo, `d` to remove one, `u` to refresh everything.

Downloads run in the background, so it never freezes on you.

## 🧹 Keeping it fresh

```bash
steroids update      # pull the latest code for everything
steroids repos       # see what you have
steroids stats       # see what it costs you
```

Repos go quiet. You can have those cleaned out automatically:

```bash
steroids config decay_months 6
steroids decay --dry-run
steroids decay
```

That measures from the last actual commit, not from when you added it. Dry run first, always.

## ⚙️ All the settings

```bash
steroids config                  # show everything
steroids config min_stars 500    # change one
```

| Setting | Default | What it does |
|---|---|---|
| `decay_months` | `0` | Drop repos with no commits in N months. 0 means never |
| `decay_archived` | `false` | Also drop repos the owner shut down |
| `auto_discover` | `false` | Top up with new repos on every update |
| `discover_query` | `topic:ai-agents` | What to look for when discovering |
| `discover_limit` | `25` | Cap on how many one discovery run adds |
| `min_stars` | `100` | Skip anything below this |

Settings live inside the library itself, so they travel with it.

## 💾 Taking it with you

The whole thing is one folder. Copy it to an external drive and point at it:

```bash
STEROIDS_ROOT=/Volumes/MyDrive/steroids steroids search 'retry'
```

Same on any machine. Nothing else to set up.

---

## 👨‍💻 For devs

Rust 1.88 or newer. No other dependencies, no runtime, no daemon.

```bash
git clone https://github.com/KenKaiii/agent-steroids.git
cd agent-steroids
cargo install --path .
```

```bash
cargo test --release      # 25 tests
cargo clippy --release --all-targets -- -D warnings
cargo fmt
```

Some tests need a populated corpus and skip without one. Point them at yours:

```bash
STEROIDS_TEST_ROOT=~/.steroids cargo test --release
```

**How it works.** Repos come down as tarballs straight from codeload, which has no rate limit, so a 500 repo ingest is capped by your bandwidth and nothing else. Files are filtered while the download is still streaming, so the full repo never lands on disk. What survives gets compressed against a shared zstd dictionary trained on your own corpus. Search narrows candidates with a non-positional trigram index, then confirms each hit with a real regex, which is why the index costs a fraction of what a normal one would.

The GitHub API is only used for discovery and for the optional star and commit-date metadata that `decay` needs. Plain `add` never touches it.

```
src/filters.rs  what earns disk space
src/fetch.rs    tarball download and filtering
src/bulk.rs     parallel ingest
src/store.rs    compressed content store (sqlite + blobs.bin)
src/index.rs    trigram index
src/search.rs   query: narrow by trigram, verify by regex
src/tui/        the interactive browser
```

Set `GITHUB_TOKEN` to raise the discovery rate limit from 60 to 5,000 requests an hour. Ingest does not need it.

---

## 📄 Licence

MIT. Use it, change it, ship it, sell it. Just keep the copyright notice.

---

## 👥 Come hang out

- [YouTube @kenkaidoesai](https://youtube.com/@kenkaidoesai), tutorials and demos
- [Skool community](https://skool.com/kenkai)

---

<p align="center">
  <strong>Your agent stops guessing. It reads what actually shipped.</strong>
</p>
