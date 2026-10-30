# SkillParity

**CI gate for skill authors:** the same `SKILL.md` + YAML tasks, asserted across harnesses (Claude Code / Cursor / Codex / …) — did it trigger, call the right tools, and produce the right artifact?

Static lint for skills is a red ocean. SkillParity fills the **behavioral** hole after AAIF / first-class skills: a **harness × skill** matrix, not another marketplace.

## Quick start

```bash
npm test
```

That runs unit tests, then the mock harness on `examples/summarize-pr` (`--ci`). The mock run prints `3 pass`.

```bash
node bin/skillparity.js run \
  --skill examples/summarize-pr \
  --parity examples/summarize-pr/parity.yaml \
  --harness mock --ci
```

Other harnesses:

```bash
npm run test:claude   # --skip-ok; green when claude or credentials are absent
npm run test:cursor   # same for the Cursor Agent CLI
```

Report: terminal matrix + `parity-report.json`.

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

Codex is not wired. Cursor is the second harness and already skips without a binary or `CURSOR_API_KEY`. A Codex adapter can follow the same shape later (`codex` CLI, its own skill roots) without changing `parity.yaml`.

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
| `expect.stdout_matches` | optional regex |

Each task runs in `os.tmpdir()` sandbox. No private keys.

## Non-goals

- Skill marketplace / registry
- Full static lint suite (use existing linters)
- Hosted eval farm or multi-model quality scoring
- Replacing harness-native tests

## Status

- **Mock** is the CI gate. `npm test` runs unit tests plus `examples/summarize-pr` × mock (`3 pass`) and the GitHub Action job `mock-parity` uploads `parity-report.json`.
- **Claude** is a real adapter (skill install, `claude -p` print mode, stream-json / transcript parsing), not a stub. It skips cleanly when the CLI or credentials are missing. This repo does not ship a key, and the adapter is not executed against a live model here.
- **Cursor** is the second harness (documented `agent -p` flags and `tool_call` parsing) with the same skip-without-secrets behavior. `claude-parity` and `cursor-parity` in CI use `--skip-ok` and `continue-on-error`, so a missing CLI stays green and a live failure does not block the mock job.
- Not in this repo: marketplace, hosted eval farm, Codex adapter, golden tasks beyond `summarize-pr`.
