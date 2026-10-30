import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { decideExitCode } from "../src/ci.js";
import { findOnPath } from "../src/which.js";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function runCli(args, env) {
  return spawnSync(process.execPath, [path.join(root, "bin", "skillparity.js"), ...args], {
    cwd: root,
    encoding: "utf8",
    env,
  });
}

function cleanEnv() {
  const env = { ...process.env, PATH: "" };
  delete env.ANTHROPIC_API_KEY;
  delete env.ANTHROPIC_AUTH_TOKEN;
  delete env.CURSOR_API_KEY;
  delete env.SKILLPARITY_CLAUDE_PERMISSION_MODE;
  return env;
}

test("decideExitCode", () => {
  assert.equal(decideExitCode({ failed: 0, skipped: 0, total: 3, ci: true, skipOk: false }), 0);
  assert.equal(decideExitCode({ failed: 1, skipped: 0, total: 3, ci: true, skipOk: true }), 1);
  assert.equal(decideExitCode({ failed: 0, skipped: 3, total: 3, ci: true, skipOk: false }), 1);
  assert.equal(decideExitCode({ failed: 0, skipped: 3, total: 3, ci: true, skipOk: true }), 0);
  assert.equal(decideExitCode({ failed: 2, skipped: 0, total: 2, ci: false, skipOk: false }), 0);
});

test("findOnPath", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sp-which-"));
  const bin = path.join(dir, "claude");
  fs.writeFileSync(bin, "#!/bin/sh\nexit 0\n");
  fs.chmodSync(bin, 0o755);
  fs.writeFileSync(path.join(dir, "note"), "nope");
  assert.equal(findOnPath("claude", { PATH: dir }), bin);
  assert.equal(findOnPath("note", { PATH: dir }), null);
  assert.equal(findOnPath("missing", { PATH: `${dir}${path.delimiter}` }), null);
  assert.equal(findOnPath("claude", { PATH: "" }), null);
});

test("mock --ci passes summarize-pr with 3 passes", () => {
  const out = fs.mkdtempSync(path.join(os.tmpdir(), "sp-mock-out-"));
  const proc = runCli([
    "run",
    "--skill",
    "examples/summarize-pr",
    "--parity",
    "examples/summarize-pr/parity.yaml",
    "--harness",
    "mock",
    "--ci",
    "--out",
    out,
  ], process.env);
  assert.equal(proc.status, 0, proc.stdout + proc.stderr);
  assert.match(proc.stdout, /3 pass/);
  const report = JSON.parse(fs.readFileSync(path.join(out, "parity-report.json"), "utf8"));
  assert.equal(report.summary.passed, 3);
  assert.equal(report.summary.failed, 0);
  assert.equal(report.harness, "mock");
});

test("claude and cursor skip cleanly and --skip-ok keeps CI green", () => {
  for (const harness of ["claude", "cursor"]) {
    const out = fs.mkdtempSync(path.join(os.tmpdir(), `sp-${harness}-out-`));
    const ok = runCli([
      "run",
      "--skill",
      "examples/summarize-pr",
      "--parity",
      "examples/summarize-pr/parity.yaml",
      "--harness",
      harness,
      "--ci",
      "--skip-ok",
      "--out",
      out,
    ], cleanEnv());
    assert.equal(ok.status, 0, ok.stdout + ok.stderr);
    assert.match(ok.stdout, /3 skip/);
    assert.match(ok.stdout, /not found/);
    const report = JSON.parse(fs.readFileSync(path.join(out, "parity-report.json"), "utf8"));
    assert.equal(report.summary.skipped, 3);
    assert.equal(report.skip_ok, true);
    assert.match(report.results[0].skip_reason, /not found/);

    const blocked = runCli([
      "run",
      "--skill",
      "examples/summarize-pr",
      "--parity",
      "examples/summarize-pr/parity.yaml",
      "--harness",
      harness,
      "--ci",
      "--out",
      fs.mkdtempSync(path.join(os.tmpdir(), `sp-${harness}-block-`)),
    ], cleanEnv());
    assert.equal(blocked.status, 1, blocked.stdout + blocked.stderr);
  }
});
