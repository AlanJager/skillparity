# SkillParity

**CI gate for skill authors:** the same `SKILL.md` + YAML tasks, asserted across harnesses (Claude Code / Cursor / Codex / Grok) — did it trigger, call the right tools, and produce the right artifact?

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

cargo run -- run \
  --skill examples/summarize-pr \
  --parity examples/summarize-pr/parity.yaml \
  --harness codex --ci --skip-ok

cargo run -- run \
  --skill examples/summarize-pr \
  --parity examples/summarize-pr/parity.yaml \
  --harness grok --ci --skip-ok
```

Report: terminal matrix + `parity-report.json` (or `--out <dir>/parity-report.json`).

## Harnesses

| harness | when it runs | when it skips |
|---------|----------------|---------------|
| `mock` | always (CI gate) | never |
| `claude` | `claude` on `PATH` and a login or `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` | CLI missing, or CLI present without credentials |
| `cursor` | `cursor-agent` or a Cursor-identified `agent`, plus a login or `CURSOR_API_KEY` | CLI missing, `agent` is not Cursor, or no credentials |
| `codex` | `codex` on `PATH` whose `exec --help` describes the Codex CLI, plus `CODEX_API_KEY` / `CODEX_ACCESS_TOKEN` or `codex login status` | CLI missing, help is not Codex, or no credentials |
| `grok` | `grok` on `PATH` whose help describes the xAI CLI (`--single` or `--output-format`), plus `XAI_API_KEY` or a non-empty `auth.json` | CLI missing, help is not Grok, or no credentials |

No network secrets are required for `mock`. Claude, Cursor, Codex, and Grok skip with an explicit reason when the CLI or credentials are absent, so CI stays green without them. A rejected key (`Invalid API key`) is a failure, not a skip.

`--skip-ok` (with `--ci`) exits 0 when every task was skipped. Without it, an all-skip run exits 1. One real failure under `--ci` is still exit 1.

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

### Codex

Non-interactive [Codex CLI](https://developers.openai.com/codex/noninteractive) (`codex exec`). The skill is installed at `.agents/skills/<name>/` inside the sandbox (repo skill root). The sandbox is a temp directory, so the run passes `--skip-git-repo-check`. `approval_policy=never` with `--sandbox workspace-write` allows workspace edits without an approval prompt. `--ephemeral` avoids writing a session rollout. Stdin is ignored.

```bash
codex exec --json --ephemeral --skip-git-repo-check \
  --sandbox workspace-write \
  -c approval_policy=never \
  --cd <sandbox> "<prompt>"
```

`--json` is JSONL (`thread.started`, `item.*`, `turn.completed` / `turn.failed`). A `command_execution` that names a file counts as `Read`. A completed `file_change` (`add` / `update`) counts as `Write`. `parity.yaml` stays harness-neutral. `CODEX_API_KEY` or `CODEX_ACCESS_TOKEN` counts as credentials; otherwise `codex login status` (exit 0, and not a logged-out message) does. `codex exec --help` is capped at 8s and `codex login status` at 15s. The query uses `SKILLPARITY_TIMEOUT_MS` (default 180s).

### Grok

Headless [xAI Grok CLI](https://docs.x.ai/build/cli/headless-scripting) (`grok -p`). The skill is installed at `.grok/skills/<name>/` (project-local discovery). `--always-approve` lets file writes proceed when nobody is present to approve them. `--no-auto-update` skips the updater. `--max-turns` defaults to 8 (`SKILLPARITY_GROK_MAX_TURNS`).

```bash
grok --no-auto-update \
  --cwd <sandbox> \
  --output-format streaming-json \
  --always-approve \
  --max-turns 8 \
  -p "<prompt>"
```

`streaming-json` events are JSONL. `toolName` values such as `read_file` / `write_file` normalize to `Read` / `Write`. Text chunks are concatenated. `XAI_API_KEY` counts as credentials. Otherwise a non-empty `auth.json` under `$GROK_HOME` or `~/.grok` does (the file is not read, only checked for presence). The query uses the same `SKILLPARITY_TIMEOUT_MS` bound. A live auth failure such as `AuthorizationRequired` with no tool calls is a skip, not a pass.

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
skillparity run --skill <dir> --parity <yaml> [--harness mock|claude|cursor|codex|grok] [--ci] [--skip-ok] [--out <dir>]
skillparity help
```

| code | when |
|------|------|
| 0 | every asserted task passed; or `--ci` was omitted (the report is still written); or `--ci --skip-ok` and every task was skipped |
| 1 | `--ci` and at least one task failed; or `--ci` without `--skip-ok` and every task was skipped |
| 2 | usage error, unknown command, or unreadable skill / parity input |

`--ci` is what turns assertion failures into a non-zero status. `--skip-ok` changes only the all-skipped case. A skipped task is not a failure by itself. Query processes are bounded by `SKILLPARITY_TIMEOUT_MS` (default 180000). A timeout is a task failure, not a skip.

## Non-goals

- Skill marketplace / registry
- Full static lint suite (use existing linters)
- Hosted eval farm or multi-model quality scoring
- Replacing harness-native tests

## Status

- **Mock** is the CI gate. `cargo test` runs unit tests plus `examples/summarize-pr` × mock (`3 pass`). The GitHub Action job `mock-parity` uploads `parity-report.json`.
- **Claude** is a real adapter (skill install, `claude -p` print mode, stream-json / transcript parsing), not a stub. It skips cleanly when the CLI or credentials are missing. This repo does not ship a key.
- **Cursor** is the second harness (documented `agent -p` flags and `tool_call` parsing) with the same skip-without-secrets behavior.
- **Codex** runs `codex exec --json` with workspace-write and `approval_policy=never`. It skips when the CLI or credentials are missing.
- **Grok** runs `grok -p` with `streaming-json` and `--always-approve`. It skips when the CLI is missing or when neither `XAI_API_KEY` nor `auth.json` is present.
- `claude-parity`, `cursor-parity`, `codex-parity`, and `grok-parity` in CI use `--skip-ok` and `continue-on-error`, so a missing CLI stays green and a live failure does not block the mock job.
- Not in this repo: marketplace, hosted eval farm, Waner, golden tasks beyond `summarize-pr`, the retired Node CLI.
