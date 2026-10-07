---
name: duped-review
description: "Check a branch, pull request or local diff for code that duplicates existing code, using duped (about two seconds repo-wide, no embeddings). Finds new functions that near-copy existing ones, files ported from another package, re-declared types, and copies that have already drifted. Use when reviewing a PR or branch for duplication, as the duplication step of a larger code review, or after porting code between packages. For a whole-repo audit, use duped-audit."
---

# duped-review

Answers "does this *change* duplicate something that already exists?" Uses the `duped` skill, installed next to this one: its `scripts/scan.mjs`, and its **Reading a finding**, **Verifying a finding** and **Reporting** sections.

**Scope: duplication only.** Correctness, security and style belong to other reviewers. Read-only: don't edit files.

## 1. Run

```bash
BASE=<the PR's base branch; for a stacked PR, its parent branch, not main>
node <duped skill directory>/scripts/scan.mjs --repo <repo root> --base "$BASE" > /tmp/duped-review.json
```

Candidates are generated across the whole repo, so copies of *unchanged* code are found; the output keeps only file pairs where a side was added or modified since the merge base (committed, uncommitted or untracked). Filtering is per file: confirm the duplicated code is in the changed lines before reporting.

## 2. Read the groups

`changed` says which side the change touched. Work in this order:

1. **2–3 analyses agree** (`bodies` + `imports` + `names`): almost always a ported module.
2. **`bodies` alone**: a function body near-copies another under different names. The most precise single analysis.
3. **`names`**: `=` names share a signature or shape (`(exported)` when both sides export them); `~` names are copies that already differ.
4. **`types`**: a new type matches an existing one field for field (`exact`) or extends it (`superset`); look for one that can be imported instead.
5. **`imports` alone**: a new file imports the same unusual set as an existing one. Confirm with the code; per-host setup files (i18n, bootstrapping) are often deliberate.

## 3. Verify and report

Read both sides, as in the `duped` skill. For the fix direction, follow `tag.copy → tag.owner` regardless of which side changed: if the change added the *owner*-side copy, the recommendation is that the existing downstream copy import the new one, or that the change reuse what's already there; never a dependency `tag` doesn't allow.

Report in the `duped` skill's format, with the changed side first in `[changed path:line <-> existing path:line]`. State the base ref, `changedFiles`, and each analysis's `kept` count. For a pair confirmed deliberate, suggest an `[[acknowledged]]` entry when it came from `bodies` or `types`; `names` and `imports` don't apply acknowledgements.
