/**
 * Cursor Agent CLI adapter (second harness).
 *
 * Docs used (no private API):
 *   https://cursor.com/docs/cli/overview
 *   https://cursor.com/docs/cli/headless
 *   https://cursor.com/docs/cli/reference/parameters
 *   https://cursor.com/docs/cli/reference/output-format
 *   https://cursor.com/docs/cli/reference/configuration
 *
 * Installs the skill at both documented project roots:
 *   <sandbox>/.cursor/skills/<name>/SKILL.md
 *   <sandbox>/.agents/skills/<name>/SKILL.md
 * and writes project permissions to `<sandbox>/.cursor/cli.json`.
 *
 * Invocation when the CLI is present and authenticated:
 *   agent -p "<prompt>" --force --trust \
 *     --output-format stream-json --workspace <sandbox>
 * `cursor-agent` is preferred when both binaries exist. A generic `agent`
 * on PATH is used only if `--version` / `about` identifies Cursor.
 * `--force` applies file writes in print mode; `--trust` skips the
 * workspace trust prompt. stdin is ignored.
 *
 * stream-json `tool_call` keys (`readToolCall`, `writeToolCall`, …) are
 * normalized to Read / Write / … so parity.yaml stays harness-neutral.
 *
 * Skip (no secrets required): neither binary is present, `agent` is not
 * Cursor, or there is no CURSOR_API_KEY and whoami/status is not logged in.
 * Codex is not a third adapter; Cursor is the second harness.
 */
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { findOnPath } from "../which.js";
import { installCursorSkill, readSkillName } from "../skill-fs.js";
import {
  addUnique,
  canonicalToolName,
  detectSkillTrigger,
  extractJsonEvents,
  filePathFromInput,
  skillRefFromInput,
  toolsFromPlainText,
} from "./parse-shared.js";

export const name = "cursor";

const READ_TOOLS = new Set(["Read", "Glob", "Grep"]);
const WRITE_TOOLS = new Set(["Write", "Edit"]);

export function buildCursorArgs(prompt, workspace) {
  return [
    "-p",
    String(prompt ?? ""),
    "--force",
    "--trust",
    "--output-format",
    "stream-json",
    "--workspace",
    workspace,
  ];
}

export function assessCursorAvailability({ bin, identityOk, auth, env = process.env }) {
  if (!bin) {
    return {
      ok: false,
      skipped: true,
      skip_reason:
        "Cursor Agent CLI not found (`cursor-agent` or `agent`). Install from https://cursor.com/docs/cli/overview or run with --harness mock.",
    };
  }
  if (!identityOk) {
    return {
      ok: false,
      skipped: true,
      skip_reason:
        "`agent` is on PATH but does not identify as the Cursor Agent CLI, and `cursor-agent` was not found. Skipping.",
    };
  }
  if (env.CURSOR_API_KEY || auth?.loggedIn) return { ok: true };
  return {
    ok: false,
    skipped: true,
    skip_reason:
      "Cursor Agent CLI is installed but not authenticated (no CURSOR_API_KEY, and whoami/status did not show a login). Skipping so CI stays green without secrets.",
  };
}

export function parseCursorOutput(stdout = "", stderr = "", options = {}) {
  const skillName = options.skillName || null;
  const text = `${stdout ?? ""}\n${stderr ?? ""}`;
  const events = extractJsonEvents(text);
  const called_tools = [];
  const skillRefs = [];
  const readPaths = [];
  const artifacts = [];
  const assistantTexts = [];
  const seenCalls = new Set();
  let resultText = "";
  let sessionId = null;
  let isError = false;
  let errorMessage = null;

  if (events.length === 0) {
    for (const tool of toolsFromPlainText(text)) addUnique(called_tools, tool);
  } else {
    for (const event of events) {
      if (!sessionId) sessionId = event.session_id || event.sessionId || null;
      if (event.type === "result") {
        if (typeof event.result === "string") resultText = event.result;
        if (event.is_error) {
          isError = true;
          errorMessage = String(event.error || event.result || "cursor result is_error");
        }
      }
      if (event.type === "assistant") {
        collectAssistantText(event, assistantTexts);
      }
      if (event.type === "tool_call" && event.tool_call && typeof event.tool_call === "object") {
        if (event.call_id && seenCalls.has(event.call_id)) continue;
        if (event.call_id) seenCalls.add(event.call_id);
        recordCursorToolCall(event.tool_call, {
          called_tools,
          skillRefs,
          readPaths,
          artifacts,
        });
      }
    }
  }

  const skill_triggered = detectSkillTrigger({
    skillName,
    skillRefs,
    readPaths,
    assistantTexts: resultText ? [...assistantTexts, resultText] : assistantTexts,
  });
  const visible = [...assistantTexts, resultText].filter(Boolean).join("\n");
  return {
    called_tools,
    skill_triggered,
    stdout: visible || String(stdout ?? ""),
    stderr: String(stderr ?? ""),
    artifacts,
    session_id: sessionId,
    is_error: isError,
    error: errorMessage,
  };
}

function collectAssistantText(event, texts) {
  const content = event.message?.content;
  if (!Array.isArray(content)) return;
  for (const block of content) {
    if (block && block.type === "text" && typeof block.text === "string") texts.push(block.text);
  }
}

function recordCursorToolCall(toolCall, ctx) {
  for (const [key, value] of Object.entries(toolCall)) {
    if (key === "function" && value && typeof value === "object") {
      addUnique(ctx.called_tools, value.name);
      noteInput(ctx, canonicalToolName(value.name), parseMaybeJson(value.arguments));
      continue;
    }
    const canon = canonicalToolName(key);
    addUnique(ctx.called_tools, key);
    const args =
      value && typeof value === "object" ? value.args || value.input || {} : {};
    noteInput(ctx, canon, args);
  }
}

function parseMaybeJson(value) {
  if (value && typeof value === "object") return value;
  if (typeof value !== "string" || !value.trim()) return {};
  try {
    const parsed = JSON.parse(value);
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};
  }
}

function noteInput(ctx, canon, input) {
  if (canon === "Skill") ctx.skillRefs.push(skillRefFromInput(input));
  const filePath = filePathFromInput(input);
  if (!filePath) return;
  if (READ_TOOLS.has(canon)) ctx.readPaths.push(filePath);
  if (WRITE_TOOLS.has(canon)) ctx.artifacts.push({ path: filePath });
}

function outputOf(spawnImpl, bin, args, env) {
  const proc = spawnImpl(bin, args, {
    encoding: "utf8",
    timeout: 8_000,
    maxBuffer: 2 * 1024 * 1024,
    stdio: ["ignore", "pipe", "pipe"],
    env,
  });
  return {
    status: proc.status,
    error: proc.error,
    text: `${proc.stdout || ""}\n${proc.stderr || ""}`,
  };
}

export function cursorIdentifies(bin, spawnImpl, env) {
  const version = outputOf(spawnImpl, bin, ["--version"], env);
  if (/cursor/i.test(version.text)) return true;
  const about = outputOf(spawnImpl, bin, ["about"], env);
  if (/cursor/i.test(about.text)) return true;
  const help = outputOf(spawnImpl, bin, ["--help"], env);
  return /cursor agent|cursor\.com\/docs\/cli|Cursor CLI/i.test(help.text);
}

export function findCursorCli({ env = process.env, spawnImpl = spawnSync, find = findOnPath } = {}) {
  const cursorAgent = find("cursor-agent", env);
  if (cursorAgent) return { bin: cursorAgent, identityOk: true, via: "cursor-agent" };
  const agent = find("agent", env);
  if (!agent) return { bin: null, identityOk: false, via: null };
  const identityOk = cursorIdentifies(agent, spawnImpl, env);
  return { bin: agent, identityOk, via: "agent" };
}

const LOGGED_OUT_RE = /not logged in|not authenticated|login required|no credentials|no api key/i;

function probeCursorAuth(bin, spawnImpl, env) {
  const attempts = [
    ["whoami", "--format", "json"],
    ["status", "--format", "json"],
    ["whoami"],
    ["status"],
  ];
  for (const args of attempts) {
    const proc = spawnImpl(bin, args, {
      encoding: "utf8",
      timeout: 15_000,
      maxBuffer: 2 * 1024 * 1024,
      stdio: ["ignore", "pipe", "pipe"],
      env,
    });
    if (proc.error) continue;
    const text = `${proc.stdout || ""}\n${proc.stderr || ""}`;
    if (proc.status === 0 && text.trim() && !LOGGED_OUT_RE.test(text)) {
      return { loggedIn: true };
    }
  }
  return { loggedIn: false };
}

function readExistingFiles(sandboxDir, artifacts) {
  const files = {};
  const root = path.resolve(sandboxDir);
  const prefix = root.endsWith(path.sep) ? root : root + path.sep;
  for (const art of artifacts) {
    if (!art.path) continue;
    const abs = path.resolve(root, art.path);
    if (abs !== root && !abs.startsWith(prefix)) continue;
    const rel = path.relative(root, abs).split(path.sep).join("/");
    if (!rel || rel.startsWith("..") || files[rel]) continue;
    if (!fs.existsSync(abs) || !fs.statSync(abs).isFile()) continue;
    files[rel] = fs.readFileSync(abs, "utf8");
  }
  const summary = path.join(root, "summary.md");
  if (fs.existsSync(summary) && !("summary.md" in files)) {
    files["summary.md"] = fs.readFileSync(summary, "utf8");
  }
  return files;
}

export async function runTask(input) {
  const env = input.env || process.env;
  const spawnImpl = input.spawnImpl || spawnSync;
  const found =
    input.cursorBin !== undefined
      ? {
          bin: input.cursorBin,
          identityOk: input.identityOk !== false && Boolean(input.cursorBin),
          via: "injected",
        }
      : findCursorCli({ env, spawnImpl });
  const auth = env.CURSOR_API_KEY
    ? { loggedIn: true }
    : found.bin && found.identityOk
      ? input.authProbe
        ? input.authProbe(found.bin)
        : probeCursorAuth(found.bin, spawnImpl, env)
      : { loggedIn: false };
  const gate = assessCursorAvailability({
    bin: found.bin,
    identityOk: found.identityOk,
    auth,
    env,
  });
  if (!gate.ok) {
    return {
      skipped: true,
      skip_reason: gate.skip_reason,
      skill_triggered: false,
      called_tools: [],
      files: {},
      stdout: "",
      harness: name,
    };
  }

  const installed = installCursorSkill(input.skillDir, input.sandboxDir);
  const skillName = input.skillName || installed.skillName || readSkillName(input.skillDir);
  const timeout = Number(env.SKILLPARITY_TIMEOUT_MS || input.timeoutMs || 180_000);
  const args = buildCursorArgs(input.task?.prompt ?? "", input.sandboxDir);
  const proc = spawnImpl(found.bin, args, {
    cwd: input.sandboxDir,
    encoding: "utf8",
    timeout,
    maxBuffer: 16 * 1024 * 1024,
    stdio: ["ignore", "pipe", "pipe"],
    env,
  });

  const stdout = String(proc.stdout || "");
  const stderr = String(proc.stderr || "");
  const parsed = parseCursorOutput(stdout, stderr, { skillName });
  const files = readExistingFiles(input.sandboxDir, parsed.artifacts);
  if (files["summary.md"] && !parsed.called_tools.includes("Write")) {
    parsed.called_tools.push("Write");
  }

  let error;
  if (proc.error) {
    error =
      proc.error.code === "ETIMEDOUT"
        ? `cursor agent timed out after ${timeout}ms`
        : proc.error.message;
  } else if (parsed.is_error) {
    error = parsed.error || "cursor result is_error";
  } else if (
    proc.status !== 0 &&
    proc.status != null &&
    !parsed.stdout.trim() &&
    parsed.called_tools.length === 0
  ) {
    const detail = stderr.trim() || stdout.trim();
    error = detail
      ? `cursor agent exited ${proc.status}: ${detail.slice(0, 500)}`
      : `cursor agent exited ${proc.status}`;
  }

  return {
    error,
    skill_triggered: parsed.skill_triggered,
    called_tools: parsed.called_tools,
    files,
    stdout: parsed.stdout,
    harness: name,
    meta: {
      argv: args,
      exitCode: proc.status ?? null,
      session_id: parsed.session_id,
      via: found.via,
      skill_dirs: installed.dirs,
    },
  };
}
