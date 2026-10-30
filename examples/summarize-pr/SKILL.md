---
name: summarize-pr
description: Summarize a local pull-request diff into a short markdown brief.
---

# summarize-pr

When the user asks to summarize a PR, pull request, or diff:

1. Read the local file `pr.diff` in the working directory.
2. Write `summary.md` with:
   - one `#` title line describing the change
   - exactly three bullet points of what changed

If `pr.diff` is missing or empty, do **not** invent a summary. Reply clearly that `pr.diff` is missing or empty and stop.

Do not run this skill for unrelated questions (e.g. weather, jokes, unrelated coding help).
