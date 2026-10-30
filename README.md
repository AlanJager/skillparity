# SkillParity

**CI gate for skill authors:** the same `SKILL.md` + YAML tasks, asserted across harnesses (Claude Code / Cursor / …) — did it trigger, call the right tools, and produce the right artifact?

Static lint for skills is a red ocean. SkillParity fills the **behavioral** hole: a **harness × skill** matrix, not another marketplace.

The CLI is a Rust binary. It replaces the Node 0.2.0 implementation and keeps the same `examples/summarize-pr` tasks, harnesses, assertions, and report shape.

## Quick start

Requires Rust 1.85+ (stable is fine).

```bash
cargo test
```

That runs unit tests, then the mock harness on `examples/summarize-pr` (`--ci`). The mock run prints `3 pass`.

```bash
cargo run -- run \
  --skill examples/summarize-pr \
  --parity examples/summarize-pr/parity.yaml \
  --harness mock --ci
```

Release binary:

```bash
cargo build --release
./target/release/skillparity run \
  --skill examples/summarize-pr \
  --parity examples/summarize-pr/parity.yaml \
  --harness mock --ci
```

Other harnesses (green when the CLI or credentials are absent):

```bash
cargo run -- run \
  --skill examples/summarize-pr \
  --parity examples/summarize-pr/parity.yaml \
  --harness claude --ci --skip-ok

cargo run -- run \
  --skill examples/summarize-pr \
  --parity examples/summarize-pr/parity.yaml \
  --harness cursor --ci --skip-ok
```

Report: terminal matrix + `parity-report.json` (or `--out <dir>/parity-report.json`).

## Harnesses

| harness | when it runs | when it skips |
|---------|----------------|---------------|
| `mock` | always (CI gate) | never |
| `claude` | `claude` on `PATH` and a login or `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` | CLI missing, or CLI present without credentials |
| `cursor` | `cursor-agent` or a Cursor-identified `agent`, plus a login or `CURSOR_API_KEY` | CLI missing, `agent` is not Cursor, or no credentials |

No network secrets are required for `mock`. Claude and Cursor skip with an explicit reason when the CLI or credentials are absent, so CI stays green without them.

`--skip-ok` (with `--ci`) exits 0 when every task was skipped. Without it, an all-skip run exits 1.

### Claude Code

Installs the skill at `<sandbox>/.claude/skills/<name>/SKILL.md` (supporting files included; `fixtures/` and `parity.yaml` omitted) and writes `.claude/settings.json` allowing Read, Write, Edit, Glob, Grep, and Skill.

Invocation (print mode, stdin ignored):

```bash
claude -p "<prompt>" \
  --verbose \
  --output-format stream-json \
  --permission-mode acceptEdits \
  --allowedTools Read,Write,Edit,Glob,Grep,Skill \
  --max-turns 8
```

`--bare` is not used: bare mode does not present project skills to the model. `acceptEdits` is the default so file writes do not prompt; `SKILLPARITY_CLAUDE_PERMISSION_MODE=bypassPermissions` is opt-in for an isolated VM. The prompt is not prefixed with `/skill-name`, so a task can still assert that the skill did **not** trigger.

Parsing prefers stream-json `tool_use` blocks (and a single `--output-format json` result). If stdout has a session id but no tools, the adapter also reads `~/.claude/projects/<slug>/<session-id>.jsonl`. A `summary.md` written in the sandbox counts as `Write`. `Read` is recorded only when the transcript says so.

### Cursor

Second harness, using the public Agent CLI (`agent` / `cursor-agent`):

```bash
agent -p "<prompt>" --force --trust \
  --output-format stream-json --workspace <sandbox>
```

The skill is installed at `.cursor/skills/<name>/` and `.agents/skills/<name>/`. Project permissions go in `.cursor/cli.json`. `readToolCall` / `writeToolCall` (and similar) events are normalized to `Read` / `Write` so `parity.yaml` stays harness-neutral. `--force` is what applies file writes in print mode; `--trust` skips the workspace trust prompt.

Codex is not wired. A Codex adapter can follow the same shape later without changing `parity.yaml`.

## Example skill

`examples/summarize-pr` — on “summarize this PR”, read local `pr.diff`, write `summary.md` (title + 3 bullets). `parity.yaml` has three tasks: trigger+write, no-trigger on unrelated ask, missing `pr.diff` → clear failure message.

## parity.yaml schema (per task)

| field | meaning |
|--------|---------|
| `id` | task id |
| `prompt` | user utterance |
| `fixture` | optional dir copied into a temp sandbox |
| `expect.skill_triggered` | bool |
| `expect.called_tools` | optional list |
| `expect.not_called_tools` | optional list |
| `expect.files_exist` | optional list (empty = must not leave `summary.md`) |
| `expect.stdout_matches` | optional regex (`(?i)` is honored) |

Each task runs in a temp sandbox. No private keys.

## CLI

```text
skillparity run --skill <dir> --parity <yaml> [--harness mock|claude|cursor] [--ci] [--skip-ok] [--out <dir>]
skillparity help
```

Exit codes: `0` success (or all-skip with `--skip-ok`), `1` a task failed under `--ci` (or every task skipped without `--skip-ok`), `2` usage or input error.

## Non-goals

- Skill marketplace / registry
- Full static lint suite (use existing linters)
- Hosted eval farm or multi-model quality scoring
- Replacing harness-native tests

## Status

- **Mock** is the CI gate. `cargo test` runs unit tests plus `examples/summarize-pr` × mock (`3 pass`). The GitHub Action job `mock-parity` uploads `parity-report.json`.
- **Claude** is a real adapter (skill install, `claude -p` print mode, stream-json / transcript parsing), not a stub. It skips cleanly when the CLI or credentials are missing. This repo does not ship a key.
- **Cursor** is the second harness (documented `agent -p` flags and `tool_call` parsing) with the same skip-without-secrets behavior. `claude-parity` and `cursor-parity` in CI use `--skip-ok` and `continue-on-error`, so a missing CLI stays green and a live failure does not block the mock job.
- Not in this repo: marketplace, hosted eval farm, Codex adapter, golden tasks beyond `summarize-pr`, the retired Node CLI.
