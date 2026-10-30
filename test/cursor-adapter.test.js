import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import {
  assessCursorAvailability,
  buildCursorArgs,
  findCursorCli,
  parseCursorOutput,
  runTask,
} from "../src/adapters/cursor.js";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exampleSkill = path.join(root, "examples", "summarize-pr");

function ndjson(events) {
  return events.map((event) => JSON.stringify(event)).join("\n");
}

test("parseCursorOutput normalizes tool_call names and ignores the duplicate completion", () => {
  const stdout = ndjson([
    { type: "system", subtype: "init", session_id: "c1" },
    {
      type: "assistant",
      message: { role: "assistant", content: [{ type: "text", text: "I'll read the diff." }] },
      session_id: "c1",
    },
    {
      type: "tool_call",
      subtype: "started",
      call_id: "call-read",
      tool_call: { readToolCall: { args: { path: "pr.diff" } } },
    },
    {
      type: "tool_call",
      subtype: "completed",
      call_id: "call-read",
      tool_call: { readToolCall: { args: { path: "pr.diff" }, result: { success: {} } } },
    },
    {
      type: "tool_call",
      subtype: "completed",
      call_id: "call-write",
      tool_call: {
        writeToolCall: {
          args: { path: "summary.md", fileText: "# Update src/hello.js\n" },
          result: { success: { path: "/tmp/summary.md" } },
        },
      },
    },
    {
      type: "tool_call",
      subtype: "started",
      call_id: "call-fn",
      tool_call: { function: { name: "Grep", arguments: "{\"path\":\"src\"}" } },
    },
    { type: "result", subtype: "success", is_error: false, result: "Done", session_id: "c1" },
  ]);
  const parsed = parseCursorOutput(stdout, "", { skillName: "summarize-pr" });
  assert.deepEqual(parsed.called_tools, ["Read", "Write", "Grep"]);
  assert.equal(parsed.skill_triggered, false);
  assert.ok(parsed.artifacts.some((a) => a.path === "summary.md"));
  assert.match(parsed.stdout, /I'll read the diff/);
  assert.match(parsed.stdout, /Done/);
});

test("parseCursorOutput counts a skill when SKILL.md is read", () => {
  const stdout = ndjson([
    {
      type: "tool_call",
      subtype: "started",
      call_id: "s",
      tool_call: {
        readToolCall: { args: { path: ".cursor/skills/summarize-pr/SKILL.md" } },
      },
    },
  ]);
  const parsed = parseCursorOutput(stdout, "", { skillName: "summarize-pr" });
  assert.equal(parsed.skill_triggered, true);
  assert.deepEqual(parsed.called_tools, ["Read"]);
});

test("buildCursorArgs matches documented print mode", () => {
  const args = buildCursorArgs("Summarize the PR", "/tmp/sandbox");
  assert.deepEqual(args, [
    "-p",
    "Summarize the PR",
    "--force",
    "--trust",
    "--output-format",
    "stream-json",
    "--workspace",
    "/tmp/sandbox",
  ]);
});

test("assessCursorAvailability skips without a binary, identity, or credentials", () => {
  assert.match(
    assessCursorAvailability({ bin: null, identityOk: false, auth: {}, env: {} }).skip_reason,
    /not found/
  );
  assert.match(
    assessCursorAvailability({
      bin: "/usr/bin/agent",
      identityOk: false,
      auth: {},
      env: {},
    }).skip_reason,
    /does not identify/
  );
  assert.match(
    assessCursorAvailability({
      bin: "/usr/bin/agent",
      identityOk: true,
      auth: { loggedIn: false },
      env: {},
    }).skip_reason,
    /not authenticated/
  );
  assert.equal(
    assessCursorAvailability({
      bin: "/usr/bin/agent",
      identityOk: true,
      auth: { loggedIn: false },
      env: { CURSOR_API_KEY: "test" },
    }).ok,
    true
  );
});

test("findCursorCli prefers cursor-agent and rejects a generic agent", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sp-path-"));
  const cursorAgent = path.join(dir, "cursor-agent");
  fs.writeFileSync(cursorAgent, "#!/bin/sh\necho cursor-agent\n");
  fs.chmodSync(cursorAgent, 0o755);
  const preferred = findCursorCli({ env: { PATH: dir }, spawnImpl: spawnSync });
  assert.equal(preferred.bin, cursorAgent);
  assert.equal(preferred.identityOk, true);
  assert.equal(preferred.via, "cursor-agent");

  const other = fs.mkdtempSync(path.join(os.tmpdir(), "sp-agent-"));
  const agent = path.join(other, "agent");
  fs.writeFileSync(agent, "#!/bin/sh\necho vault agent\n");
  fs.chmodSync(agent, 0o755);
  const generic = findCursorCli({ env: { PATH: other }, spawnImpl: spawnSync });
  assert.equal(generic.identityOk, false);

  const real = fs.mkdtempSync(path.join(os.tmpdir(), "sp-cursor-agent-"));
  const named = path.join(real, "agent");
  fs.writeFileSync(named, "#!/bin/sh\necho 'Cursor Agent CLI 2026.10'\n");
  fs.chmodSync(named, 0o755);
  const identified = findCursorCli({ env: { PATH: real }, spawnImpl: spawnSync });
  assert.equal(identified.identityOk, true);
  assert.equal(identified.via, "agent");
});

test("runTask installs cursor skill roots and parses injected stream-json", async () => {
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), "sp-cursor-run-"));
  const stdout = ndjson([
    {
      type: "tool_call",
      subtype: "started",
      call_id: "skill",
      tool_call: { skillToolCall: { args: { skill: "summarize-pr" } } },
    },
    {
      type: "tool_call",
      subtype: "started",
      call_id: "read",
      tool_call: { readToolCall: { args: { path: "pr.diff" } } },
    },
    {
      type: "tool_call",
      subtype: "started",
      call_id: "write",
      tool_call: { writeToolCall: { args: { path: "summary.md", fileText: "# T\n" } } },
    },
    { type: "result", is_error: false, result: "Wrote summary.md" },
  ]);
  let captured;
  const result = await runTask({
    sandboxDir: sandbox,
    skillDir: exampleSkill,
    task: { prompt: "Please summarize this PR using the local pr.diff file." },
    skillName: "summarize-pr",
    cursorBin: "/opt/agent",
    identityOk: true,
    env: {},
    authProbe: () => ({ loggedIn: true }),
    spawnImpl(bin, args, opts) {
      captured = { bin, args, opts };
      fs.writeFileSync(path.join(sandbox, "summary.md"), "# Update src/hello.js\n\n- a\n- b\n- c\n");
      return { status: 0, stdout, stderr: "" };
    },
  });

  assert.equal(result.skill_triggered, true);
  assert.deepEqual(result.called_tools, ["Skill", "Read", "Write"]);
  assert.match(result.files["summary.md"], /hello\.js/);
  assert.equal(captured.opts.stdio[0], "ignore");
  assert.ok(captured.args.includes("--force"));
  assert.ok(captured.args.includes("--trust"));
  assert.equal(captured.args[captured.args.indexOf("--workspace") + 1], sandbox);
  for (const rel of [
    path.join(".cursor", "skills", "summarize-pr", "SKILL.md"),
    path.join(".agents", "skills", "summarize-pr", "SKILL.md"),
  ]) {
    assert.equal(fs.existsSync(path.join(sandbox, rel)), true, rel);
  }
  assert.equal(
    fs.existsSync(path.join(sandbox, ".cursor", "skills", "summarize-pr", "fixtures")),
    false
  );
  const cli = JSON.parse(fs.readFileSync(path.join(sandbox, ".cursor", "cli.json"), "utf8"));
  assert.ok(cli.permissions.allow.includes("Read(**)"));
  assert.ok(cli.permissions.allow.includes("Write(**)"));
  assert.equal("version" in cli, false);
});

test("runTask skips a missing cursor CLI before writing the sandbox", async () => {
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), "sp-cursor-skip-"));
  const result = await runTask({
    sandboxDir: sandbox,
    skillDir: exampleSkill,
    task: { prompt: "hi" },
    cursorBin: null,
    env: {},
  });
  assert.equal(result.skipped, true);
  assert.match(result.skip_reason, /not found/);
  assert.equal(fs.existsSync(path.join(sandbox, ".cursor")), false);
});
