# agent

The Rust crate lives in `source/`; run cargo there.

Before a commit, run the three checks in `CONTRIBUTING.md`, the same ones CI runs. Plain `cargo clippy` passes what CI fails: CI adds `--all-targets --locked -- -D warnings` and `cargo fmt --check`.

Try the TUI on a throwaway home: `cargo build && tui run t "env HOME=/tmp/agent-home $PWD/target/debug/agent"`. Its settings land in `/tmp/agent-home/.agents/settings.json`.

Release, straight on `main`: bump `version` in `source/Cargo.toml`, run `cargo update -p agent --offline`, commit `chore(release): X.Y.Z`, tag `vX.Y.Z`, push `main` and then the tag, follow the run with `gh run watch --exit-status`. `release.yml` fails when the tag differs from the crate version.

## Agent skills

### Issue tracker

GitHub Issues in `rokartur/agent`, through `gh`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five default labels: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
