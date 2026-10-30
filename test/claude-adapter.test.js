import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  assessClaudeAvailability,
  buildClaudeArgs,
  claudeProjectSlug,
  claudeTranscriptPath,
  parseClaudeOutput,
  runTask,
} from "../src/adapters/claude.js";
import { installClaudeSkill } from "../src/skill-fs.js";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exampleSkill = path.join(root, "examples", "summarize-pr");

const STREAM = [
  JSON.stringify({ type: "system", subtype: "init", session_id: "sess-1", tools: ["Read", "Write", "Skill"] }),
  JSON.stringify({
    type: "user",
    message: { role: "user", content: [{ type: "text", text: "Please summarize this PR using the local pr.diff file." }] },
  }),
  JSON.stringify({
    type: "assistant",
    message: {
      role: "assistant",
      content: [
        { type: "text", text: "I'll use the summarize-pr skill." },
        { type: "tool_use", name: "Skill", input: { skill: "summarize-pr" } },
      ],
    },
  }),
  JSON.stringify({
    type: "assistant",
    message: {
      role: "assistant",
      content: [{ type: "tool_use", name: "Read", input: { file_path: "pr.diff" } }],
    },
  }),
  JSON.stringify({
    type: "assistant",
    message: {
      role: "assistant",
      content: [
        {
          type: "tool_use",
          name: "Write",
          input: { file_path: "summary.md", content: "# Update src/hello.js\n" },
        },
      ],
    },
  }),
  JSON.stringify({
    type: "result",
    subtype: "success",
    is_error: false,
    result: "Wrote summary.md",
    session_id: "sess-1",
  }),
].join("\n");

test("parseClaudeOutput reads stream-json tool_use, skill, and write path", () => {
  const parsed = parseClaudeOutput(STREAM, "", { skillName: "summarize-pr" });
  assert.deepEqual(parsed.called_tools, ["Skill", "Read", "Write"]);
  assert.equal(parsed.skill_triggered, true);
  assert.equal(parsed.session_id, "sess-1");
  assert.equal(parsed.is_error, false);
  assert.ok(parsed.artifacts.some((a) => a.path === "summary.md"));
  assert.match(parsed.stdout, /Wrote summary\.md/);
  assert.match(parsed.stdout, /summarize-pr skill/);
  assert.doesNotMatch(parsed.stdout, /Please summarize this PR/);
});

test("parseClaudeOutput does not treat prose or the tool catalog as calls", () => {
  const prose = JSON.stringify({
    type: "assistant",
    message: { role: "assistant", content: [{ type: "text", text: "I will Write a poem about the weather." }] },
  });
  const parsed = parseClaudeOutput(prose, "Tool catalog: Read Write", { skillName: "summarize-pr" });
  assert.deepEqual(parsed.called_tools, []);
  assert.equal(parsed.skill_triggered, false);
});

test("parseClaudeOutput plain-text fallback is explicit and does not imply a skill", () => {
  const parsed = parseClaudeOutput("Using tool: Read\nTool: Write\n", "", {
    skillName: "summarize-pr",
  });
  assert.deepEqual(parsed.called_tools, ["Read", "Write"]);
  assert.equal(parsed.skill_triggered, false);
});

test("parseClaudeOutput treats a Read of SKILL.md as a trigger", () => {
  const stdout = JSON.stringify({
    type: "assistant",
    message: {
      role: "assistant",
      content: [
        {
          type: "tool_use",
          name: "Read",
          input: { file_path: ".claude/skills/summarize-pr/SKILL.md" },
        },
      ],
    },
  });
  const parsed = parseClaudeOutput(stdout, "", { skillName: "summarize-pr" });
  assert.equal(parsed.skill_triggered, true);
  assert.deepEqual(parsed.called_tools, ["Read"]);
});

test("parseClaudeOutput ignores a different skill name", () => {
  const stdout = JSON.stringify({
    type: "assistant",
    message: {
      content: [{ type: "tool_use", name: "Skill", input: { skill: "other-skill" } }],
    },
  });
  const parsed = parseClaudeOutput(stdout, "", { skillName: "summarize-pr" });
  assert.equal(parsed.skill_triggered, false);
  assert.deepEqual(parsed.called_tools, ["Skill"]);
});

test("parseClaudeOutput accepts a single JSON result object", () => {
  const stdout = JSON.stringify({
    type: "result",
    result: "pr.diff is missing",
    is_error: false,
    session_id: "abc",
  });
  const parsed = parseClaudeOutput(stdout, "");
  assert.equal(parsed.stdout, "pr.diff is missing");
  assert.equal(parsed.session_id, "abc");
  assert.equal(parsed.skill_triggered, false);
});

test("buildClaudeArgs is non-interactive print mode without bare or bypass", () => {
  const args = buildClaudeArgs("Summarize the PR");
  assert.deepEqual(args.slice(0, 2), ["-p", "Summarize the PR"]);
  assert.ok(args.includes("--verbose"));
  assert.ok(args.includes("--output-format"));
  assert.equal(args[args.indexOf("--output-format") + 1], "stream-json");
  assert.equal(args[args.indexOf("--permission-mode") + 1], "acceptEdits");
  assert.equal(args[args.indexOf("--allowedTools") + 1], "Read,Write,Edit,Glob,Grep,Skill");
  assert.equal(args.includes("--bare"), false);
  assert.equal(args.includes("--dangerously-skip-permissions"), false);
  assert.equal(args.includes("bypassPermissions"), false);
});

test("claude project slug and transcript path", () => {
  assert.equal(claudeProjectSlug("/tmp/skillparity-abc"), "-tmp-skillparity-abc");
  const file = claudeTranscriptPath({
    cwd: "/tmp/skillparity-abc",
    sessionId: "sess-9",
    home: "/home/me",
  });
  assert.equal(file, "/home/me/.claude/projects/-tmp-skillparity-abc/sess-9.jsonl");
});

test("assessClaudeAvailability skips when the CLI or credentials are missing", () => {
  const missing = assessClaudeAvailability({ bin: null, auth: { loggedIn: false }, env: {} });
  assert.equal(missing.skipped, true);
  assert.match(missing.skip_reason, /not found on PATH/);

  const noAuth = assessClaudeAvailability({
    bin: "/usr/bin/claude",
    auth: { loggedIn: false },
    env: {},
  });
  assert.match(noAuth.skip_reason, /no credentials/);

  const key = assessClaudeAvailability({
    bin: "/usr/bin/claude",
    auth: { loggedIn: false },
    env: { ANTHROPIC_API_KEY: "test-key" },
  });
  assert.equal(key.ok, true);
});

test("runTask skips before install when claude is absent", async () => {
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), "sp-claude-skip-"));
  const result = await runTask({
    sandboxDir: sandbox,
    skillDir: exampleSkill,
    task: { prompt: "hi" },
    skillName: "summarize-pr",
    claudeBin: null,
    env: {},
  });
  assert.equal(result.skipped, true);
  assert.match(result.skip_reason, /not found on PATH/);
  assert.equal(fs.existsSync(path.join(sandbox, ".claude")), false);
});

test("runTask installs the skill and parses an injected claude process", async () => {
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), "sp-claude-run-"));
  fs.writeFileSync(path.join(sandbox, "pr.diff"), "diff\n");
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "sp-claude-home-"));
  let captured;
  const result = await runTask({
    sandboxDir: sandbox,
    skillDir: exampleSkill,
    task: { id: "trigger-and-write", prompt: "Please summarize this PR using the local pr.diff file." },
    skillName: "summarize-pr",
    claudeBin: "/opt/claude",
    env: {},
    homeDir: home,
    authProbe: () => ({ loggedIn: true }),
    spawnImpl(bin, args, opts) {
      captured = { bin, args, opts };
      fs.writeFileSync(path.join(sandbox, "summary.md"), "# Update src/hello.js\n\n- a\n- b\n- c\n");
      return { status: 0, stdout: STREAM, stderr: "", error: null };
    },
  });

  assert.equal(result.skipped, undefined);
  assert.equal(result.error, undefined);
  assert.equal(result.skill_triggered, true);
  assert.deepEqual(result.called_tools, ["Skill", "Read", "Write"]);
  assert.match(result.files["summary.md"], /# Update src\/hello\.js/);
  assert.equal(captured.bin, "/opt/claude");
  assert.equal(captured.opts.stdio[0], "ignore");
  assert.equal(captured.opts.cwd, sandbox);
  assert.equal(captured.args[captured.args.indexOf("--permission-mode") + 1], "acceptEdits");
  assert.equal(fs.existsSync(path.join(sandbox, ".claude", "skills", "summarize-pr", "SKILL.md")), true);
  assert.equal(fs.existsSync(path.join(sandbox, ".claude", "skills", "summarize-pr", "fixtures")), false);
  assert.equal(fs.existsSync(path.join(sandbox, ".claude", "skills", "summarize-pr", "parity.yaml")), false);
  const settings = JSON.parse(fs.readFileSync(path.join(sandbox, ".claude", "settings.json"), "utf8"));
  assert.equal(settings.permissions.defaultMode, "acceptEdits");
  assert.ok(settings.permissions.allow.includes("Read"));
  assert.ok(settings.permissions.allow.includes("Write"));
  assert.ok(settings.permissions.allow.includes("Skill"));
});

test("runTask reads a session transcript when stdout has no tool events", async () => {
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), "sp-claude-jsonl-"));
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "sp-claude-jsonl-home-"));
  const sessionId = "11111111-1111-4111-8111-111111111111";
  const transcript = claudeTranscriptPath({ cwd: sandbox, sessionId, home });
  fs.mkdirSync(path.dirname(transcript), { recursive: true });
  fs.writeFileSync(
    transcript,
    JSON.stringify({
      type: "assistant",
      message: {
        content: [{ type: "tool_use", name: "Read", input: { file_path: "pr.diff" } }],
      },
    }) + "\n"
  );
  const stdout = JSON.stringify({
    type: "result",
    subtype: "success",
    is_error: false,
    result: "pr.diff is missing or empty",
    session_id: sessionId,
  });
  const result = await runTask({
    sandboxDir: sandbox,
    skillDir: exampleSkill,
    task: { prompt: "Summarize the pull request from pr.diff." },
    claudeBin: "/opt/claude",
    env: { ANTHROPIC_API_KEY: "test-key" },
    homeDir: home,
    spawnImpl: () => ({ status: 0, stdout, stderr: "" }),
  });
  assert.equal(result.skill_triggered, false);
  assert.deepEqual(result.called_tools, ["Read"]);
  assert.match(result.stdout, /missing or empty/);
  assert.equal(result.error, undefined);
});

test("installClaudeSkill copies nested files and skips fixtures, parity, and symlinks", () => {
  const skill = fs.mkdtempSync(path.join(os.tmpdir(), "sp-skill-"));
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), "sp-skill-dest-"));
  fs.mkdirSync(path.join(skill, "references"));
  fs.writeFileSync(path.join(skill, "SKILL.md"), "---\nname: nested-skill\n---\n\n# nested\n");
  fs.writeFileSync(path.join(skill, "references", "guide.md"), "guide\n");
  fs.mkdirSync(path.join(skill, "fixtures"));
  fs.writeFileSync(path.join(skill, "fixtures", "secret.txt"), "nope\n");
  fs.writeFileSync(path.join(skill, "parity.yaml"), "tasks: []\n");
  fs.symlinkSync(path.join(skill, "references", "guide.md"), path.join(skill, "leak.md"));

  const installed = installClaudeSkill(skill, sandbox);
  assert.equal(installed.skillName, "nested-skill");
  const root = path.join(sandbox, ".claude", "skills", "nested-skill");
  assert.equal(fs.readFileSync(path.join(root, "references", "guide.md"), "utf8"), "guide\n");
  assert.equal(fs.existsSync(path.join(root, "fixtures")), false);
  assert.equal(fs.existsSync(path.join(root, "parity.yaml")), false);
  assert.equal(fs.existsSync(path.join(root, "leak.md")), false);
});

test("runTask surfaces a hard claude failure", async () => {
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), "sp-claude-err-"));
  const result = await runTask({
    sandboxDir: sandbox,
    skillDir: exampleSkill,
    task: { prompt: "hi" },
    claudeBin: "/opt/claude",
    env: {},
    authProbe: () => ({ loggedIn: true }),
    spawnImpl: () => ({ status: 1, stdout: "", stderr: "Invalid API key" }),
  });
  assert.match(result.error, /exited 1/);
  assert.match(result.error, /Invalid API key/);
});
