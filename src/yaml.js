/**
 * Minimal YAML subset parser for SkillParity parity.yaml (no deps).
 * Supports: top-level scalars, `tasks:` list of maps, nested `expect:` maps/lists,
 * booleans, numbers, quoted/unquoted strings, # comments.
 */

export function parseParityYaml(text) {
  const root = {};
  const lines = text.split(/\r?\n/);

  // First: top-level scalar keys (anything before tasks:)
  for (const raw of lines) {
    if (!raw.trim() || raw.trim().startsWith("#")) continue;
    const indent = leadingSpaces(raw);
    if (indent !== 0) continue;
    const line = raw.trim();
    if (line === "tasks:" || line.startsWith("tasks:")) break;
    const { key, value, empty } = splitKV(line);
    if (!key || key === "tasks") continue;
    if (!empty) root[key] = parseScalar(value);
  }

  root.tasks = parseTaskList(lines);
  return root;
}

function parseTaskList(lines) {
  const tasks = [];
  let i = 0;

  // find tasks:
  while (i < lines.length) {
    const t = lines[i].trim();
    if (t === "tasks:" || t.startsWith("tasks:")) {
      i++;
      break;
    }
    i++;
  }

  let current = null;
  let mode = "task"; // task | expect | expect-list
  let listKey = null;

  for (; i < lines.length; i++) {
    const raw = lines[i];
    if (!raw.trim() || raw.trim().startsWith("#")) continue;
    const indent = leadingSpaces(raw);
    const line = raw.trim();

    // new task item at indent 2 (or any "- " that starts a task)
    if (line.startsWith("- ") && indent <= 2) {
      current = {};
      tasks.push(current);
      mode = "task";
      listKey = null;
      const rest = line.slice(2).trim();
      if (rest) {
        const { key, value, empty } = splitKV(rest);
        if (key) current[key] = empty ? null : parseScalar(value);
      }
      continue;
    }

    if (!current) continue;

    if (mode === "expect-list" && line.startsWith("- ")) {
      current.expect[listKey].push(parseScalar(line.slice(2).trim()));
      continue;
    }

    const { key, value, empty } = splitKV(line);
    if (!key) continue;

    if (key === "expect" && empty) {
      current.expect = {};
      mode = "expect";
      listKey = null;
      continue;
    }

    if (mode === "expect" || mode === "expect-list") {
      // leaving expect block if key at task field indent (~4) and not deeper list
      if (indent <= 4 && key !== listKey && !empty && mode === "expect-list") {
        // still in expect if indent is 4
      }
      if (indent >= 4) {
        if (empty) {
          // start list under expect
          current.expect[key] = [];
          mode = "expect-list";
          listKey = key;
        } else {
          current.expect[key] = parseScalar(value);
          mode = "expect";
          listKey = null;
        }
        continue;
      }
      // indent < 4 → back to task fields
      mode = "task";
      listKey = null;
    }

    // task-level field
    if (empty) {
      current[key] = null;
    } else {
      current[key] = parseScalar(value);
    }
  }

  return tasks;
}

function leadingSpaces(s) {
  const m = s.match(/^[ \t]*/);
  return m ? m[0].replace(/\t/g, "  ").length : 0;
}

function splitKV(line) {
  const c = line.indexOf(":");
  if (c < 0) return { key: null, value: "", empty: true };
  const key = line.slice(0, c).trim();
  const value = line.slice(c + 1).trim();
  return { key, value, empty: value === "" };
}

function parseScalar(s) {
  let t = s.trim();
  const hash = t.search(/\s+#/);
  if (hash >= 0) t = t.slice(0, hash).trim();
  if (t === "true") return true;
  if (t === "false") return false;
  if (t === "null" || t === "~") return null;
  if (t === "[]") return [];
  if (t === "{}") return {};
  if (/^-?\d+(\.\d+)?$/.test(t)) return Number(t);
  if (
    (t.startsWith('"') && t.endsWith('"')) ||
    (t.startsWith("'") && t.endsWith("'"))
  ) {
    const inner = t.slice(1, -1);
    // unescape common sequences inside quotes
    return inner.replace(/\\n/g, "\n").replace(/\\"/g, '"').replace(/\\'/g, "'").replace(/\\\\/g, "\\");
  }
  return t;
}
