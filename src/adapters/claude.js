/**
 * Claude Code CLI adapter.
 *
 * When `claude` is on PATH and credentials are present, each task:
 *   1. Installs the skill at `<sandbox>/.claude/skills/<name>/SKILL.md`
 *      (project skill root; https://code.claude.com/docs/en/skills).
 *   2. Runs non-interactive print mode from that sandbox:
 *        claude -p "<prompt>" \
 *          --verbose \
 *          --output-format stream-json \
 *          --permission-mode acceptEdits \
 *          --allowedTools Read,Write,Edit,Glob,Grep,Skill \
 *          --max-turns 8
 *      `--bare` is intentionally not used: bare mode does not present
 *      project skills to the model. `bypassPermissions` is not the default;
 *      set SKILLPARITY_CLAUDE_PERMISSION_MODE=bypassPermissions only in a
 *      VM/container (it is refused by `--restricted` and is broader than
 *      this sandbox needs). stdin is ignored so print mode cannot block
 *      on an open pipe.
 *   3. Parses stream-json / json events for tool_use, Skill invocation,
 *      and Write/Edit paths. If stdout has a session id but no tools,
 *      also reads `~/.claude/projects/<slug>/<session-id>.jsonl`.
 *
 * Skip (no secrets required): CLI missing, or CLI present but
 * `claude auth status` is not logged in and neither ANTHROPIC_API_KEY
 * nor ANTHROPIC_AUTH_TOKEN is set.
 */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { findOnPath } from "../which.js";
import { installClaudeSkill } from "../skill-fs.js";
import {
  addUnique,
  canonicalToolName,
  detectSkillTrigger,
  extractJsonEvents,
  filePathFromInput,
  skillRefFromInput,
  toolsFromPlainText,
} from "./parse-shared.js";

export const name = "claude";

const READ_TOOLS = new Set(["Read", "Glob", "Grep"]);
const WRITE_TOOLS = new Set(["Write", "Edit"]);

export function buildClaudeArgs(prompt, options = {}) {
  const permissionMode = options.permissionMode || "acceptEdits";
  const maxTurns = String(options.maxTurns || 8);
  return [
    "-p",
    String(prompt ?? ""),
    "--verbose",
    "--output-format",
    "stream-json",
    "--permission-mode",
    permissionMode,
    "--allowedTools",
    "Read,Write,Edit,Glob,Grep,Skill",
    "--max-turns",
    maxTurns,
  ];
}

export function claudeProjectSlug(cwd) {
  return path.resolve(cwd).replace(/[^a-zA-Z0-9]/g, "-");
}

export function claudeTranscriptPath({ cwd, sessionId, home = os.homedir() }) {
  if (!sessionId) return null;
  return path.join(home, ".claude", "projects", claudeProjectSlug(cwd), `${sessionId}.jsonl`);
}

/**
 * Parse Claude Code print-mode stdout/stderr (stream-json, a single JSON
 * result, a jsonl transcript, or a plain "Using tool:" log).
 */
export function parseClaudeOutput(stdout = "", stderr = "", options = {}) {
  const skillName = options.skillName || null;
  const text = `${stdout ?? ""}\n${stderr ?? ""}`;
  const events = extractJsonEvents(text);
  const called_tools = [];
  const skillRefs = [];
  const readPaths = [];
  const artifacts = [];
  const assistantTexts = [];
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
          errorMessage = String(event.error || event.result || "claude result is_error");
        }
      }
      visitClaudeEvent(event, {
        called_tools,
        skillRefs,
        readPaths,
        artifacts,
        assistantTexts,
        inAssistant: event.type === "assistant" || event.role === "assistant",
      });
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

function visitClaudeEvent(node, ctx) {
  if (!node || typeof node !== "object") return;
  if (Array.isArray(node)) {
    for (const item of node) visitClaudeEvent(item, ctx);
    return;
  }

  if (node.type === "tool_use" && typeof node.name === "string") {
    recordClaudeTool(ctx, node.name, node.input || {});
  }
  if (node.type === "text" && typeof node.text === "string" && ctx.inAssistant) {
    ctx.assistantTexts.push(node.text);
  }

  let childCtx = ctx;
  if (node.role === "assistant" || node.type === "assistant") {
    childCtx = { ...ctx, inAssistant: true };
  } else if (node.role === "user" || node.type === "user") {
    childCtx = { ...ctx, inAssistant: false };
  }
  for (const value of Object.values(node)) {
    if (value && typeof value === "object") visitClaudeEvent(value, childCtx);
  }
}

function recordClaudeTool(ctx, toolName, input) {
  addUnique(ctx.called_tools, toolName);
  const canon = canonicalToolName(toolName);
  if (canon === "Skill") ctx.skillRefs.push(skillRefFromInput(input));
  const filePath = filePathFromInput(input);
  if (!filePath) return;
  if (READ_TOOLS.has(canon)) ctx.readPaths.push(filePath);
  if (WRITE_TOOLS.has(canon)) ctx.artifacts.push({ path: filePath });
}

export function assessClaudeAvailability({ bin, auth, env = process.env }) {
  if (!bin) {
    return {
      ok: false,
      skipped: true,
      skip_reason:
        "claude CLI not found on PATH. Install Claude Code (https://code.claude.com/docs/en/cli-reference) or run with --harness mock.",
    };
  }
  const hasKey = Boolean(env.ANTHROPIC_API_KEY || env.ANTHROPIC_AUTH_TOKEN);
  if (hasKey || auth?.loggedIn) return { ok: true };
  return {
    ok: false,
    skipped: true,
    skip_reason:
      "claude CLI is installed but no credentials are available (`claude auth status` is not logged in, and ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN are unset). Skipping so CI stays green without secrets.",
  };
}

function probeClaudeAuth(bin, spawnImpl, env) {
  const proc = spawnImpl(bin, ["auth", "status"], spawnOpts(env, 20_000));
  if (proc.error) return { loggedIn: false, detail: proc.error.message };
  const stdout = String(proc.stdout || "");
  const text = `${stdout}\n${proc.stderr || ""}`;
  if (proc.status === 0) {
    try {
      const parsed = JSON.parse(stdout);
      if (parsed.authMethod === "none" || parsed.loggedIn === false) {
        return { loggedIn: false, text };
      }
    } catch {
      // exit 0 is logged-in per `claude auth status`
    }
    return { loggedIn: true, text };
  }
  return { loggedIn: false, text };
}

function spawnOpts(env, timeout) {
  return {
    encoding: "utf8",
    timeout,
    maxBuffer: 16 * 1024 * 1024,
    stdio: ["ignore", "pipe", "pipe"],
    env,
  };
}

function mergeParse(primary, extra) {
  const called_tools = [...primary.called_tools];
  for (const tool of extra.called_tools) addUnique(called_tools, tool);
  const artifacts = [...primary.artifacts];
  for (const art of extra.artifacts) {
    if (!artifacts.some((a) => a.path === art.path)) artifacts.push(art);
  }
  return {
    ...primary,
    called_tools,
    artifacts,
    skill_triggered: primary.skill_triggered || extra.skill_triggered,
    stdout: primary.stdout || extra.stdout,
    session_id: primary.session_id || extra.session_id,
    is_error: primary.is_error || extra.is_error,
    error: primary.error || extra.error,
  };
}

function readExistingFiles(sandboxDir, artifacts) {
  const files = {};
  const root = path.resolve(sandboxDir);
  for (const art of artifacts) {
    const rel = sandboxRelative(root, art.path);
    if (!rel || files[rel]) continue;
    const abs = path.join(root, rel);
    if (!fs.existsSync(abs) || !fs.statSync(abs).isFile()) continue;
    files[rel] = fs.readFileSync(abs, "utf8");
  }
  const summaryRel = "summary.md";
  const summaryAbs = path.join(root, summaryRel);
  if (fs.existsSync(summaryAbs) && fs.statSync(summaryAbs).isFile() && !(summaryRel in files)) {
    files[summaryRel] = fs.readFileSync(summaryAbs, "utf8");
  }
  return files;
}

function sandboxRelative(root, filePath) {
  if (!filePath) return null;
  const abs = path.resolve(root, filePath);
  const prefix = root.endsWith(path.sep) ? root : root + path.sep;
  if (abs !== root && !abs.startsWith(prefix)) return null;
  const rel = path.relative(root, abs);
  if (!rel || rel.startsWith("..")) return null;
  return rel.split(path.sep).join("/");
}

export async function runTask(input) {
  const env = input.env || process.env;
  const spawnImpl = input.spawnImpl || spawnSync;
  const bin =
    input.claudeBin !== undefined ? input.claudeBin : findOnPath("claude", env);
  const hasKey = Boolean(env.ANTHROPIC_API_KEY || env.ANTHROPIC_AUTH_TOKEN);
  const auth = !bin
    ? { loggedIn: false }
    : hasKey
      ? { loggedIn: true }
      : input.authProbe
        ? input.authProbe(bin)
        : probeClaudeAuth(bin, spawnImpl, env);
  const gate = assessClaudeAvailability({ bin, auth, env });
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

  const installed = installClaudeSkill(input.skillDir, input.sandboxDir);
  const timeout = Number(env.SKILLPARITY_TIMEOUT_MS || input.timeoutMs || 180_000);
  const args = buildClaudeArgs(input.task?.prompt ?? "", {
    permissionMode: input.permissionMode || env.SKILLPARITY_CLAUDE_PERMISSION_MODE,
    maxTurns: input.maxTurns || env.SKILLPARITY_CLAUDE_MAX_TURNS,
  });
  const proc = spawnImpl(bin, args, {
    ...spawnOpts(env, timeout),
    cwd: input.sandboxDir,
  });

  const stdout = String(proc.stdout || "");
  const stderr = String(proc.stderr || "");
  let parsed = parseClaudeOutput(stdout, stderr, { skillName: installed.skillName });

  if (parsed.session_id && parsed.called_tools.length === 0) {
    const transcript = claudeTranscriptPath({
      cwd: input.sandboxDir,
      sessionId: parsed.session_id,
      home: input.homeDir || os.homedir(),
    });
    if (transcript && fs.existsSync(transcript)) {
      const disk = parseClaudeOutput(fs.readFileSync(transcript, "utf8"), "", {
        skillName: installed.skillName,
      });
      parsed = mergeParse(parsed, disk);
    }
  }

  const files = readExistingFiles(input.sandboxDir, parsed.artifacts);
  if (files["summary.md"] && !parsed.called_tools.includes("Write")) {
    parsed.called_tools.push("Write");
  }

  let error;
  if (proc.error) {
    error =
      proc.error.code === "ETIMEDOUT"
        ? `claude timed out after ${timeout}ms`
        : proc.error.message;
  } else if (parsed.is_error) {
    error = parsed.error || "claude result is_error";
  } else if (
    proc.status !== 0 &&
    proc.status != null &&
    !parsed.stdout.trim() &&
    parsed.called_tools.length === 0
  ) {
    const detail = stderr.trim() || stdout.trim();
    error = detail
      ? `claude exited ${proc.status}: ${detail.slice(0, 500)}`
      : `claude exited ${proc.status}`;
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
      permission_mode: args[args.indexOf("--permission-mode") + 1],
      skill_dir: installed.dest,
    },
  };
}
