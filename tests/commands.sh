#!/usr/bin/env bash
# Exercises every command an agent can run, including its failure paths.
#
# Each case asserts on exit code as well as output, because a command that
# prints an error and exits 0 looks like success to a caller checking $?.
#
#   bash tests/commands.sh
#   BIN=./target/release/steroids bash tests/commands.sh
set -uo pipefail

BIN="${BIN:-steroids}"
ROOT="$(mktemp -d)"
export STEROIDS_ROOT="$ROOT"
# The suite must never reach for a release: no nudge, no binary swap.
export STEROIDS_NO_UPGRADE=1
PASS=0
FAIL=0

trap 'rm -rf "$ROOT"' EXIT

# ok <name> <expected-exit> <must-contain> -- <command...>
ok() {
  local name="$1" want="$2" needle="$3"; shift 4
  local out code
  out=$(timeout 300 "$@" 2>&1); code=$?
  if [ "$code" != "$want" ]; then
    printf '  FAIL %-44s exit %s, wanted %s\n' "$name" "$code" "$want"
    printf '       got: %s\n' "$(head -c 100 <<<"$out")"
    FAIL=$((FAIL + 1)); return
  fi
  if [ -n "$needle" ] && ! grep -qi -- "$needle" <<<"$out"; then
    printf '  FAIL %-44s missing %q\n' "$name" "$needle"
    printf '       got: %s\n' "$(head -c 100 <<<"$out")"
    FAIL=$((FAIL + 1)); return
  fi
  printf '  ok   %s\n' "$name"
  PASS=$((PASS + 1))
}

echo "=== empty corpus: every read command must behave ==="
ok "repos on empty"        0 "no repositories"    -- "$BIN" repos
ok "stats on empty"        0 "repositories"       -- "$BIN" stats
ok "audit on empty"        0 "repositories : 0"   -- "$BIN" audit
ok "audit --json on empty" 0 '"suspect_directories"' -- "$BIN" audit --json
ok "tag list on empty"     0 "no tags"            -- "$BIN" tag
ok "config on empty"       0 "min_stars"          -- "$BIN" config
ok "search on empty"       0 "empty"              -- "$BIN" search "anything"
ok "define on empty"       0 "empty"              -- "$BIN" define Thing
ok "recent on empty"       0 "no repositories"    -- "$BIN" recent --hours 24
ok "index on empty"        0 "documents"          -- "$BIN" index
ok "compact on empty"      0 "reclaimed"          -- "$BIN" compact
ok "upgrade check offline" 0 "skipped"            -- "$BIN" upgrade --check
ok "decay off by default"  0 "archived"           -- "$BIN" decay
ok "files, unknown repo"   1 "not in corpus"      -- "$BIN" files ghost/repo
ok "files json, unknown"   1 '{"error"'          -- "$BIN" files ghost/repo --json
ok "show, unknown repo"    1 "not in corpus"      -- "$BIN" show ghost/repo a.py
ok "remove, unknown repo"  1 "not in corpus"      -- "$BIN" remove ghost/repo

echo
echo "=== bad input must fail loudly, not silently ==="
ok "add with no arguments"  2 "no repositories given"      -- "$BIN" add
ok "add malformed name"     1 "failed"                     -- "$BIN" add "not a repo"
ok "add traversal attempt"  1 "failed"                     -- "$BIN" add "../../etc"
ok "search invalid regex"   1 "invalid pattern"            -- "$BIN" search "("
ok "search limit zero"      1 "at least 1"                 -- "$BIN" search x --limit 0
ok "config unknown key"     1 "unknown setting"            -- "$BIN" config nonsense 1
ok "config bad number"      1 "whole number"               -- "$BIN" config decay_months soon
ok "config bad boolean"     1 "true or false"              -- "$BIN" config decay_archived maybe
ok "config empty query"     1 "cannot be empty"            -- "$BIN" config discover_query ""
ok "unknown tag on search"  0 "no repositories are tagged" -- "$BIN" search x --tag ghost
ok "tag an unknown repo"       1 "not in corpus"              -- "$BIN" tag --add x ghost/repo
ok "config unknown key read"   1 "unknown setting"            -- "$BIN" config nosuchkey
ok "empty --tag on search"     1 "must not be empty"          -- "$BIN" search x --tag ""
ok "lookaround explained"      1 "look-around"                -- "$BIN" search 'foo(?=bar)'
ok "json error is json"        1 '{"error"'                   -- "$BIN" search "" --json
ok "add with ref refused"      1 "not supported"              -- "$BIN" add owner/repo@v1
ok "add subpath refused"       1 "not supported"              -- "$BIN" add https://github.com/o/r/tree/main/src
ok "discover limit 0 refused"  1 "at least 1"                 -- "$BIN" discover x --limit 0
ok "per-repo 0 refused"        1 "at least 1"                 -- "$BIN" search x --per-repo 0
ok "tiny token budget refused" 1 "at least 50"                -- "$BIN" search x --max-tokens 10
ok "one-char symbol refused"   1 "at least 2"                 -- "$BIN" define x

# An agent has neither a terminal in nor out. The browser must not try to
# draw on a pipe; help and a distinct exit code tell it what to do instead.
no_tty_out=$("$BIN" </dev/null 2>&1 | cat); no_tty_code=${PIPESTATUS[0]}
if [ "$no_tty_code" = 2 ] && grep -q "Usage" <<<"$no_tty_out"; then
  echo "  ok   no subcommand without a tty"; PASS=$((PASS + 1))
else
  echo "  FAIL no subcommand without a tty (exit $no_tty_code)"; FAIL=$((FAIL + 1))
fi

echo
echo "=== a real repository, end to end ==="
ok "add"                       0 "files kept"    -- "$BIN" add antirez/smallchat
# add indexes before it returns. Searching straight after used to be told the
# topic was absent and to go discover repositories, with the code sitting
# there unindexed; that was the single worst trap for an agent.
ok "searchable right after add" 0 "match(es)"     -- "$BIN" search "listen" --repo antirez/smallchat
ok "index"                     0 "documents"     -- "$BIN" index
ok "repos lists it"            0 "smallchat"     -- "$BIN" repos
# The dominant language is stored rather than derived, so a listing after an
# ingest must still show it: a stale or empty column is silent.
ok "repos shows the language"  0 "c "            -- "$BIN" repos
# Matching runs per line, so a newline pattern is unmatchable rather than
# merely absent. Saying "add more repositories" here sends agents chasing
# repositories that would never have helped.
ok "newline pattern explained" 0 "one line at a time" -- "$BIN" search 'try:\n' 
# Inputs an agent will eventually send by accident. Each used to succeed
# quietly or, for the reversed range, panic.
ok "empty pattern refused"     1 "must not be empty" -- "$BIN" search ""
ok "empty symbol refused"      1 "must not be empty" -- "$BIN" define ""
ok "reversed range refused"    1 "before --from"     -- "$BIN" show antirez/smallchat chatlib.c --from 50 --to 10
ok "show limit 0 refused"      1 "at least 1"        -- "$BIN" show antirez/smallchat chatlib.c --limit 0
ok "empty tag refused"         1 "must not be empty" -- "$BIN" tag antirez/smallchat --add ""
ok "spaced tag refused"        1 "spaces or commas"  -- "$BIN" tag antirez/smallchat --add "a b"
ok "tagging nothing fails"     1 "not in corpus"     -- "$BIN" tag nope/nope --add x
ok "deep bare path refused"    1 "expected owner/name" -- "$BIN" add owner/repo/extra
ok "repos --json"              0 '"repo"'        -- "$BIN" repos --json
ok "repos --json counts"       0 '"count"'       -- "$BIN" repos --json
ok "repos limit 0 refused"     1 "at least 1"    -- "$BIN" repos --limit 0
ok "files"                     0 "chat"          -- "$BIN" files antirez/smallchat
ok "search finds code"         0 "match"         -- "$BIN" search "int main" --limit 2
ok "search --json"             0 '"matches"'     -- "$BIN" search "int main" --json --limit 1
ok "search --repo"             0 "match"         -- "$BIN" search "int" --repo antirez/smallchat --limit 1
ok "search --repo any case"    0 "match"         -- "$BIN" search "int" --repo ANTIREZ/SmallChat --limit 1
ok "search --repo url form"    0 "match"         -- "$BIN" search "int" --repo https://github.com/antirez/smallchat --limit 1
ok "search --repo list"        0 "match"         -- "$BIN" search "int" --repo antirez/smallchat,antirez/smallchat --limit 1
ok "search --repo unknown in list" 0 "ghost/x.*not in this corpus" -- "$BIN" search "int" --repo antirez/smallchat,ghost/x --limit 1
ok "search --language"         0 "match"         -- "$BIN" search "int" --language c --limit 1
ok "search --language alias"   0 "match"         -- "$BIN" search "int" --language C --limit 1
ok "search --path prefix"      0 "no indexed file path" -- "$BIN" search "int" --path src --limit 1
ok "search --path bracket"     0 "no indexed file path" -- "$BIN" search "int" --path '[' --limit 1
ok "index --refilter"          0 "dropped"       -- "$BIN" index --refilter
ok "--include-tests needs it"  2 "refilter"      -- "$BIN" index --include-tests
ok "search fixed string"       0 "match"         -- "$BIN" search -F 'main(' --limit 1
ok "truncation says N of M"    0 "of .* shown"   -- "$BIN" search "int" --max-tokens 50 --limit 5
ok "files --json"              0 '"files"'       -- "$BIN" files antirez/smallchat --json
ok "files any case"            0 "chat"          -- "$BIN" files ANTIREZ/smallchat
ok "show --json"               0 '"total_lines"' -- "$BIN" show antirez/smallchat smallchat-server.c --json --from 1 --to 3
ok "define never defined"      0 "referenced"    -- "$BIN" define printf
ok "search --ignore-case"      0 "match"         -- "$BIN" search "INT MAIN" -i --limit 1
ok "search --path glob"        0 ""              -- "$BIN" search "int" --path "*.c" --limit 1
ok "search --path matches none" 0 "no indexed file path" -- "$BIN" search "int" --path "ghost/**" --limit 1
ok "search --include-comments" 0 ""              -- "$BIN" search "the" --include-comments --limit 1
ok "search -C wide context"    0 "match"         -- "$BIN" search "int main" -C 10 --limit 1
ok "search -C 0"               0 "match"         -- "$BIN" search "int main" -C 0 --limit 1
ok "search --per-repo"         0 "match"         -- "$BIN" search "int" --per-repo 1 --limit 3
ok "search --max-tokens"       0 "match"         -- "$BIN" search "int" --max-tokens 500 --limit 20
ok "show --from/--to"          0 "lines"         -- "$BIN" show antirez/smallchat smallchat-server.c --from 10 --to 20
ok "show past end of file"     0 "nothing at"    -- "$BIN" show antirez/smallchat smallchat-server.c --from 999999
ok "recent, repo not indexed"  0 "not in this corpus" -- "$BIN" recent --repo ghost/x --hours 24
ok "search json has line map"  0 "context_first_line" -- "$BIN" search "int main" --json --limit 1
ok "search json honours budget" 0 '"omitted": 1' -- "$BIN" search "int" --json --max-tokens 50 --limit 2
ok "define"                    0 ""              -- "$BIN" define main --limit 1
ok "define --json"             0 ""              -- "$BIN" define main --json --limit 1
ok "show"                      0 "smallchat"     -- "$BIN" show antirez/smallchat smallchat-server.c
ok "tag it"                    0 "tagged 1"      -- "$BIN" tag --add demo antirez/smallchat
ok "tag list shows it"         0 "demo"          -- "$BIN" tag
ok "repos --tag"               0 "smallchat"     -- "$BIN" repos --tag demo
ok "search --tag"              0 ""              -- "$BIN" search "int" --tag demo --limit 1
ok "tag partial miss fails"    1 "not in corpus" -- "$BIN" tag --add extra antirez/smallchat ghost/x
ok "tag --remove"              0 "untagged 1"    -- "$BIN" tag --remove extra antirez/smallchat
ok "removed tag is gone"       0 "no repositories are tagged" -- "$BIN" search int --tag extra
ok "stats"                     0 "total on disk" -- "$BIN" stats

# A filter that excludes everything is not a failed search. Saying "no matches"
# sends the caller rewriting a query that was never the problem.
ok "unknown --repo names itself"     0 "not in this corpus"   -- "$BIN" search int --repo ghost/x
ok "absent --language names itself"  0 "no cobol files"       -- "$BIN" search int --language cobol
ok "unknown --tag names itself"      0 "no repositories are tagged" -- "$BIN" search int --tag ghost
ok "filter miss stays json"         0 '"reason": "filter_excludes_all"' -- "$BIN" search int --repo ghost/x --json
ok "valid --repo still searches"     0 "match"                -- "$BIN" search int --repo antirez/smallchat --limit 1
ok "update"                    0 "up to date"    -- "$BIN" update
ok "compact"                   0 "reclaimed"     -- "$BIN" compact
ok "remove"                    0 "removed"       -- "$BIN" remove antirez/smallchat
ok "gone after remove"         0 "no repositories" -- "$BIN" repos

echo
echo "=== idempotence: repeating a command must be safe ==="
ok "re-add same repo"      0 "files kept" -- "$BIN" add antirez/smallchat
ok "add duplicate names"   0 "files kept" -- "$BIN" add antirez/smallchat antirez/smallchat
ok "index twice"           0 "documents"  -- "$BIN" index
ok "compact twice"         0 "reclaimed"  -- "$BIN" compact
ok "url form of same repo" 0 "files kept" -- "$BIN" add "https://github.com/antirez/smallchat.git"
ok "still one repository"  0 "1 repositories" -- "$BIN" repos
ok "case variant of same repo" 0 "files kept" -- "$BIN" add ANTIREZ/SMALLCHAT
ok "still one after case variant" 0 "1 repositories" -- "$BIN" repos
# The spelling already stored wins over the one just typed: tags and scripts
# refer to it. (`ok` compares case-insensitively, so check by hand.)
if "$BIN" repos | grep -q "antirez/smallchat"; then
  echo "  ok   stored spelling survives a case variant"; PASS=$((PASS + 1))
else
  echo "  FAIL stored spelling survives a case variant"; FAIL=$((FAIL + 1))
fi

# Piping into head closes stdout early. Every Unix tool exits quietly; a
# panic with exit 101 is what an agent used to see.
head_out=$("$BIN" files antirez/smallchat --limit 999999 2>&1 | head -1); head_code=${PIPESTATUS[0]}
if [ "$head_code" = 0 ] && ! grep -q "panicked" <<<"$head_out"; then
  echo "  ok   pipe to head exits 0"; PASS=$((PASS + 1))
else
  echo "  FAIL pipe to head exits 0 (exit $head_code)"; FAIL=$((FAIL + 1))
fi

echo
printf '  %s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
