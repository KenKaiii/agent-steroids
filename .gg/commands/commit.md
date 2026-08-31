---
name: commit
description: Run checks, agent code review, commit with AI message, and push
---

1. Run quality checks:
   `cargo fmt` (auto-fixes formatting), then `cargo fmt --check`
   `cargo clippy --release --all-targets -- -D warnings`
   `cargo test --release`
   Fix ALL errors before continuing. Re-read any file `cargo fmt` rewrote.

2. Review changes: run git status and git diff --staged and git diff

3. Fast review gate: spawn ONE subagent with the full diff. Instructions: review ONLY
   the diff for real bugs, regressions, leftover debug code, and unintended changes.
   Score each issue 0-100 confidence (pre-existing issues and stylistic nitpicks = false
   positives, score low). Report ONLY issues with confidence >= 80, with file:line and a
   one-line fix. If none, reply "CLEAR". This is a last check, not a deep audit - be fast.

4. If CLEAR: proceed straight to step 5 and push WITHOUT asking the user anything.
   If issues >= 80 were reported: STOP, show the issues, and ask with the `ask_user`
   tool — one `choice` question (`id: "land"`, question "Want me to fix this first, or commit and push anyway?") with:
   - "Fix it first, then commit & push" (recommended, hint: keeps the branch green)
   - "Commit & push anyway" (hint: issue stays open in the log)
   The card is the ONLY ask: show the issues, then stop — do not restate the two options as text or end with an asking line. Only if `ask_user` is unavailable, ask the same two options in prose.
   On fix-first: fix, re-run step 1, then continue (no re-review). Otherwise continue as-is.

5. Stage relevant files with git add (specific files, not -A)

6. Generate a commit message:
   - Start with verb (Add/Update/Fix/Remove/Refactor)
   - Be specific and concise, one line preferred

7. Commit AND push in one go - never pause for confirmation here:
   git commit -m "your generated message"
   git push (first push on a branch: `git push -u origin HEAD`)
   If `git remote -v` is empty there is nowhere to push: commit, say so, and stop.
