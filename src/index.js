import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseParityYaml } from "./yaml.js";
import { evaluateExpect } from "./assert.js";
import { writeReport } from "./report.js";
import { decideExitCode } from "./ci.js";
import { readSkillName } from "./skill-fs.js";
import * as mockAdapter from "./adapters/mock.js";
import * as claudeAdapter from "./adapters/claude.js";
import * as cursorAdapter from "./adapters/cursor.js";

const ADAPTERS = {
  mock: mockAdapter,
  claude: claudeAdapter,
  cursor: cursorAdapter,
};

function usage() {
  const harnesses = Object.keys(ADAPTERS).join("|");
  return `skillparity — multi-harness skill behavioral parity

Usage:
  skillparity run --skill <dir> --parity <yaml> [--harness ${harnesses}] [--ci] [--skip-ok]
  skillparity help

Options:
  --skill     Directory containing SKILL.md
  --parity    parity.yaml task file
  --harness   ${Object.keys(ADAPTERS).join(" | ")} (default: mock)
  --ci        exit 1 on any fail (skip does not fail unless all skipped)
  --skip-ok   with --ci, exit 0 when every task was skipped
  --out       directory for parity-report.json (default: cwd)

Example:
  skillparity run \\
    --skill examples/summarize-pr \\
    --parity examples/summarize-pr/parity.yaml \\
    --harness mock --ci
`;
}

function parseArgs(argv) {
  if (argv.length === 0 || argv[0] === "help" || argv[0] === "--help" || argv[0] === "-h") {
    return { cmd: "help" };
  }
  const cmd = argv[0];
  const out = {
    cmd,
    skill: null,
    parity: null,
    harness: "mock",
    ci: false,
    skipOk: false,
    out: process.cwd(),
  };
  for (let i = 1; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--skill") out.skill = argv[++i];
    else if (a === "--parity") out.parity = argv[++i];
    else if (a === "--harness") out.harness = argv[++i];
    else if (a === "--out") out.out = argv[++i];
    else if (a === "--ci") out.ci = true;
    else if (a === "--skip-ok") out.skipOk = true;
    else if (a === "--help" || a === "-h") out.cmd = "help";
    else throw new Error(`Unknown argument: ${a}`);
  }
  return out;
}

function prepareSandbox(skillDir, task) {
  const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), "skillparity-"));
  if (task.fixture) {
    const fixtureDir = path.isAbsolute(task.fixture)
      ? task.fixture
      : path.join(skillDir, task.fixture);
    if (!fs.existsSync(fixtureDir)) {
      throw new Error(`fixture not found: ${fixtureDir}`);
    }
    copyTree(fixtureDir, sandbox);
  }
  return sandbox;
}

function copyTree(src, dest) {
  fs.mkdirSync(dest, { recursive: true });
  for (const ent of fs.readdirSync(src, { withFileTypes: true })) {
    const s = path.join(src, ent.name);
    const d = path.join(dest, ent.name);
    if (ent.isDirectory()) copyTree(s, d);
    else fs.copyFileSync(s, d);
  }
}

async function runCommand(opts) {
  if (!opts.skill || !opts.parity) {
    throw new Error("--skill and --parity are required");
  }
  const skillDir = path.resolve(opts.skill);
  const parityPath = path.resolve(opts.parity);
  if (!fs.existsSync(path.join(skillDir, "SKILL.md"))) {
    throw new Error(`SKILL.md not found in ${skillDir}`);
  }
  if (!fs.existsSync(parityPath)) {
    throw new Error(`parity file not found: ${parityPath}`);
  }

  const adapter = ADAPTERS[opts.harness];
  if (!adapter) {
    throw new Error(
      `Unknown harness "${opts.harness}". Available: ${Object.keys(ADAPTERS).join(", ")}`
    );
  }

  const parity = parseParityYaml(fs.readFileSync(parityPath, "utf8"));
  const tasks = parity.tasks || [];
  if (tasks.length === 0) {
    throw new Error("parity.yaml has no tasks");
  }
  const skillName = readSkillName(skillDir);

  const results = [];
  for (const task of tasks) {
    const sandboxDir = prepareSandbox(skillDir, task);
    let adapterResult;
    try {
      adapterResult = await adapter.runTask({
        sandboxDir,
        skillDir,
        task,
        skillName,
      });
    } catch (err) {
      adapterResult = {
        error: err.message,
        skill_triggered: false,
        called_tools: [],
        files: {},
        stdout: "",
        harness: opts.harness,
      };
    }

    // Collect files from sandbox after run
    const files = { ...(adapterResult.files || {}) };
    const summaryPath = path.join(sandboxDir, "summary.md");
    if (fs.existsSync(summaryPath) && !("summary.md" in files)) {
      files["summary.md"] = fs.readFileSync(summaryPath, "utf8");
    }
    adapterResult.files = files;

    const assertion = evaluateExpect(task.expect || {}, adapterResult);
    results.push({
      id: task.id || "unnamed",
      harness: opts.harness,
      pass: assertion.pass,
      skipped: assertion.skipped || adapterResult.skipped,
      skip_reason: adapterResult.skip_reason || null,
      failures: assertion.failures,
      notes: assertion.notes,
      detail: {
        skill_triggered: adapterResult.skill_triggered,
        called_tools: adapterResult.called_tools,
        files: Object.keys(files),
        stdout_preview: (adapterResult.stdout || "").slice(0, 400),
        sandbox: sandboxDir,
        meta: adapterResult.meta || null,
      },
    });
  }

  const { failed, skipped, total } = writeReport({
    harness: opts.harness,
    skillDir,
    parityPath,
    results,
    outDir: path.resolve(opts.out),
    skipOk: opts.skipOk,
  });

  process.exitCode = decideExitCode({
    failed,
    skipped,
    total,
    ci: opts.ci,
    skipOk: opts.skipOk,
  });
}

export async function main(argv) {
  const opts = parseArgs(argv);
  if (opts.cmd === "help") {
    console.log(usage());
    return;
  }
  if (opts.cmd !== "run") {
    console.error(`Unknown command: ${opts.cmd}\n`);
    console.log(usage());
    process.exitCode = 2;
    return;
  }
  await runCommand(opts);
}

// Allow direct: node src/index.js
const isDirect =
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isDirect) {
  main(process.argv.slice(2)).catch((err) => {
    console.error(err?.stack || String(err));
    process.exit(2);
  });
}
