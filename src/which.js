/**
 * PATH lookup without a shell. Returns an executable file path or null.
 */
import fs from "node:fs";
import path from "node:path";

export function findOnPath(cmd, env = process.env) {
  if (!cmd) return null;
  const entries = String(env.PATH || "").split(path.delimiter);
  for (const dir of entries) {
    if (!dir) continue;
    const full = path.join(dir, cmd);
    try {
      const st = fs.statSync(full);
      if (!st.isFile()) continue;
      fs.accessSync(full, fs.constants.X_OK);
      return full;
    } catch {
      // not this entry
    }
  }
  return null;
}
