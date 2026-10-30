/**
 * Copy a skill package into a harness sandbox.
 * Skips parity fixtures and VCS/deps so the installed tree is what the harness loads.
 */
import fs from "node:fs";
import path from "node:path";

const SKIP_NAMES = new Set([
  "fixtures",
  "parity.yaml",
  "parity-report.json",
  "node_modules",
  ".git",
]);

export function readSkillName(skillDir) {
  const md = fs.readFileSync(path.join(skillDir, "SKILL.md"), "utf8");
  const m = md.match(/^name:\s*["']?([^\s"']+)/m);
  return m ? m[1] : path.basename(skillDir);
}

export function copySkillTree(skillDir, destDir) {
  fs.mkdirSync(destDir, { recursive: true });
  copyFiltered(skillDir, destDir);
  return destDir;
}

function copyFiltered(src, dest) {
  for (const ent of fs.readdirSync(src, { withFileTypes: true })) {
    if (SKIP_NAMES.has(ent.name)) continue;
    // Do not follow symlinks out of the skill directory.
    if (ent.isSymbolicLink()) continue;
    const from = path.join(src, ent.name);
    const to = path.join(dest, ent.name);
    if (ent.isDirectory()) {
      fs.mkdirSync(to, { recursive: true });
      copyFiltered(from, to);
    } else if (ent.isFile()) {
      fs.copyFileSync(from, to);
    }
  }
}

/**
 * Project skill path Claude Code loads: `.claude/skills/<name>/SKILL.md`.
 * Also writes `.claude/settings.json` so Read/Write/Edit/Skill are allowed
 * if the process falls back to permission rules (flag still sets the mode).
 */
export function installClaudeSkill(skillDir, sandboxDir) {
  const skillName = readSkillName(skillDir);
  const dest = path.join(sandboxDir, ".claude", "skills", skillName);
  copySkillTree(skillDir, dest);
  const settingsPath = path.join(sandboxDir, ".claude", "settings.json");
  const settings = {
    permissions: {
      defaultMode: "acceptEdits",
      allow: ["Read", "Write", "Edit", "Glob", "Grep", "Skill"],
      deny: [],
    },
  };
  fs.writeFileSync(settingsPath, JSON.stringify(settings, null, 2) + "\n");
  return { skillName, dest, settingsPath };
}

/**
 * Cursor Agent discovers project skills in `.cursor/skills/` and `.agents/skills/`.
 * Project CLI permissions live in `.cursor/cli.json` (permissions only).
 */
export function installCursorSkill(skillDir, sandboxDir) {
  const skillName = readSkillName(skillDir);
  const dirs = [
    path.join(sandboxDir, ".cursor", "skills", skillName),
    path.join(sandboxDir, ".agents", "skills", skillName),
  ];
  for (const dest of dirs) copySkillTree(skillDir, dest);
  const cliJsonPath = path.join(sandboxDir, ".cursor", "cli.json");
  const cliJson = {
    permissions: {
      allow: ["Read(**)", "Write(**)"],
      deny: ["Shell(rm)", "Read(.env*)", "Write(**/*.key)", "Write(**/.env*)"],
    },
  };
  fs.writeFileSync(cliJsonPath, JSON.stringify(cliJson, null, 2) + "\n");
  return { skillName, dirs, cliJsonPath };
}
