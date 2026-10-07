---
name: duped-review
description: "Check a branch, pull request or local diff for code that duplicates existing code, using duped (about two seconds repo-wide, no embeddings). Finds new functions that near-copy existing ones, files ported from another package, re-declared types, and copies that have already drifted. Use when reviewing a PR or branch for duplication, as the duplication step of a larger code review, or after porting code between packages. For a whole-repo audit, use duped-audit."
---

# duped-review

Answers "does this *change* duplicate something that already exists?" Requires the `duped` skill installed alongside this one (for `scripts/scan.mjs`) and the `duped` binary (`cargo install --locked --git https://github.com/bryan-uipath/duped`).

**Scope: duplication only.** Correctness, security and style belong to other reviewers. Read-only: don't edit files.

## 1. Run

```bash
BASE=<the PR's base branch; for a stacked PR, its parent branch, not main>
node <duped skill directory>/scripts/scan.mjs --repo <root> --base "$BASE" > /tmp/duped-review.json
```

(From this skill's directory, the script is at `../duped/scripts/scan.mjs`.) It compares the whole repo, then keeps file pairs where either side is a file the change added or modified, including uncommitted and untracked files. Copies of *unchanged* code are found because candidate generation is repo-wide.

Check `.detectors`: each should be `"status": "ok"`; report a failed one rather than skipping it. Filtering is per file, so confirm the duplicated code is in the changed lines before reporting.

## 2. Read the groups

`groups` are file pairs; `changed` says which side the change touched; `detectors` lists the analyses that agree. Work in this order:

1. **2–3 analyses agree** (`bodies` + `imports` + `names`): almost always a ported module. In evaluation on a large monorepo, the PR that introduced a ported component ranked it first of 18 pairs with three analyses agreeing.
2. **`bodies`, alone**: a function body near-copies another under different names. The most precise single analysis.
3. **`names` with `=` entries**: the change re-declares exported names with identical signatures; `~` entries are copies that already differ.
4. **`types`**: a new type matches an existing one field for field (`exact`) or extends it (`superset`); look for one that can be imported instead.
5. **`imports`, alone**: a new file imports the same unusual set as an existing one. Confirm with the bodies; per-host setup files (i18n, bootstrapping) are often deliberate.

## 3. Verify each candidate

Open both sides and the nearby callers. Then decide:

- **Genuine duplication**: the change re-implements existing code. Recommend reusing or extending it (name the symbol and `file:line`), or extracting a shared home. Use the `tag`: `importable` means the changed side can import the original today; `move-down` means both can depend on a common lower module (judge whether the suggested `home` fits); `boundary` means neither can depend on the other.
- **Diverged duplication**, the strongest finding: two copies of one idea that already behave differently (one validates, scopes or handles an error the other doesn't). Diff them and report the difference as a probable bug.
- **Acceptable**: intentional families and inverse pairs, documented boundaries (a header saying why the copy exists), interface implementations, small helpers where sharing costs more than copying, test setup. Say why in one line; no finding.

Blind spot: everything here is syntax. A rewrite that shares no structure, names or imports won't appear; skim the change for logic you recognise from elsewhere.

## 4. Report

One line per finding, strongest first:

```
<tag> <what to cut>. <what to use instead>. [changed path:line <-> existing path:line] (<analyses + evidence>)
```

Tags: `diverged` (copies behave differently; probable bug), `reuse` (call the existing code), `merge` (two implementations of one idea), `extract` (both move to a shared home).

State what ran: base ref, changed files, pairs per analysis. If nothing survives verification, say **"No actionable duplication."** with those numbers. Never pad a clean result.

If a pair is confirmed deliberate, suggest the `[[acknowledged]]` entry for `duped.toml` (see the `duped` skill) so it stops being reported.
