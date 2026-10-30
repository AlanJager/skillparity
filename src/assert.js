/**
 * Evaluate expect.* fields against an adapter run result.
 * result shape:
 *   { skill_triggered: boolean, called_tools: string[], files: Record<path, content|true>,
 *     stdout: string, skipped?: boolean, skip_reason?: string, error?: string }
 */

export function evaluateExpect(expect = {}, result = {}) {
  const failures = [];
  const notes = [];

  if (result.skipped) {
    return {
      pass: false,
      skipped: true,
      failures: [`skipped: ${result.skip_reason || "adapter skipped"}`],
      notes,
    };
  }

  if (result.error && expect.skill_triggered !== false) {
    // Hard adapter error counts as fail unless we only cared about no-trigger.
    failures.push(`adapter error: ${result.error}`);
  }

  if (typeof expect.skill_triggered === "boolean") {
    const actual = Boolean(result.skill_triggered);
    if (actual !== expect.skill_triggered) {
      failures.push(
        `skill_triggered: expected ${expect.skill_triggered}, got ${actual}`
      );
    }
  }

  const called = new Set(
    (result.called_tools || []).map((t) => String(t).toLowerCase())
  );

  if (Array.isArray(expect.called_tools)) {
    for (const tool of expect.called_tools) {
      if (!called.has(String(tool).toLowerCase())) {
        failures.push(`called_tools: missing "${tool}"`);
      }
    }
  }

  if (Array.isArray(expect.not_called_tools)) {
    for (const tool of expect.not_called_tools) {
      if (called.has(String(tool).toLowerCase())) {
        failures.push(`not_called_tools: unexpectedly called "${tool}"`);
      }
    }
  }

  if (Array.isArray(expect.files_exist)) {
    const files = result.files || {};
    for (const rel of expect.files_exist) {
      if (!(rel in files) && !Object.keys(files).some((k) => k.endsWith(rel))) {
        failures.push(`files_exist: missing "${rel}"`);
      }
    }
    // Empty list means: assert no unexpected summary.md unless listed.
    // Only enforce "must not exist" when list is explicitly empty AND skill was not expected to write.
    if (expect.files_exist.length === 0 && "summary.md" in files) {
      // Soft note if skill_triggered false — hard fail if we said no files.
      failures.push(`files_exist: unexpected "summary.md" present`);
    }
  }

  if (expect.stdout_matches) {
    let pattern = String(expect.stdout_matches);
    let flags = "m";
    // Translate common inline flags JS RegExp does not accept as (?i)
    if (pattern.startsWith("(?i)")) {
      flags += "i";
      pattern = pattern.slice(4);
    } else if (pattern.startsWith("(?im)") || pattern.startsWith("(?mi)")) {
      flags += "i";
      pattern = pattern.slice(5);
    }
    const re = new RegExp(pattern, flags);
    const stdout = result.stdout || "";
    if (!re.test(stdout)) {
      failures.push(
        `stdout_matches: /${expect.stdout_matches}/ did not match stdout`
      );
    }
  }

  return {
    pass: failures.length === 0,
    skipped: false,
    failures,
    notes,
  };
}
