# Agent notes

Read `CLAUDE.md` first — it holds the architecture, invariants and test conventions.

## Commands

- Build: `cargo build --release`
- Test: `cargo test --release`
- Lint: `cargo fmt --check && cargo clippy --release --all-targets -- -D warnings`
- CLI surface tests: `BIN=./target/release/steroids bash tests/commands.sh`

## Rules

- CI lives in `.github/workflows/ci.yml` and runs on Linux, macOS and Windows. It must stay green.
- Never commit with `--no-verify`; the pre-push hook runs `cargo fmt --check` for a reason.
