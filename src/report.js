import fs from "node:fs";
import path from "node:path";

/**
 * Print harness×task matrix and write parity-report.json
 */
export function writeReport({ harness, skillDir, parityPath, results, outDir, skipOk = false }) {
  const passed = results.filter((r) => r.pass && !r.skipped).length;
  const failed = results.filter((r) => !r.pass && !r.skipped).length;
  const skipped = results.filter((r) => r.skipped).length;
  const total = results.length;

  const report = {
    generated_at: new Date().toISOString(),
    harness,
    skill: skillDir,
    parity: parityPath,
    skip_ok: Boolean(skipOk),
    summary: { total, passed, failed, skipped },
    results: results.map((r) => ({
      id: r.id,
      harness: r.harness,
      pass: r.pass,
      skipped: Boolean(r.skipped),
      skip_reason: r.skip_reason || null,
      failures: r.failures || [],
      notes: r.notes || [],
      detail: r.detail || null,
    })),
  };

  const dir = outDir || process.cwd();
  fs.mkdirSync(dir, { recursive: true });
  const outPath = path.join(dir, "parity-report.json");
  fs.writeFileSync(outPath, JSON.stringify(report, null, 2) + "\n", "utf8");

  // Terminal matrix
  const colId = Math.max(8, ...results.map((r) => r.id.length));
  const header = `${"TASK".padEnd(colId)}  ${String(harness).toUpperCase().padEnd(8)}  STATUS`;
  console.log("");
  console.log("SkillParity matrix");
  console.log("─".repeat(header.length));
  console.log(header);
  console.log("─".repeat(header.length));
  for (const r of results) {
    const status = r.skipped ? "SKIP" : r.pass ? "PASS" : "FAIL";
    console.log(`${r.id.padEnd(colId)}  ${harness.padEnd(8)}  ${status}`);
    if (!r.pass && !r.skipped && r.failures?.length) {
      for (const f of r.failures) console.log(`  └─ ${f}`);
    }
    if (r.skipped && r.failures?.[0]) {
      console.log(`  └─ ${r.failures[0]}`);
    }
  }
  console.log("─".repeat(header.length));
  console.log(
    `summary: ${passed} pass / ${failed} fail / ${skipped} skip  (${total} total)`
  );
  console.log(`report:  ${outPath}`);
  console.log("");

  return { report, outPath, passed, failed, skipped, total };
}
