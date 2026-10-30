/**
 * Shared transcript helpers for harness adapters.
 * Tool names are normalized to the vocabulary parity.yaml already uses
 * (Read, Write, Edit, Glob, Grep, Bash, Skill).
 */

const CANON = {
  read: "Read",
  write: "Write",
  edit: "Edit",
  multiedit: "Edit",
  notebookedit: "Edit",
  bash: "Bash",
  shell: "Bash",
  strreplace: "Edit",
  searchreplace: "Edit",
  glob: "Glob",
  grep: "Grep",
  skill: "Skill",
};

export function canonicalToolName(name) {
  const raw = String(name || "").trim();
  if (!raw) return "";
  const key = raw.replace(/[^a-zA-Z0-9]/g, "").toLowerCase();
  const base = key.endsWith("toolcall") ? key.slice(0, -"toolcall".length) : key;
  if (CANON[base]) return CANON[base];
  if (CANON[key]) return CANON[key];
  return raw;
}

export function extractJsonEvents(text) {
  const events = [];
  const raw = String(text ?? "");
  let parsedLine = false;
  for (const line of raw.split(/\r?\n/)) {
    const t = line.trim();
    if (!t || (!t.startsWith("{") && !t.startsWith("["))) continue;
    try {
      const value = JSON.parse(t);
      parsedLine = true;
      pushEvent(events, value);
    } catch {
      // log line or partial JSON
    }
  }
  if (!parsedLine) {
    const t = raw.trim();
    if (t.startsWith("{") || t.startsWith("[")) {
      try {
        pushEvent(events, JSON.parse(t));
      } catch {
        // not JSON
      }
    }
  }
  return events;
}

function pushEvent(events, value) {
  if (Array.isArray(value)) {
    for (const item of value) {
      if (item && typeof item === "object") events.push(item);
    }
  } else if (value && typeof value === "object") {
    events.push(value);
  }
}

export function addUnique(list, name) {
  const canon = canonicalToolName(name);
  if (!canon) return;
  if (!list.includes(canon)) list.push(canon);
}

/**
 * skillRefs: one entry per Skill-tool call (empty string if the call named no skill).
 * readPaths: file paths from Read/Glob inputs.
 * assistantTexts: assistant-visible text, not the echoed user prompt.
 */
export function detectSkillTrigger({ skillName, skillRefs = [], readPaths = [], assistantTexts = [] }) {
  if (skillRefs.length > 0) {
    if (!skillName) return true;
    const named = skillRefs.map((r) => String(r || "").trim()).filter(Boolean);
    if (named.length === 0) return true;
    const needle = String(skillName).toLowerCase();
    return named.some((r) => r.toLowerCase().includes(needle));
  }
  if (!skillName) return false;
  const needle = String(skillName).toLowerCase();
  const pathNeedle = `${needle}/skill.md`;
  if (
    readPaths.some((p) =>
      String(p).toLowerCase().replaceAll("\\", "/").includes(pathNeedle)
    )
  ) {
    return true;
  }
  return assistantTexts.some((t) => {
    const s = String(t).toLowerCase();
    return (
      s.includes(`/${needle}`) ||
      s.includes(`skill ${needle}`) ||
      s.includes(`skill \`${needle}\``) ||
      s.includes(`\`${needle}\` skill`)
    );
  });
}

const TEXT_TOOL_RE =
  /(?:^|\n)\s*(?:using tool|called tool|tool(?:[_\s-]?name)?)\s*[:=]\s*[`'"]?([A-Za-z_][\w-]*)/gi;

/** Conservative text fallback used only when stdout has no JSON events. */
export function toolsFromPlainText(text) {
  const found = [];
  const raw = String(text ?? "");
  for (const match of raw.matchAll(TEXT_TOOL_RE)) {
    addUnique(found, match[1]);
  }
  return found;
}

export function filePathFromInput(input) {
  if (!input || typeof input !== "object") return "";
  return String(input.file_path || input.filePath || input.path || "");
}

export function skillRefFromInput(input) {
  if (!input || typeof input !== "object") return "";
  return String(input.skill || input.skill_name || input.skillName || input.command || input.name || "");
}
