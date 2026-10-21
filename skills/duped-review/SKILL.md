---
name: duped-review
description: "Review a branch, PR, or local diff for duplicated code with duped. Use for a requested duplication check or the dedicated duplication step of xreview. For a whole-codebase audit, use duped-audit."
---

# duped-review

Answers "does this *change* duplicate something that already exists?" This
workflow is self-contained; read the sibling `duped/references/cli.md` only
for installation, unfamiliar flags, dependency tags, or configuration.

**Scope: duplication only.** Correctness, security and style belong to other reviewers. Read-only: don't edit files.

## 1. Run

Reuse a supplied report only when the caller states it matches the current
repository, base, options, and working tree. Otherwise run once into a unique
temporary file and use the printed literal path in later commands (shell
variables don't persist between calls).

```bash
BASE=<the PR's base branch; for a stacked PR, its parent branch, not main>
REPORT=$(mktemp /tmp/duped-review.XXXXXX)
duped report <repo root> --base "$BASE" --json > "$REPORT"; echo "$REPORT"
```

Candidates are generated across the whole repo, so copies of *unchanged* code are found; the output keeps only file pairs where a side was added or modified since the merge base (committed, uncommitted or untracked). Filtering is per file: confirm the duplicated code is in the changed lines before reporting.

## 2. Read the groups

Inspect detector statuses, counts, and `truncated` before selected groups;
avoid dumping the full JSON into context. Report failed detectors as gaps.
If truncated (`--top` defaults to 40, JSON included), rerun once with a larger
`--top` or state the coverage limit; do not claim a complete clean result from a subset.

`changed` says which side the change touched. Work in this order:

1. **2–3 analyses agree** (`bodies` + `imports` + `names`): almost always a ported module.
2. **`bodies` alone**: a function body near-copies another under different names. The most precise single analysis.
3. **`names`**: `=` names share a signature or shape (`(exported)` when both sides export them); `~` names are copies that already differ.
4. **`types`**: a new type matches an existing one field for field (`exact`) or extends it (`superset`); look for one that can be imported instead.
5. **`imports` alone**: a new file imports the same unusual set as an existing one. Confirm with the code; per-host setup files (i18n, bootstrapping) are often deliberate.

## 3. Coupling delta

```bash
duped coupling <repo root> --base "$BASE" --json > "$REPORT.coupling"
```

Compares head against the merge base. `totals` is head, `diff.totals` is
base. `score` and `hiding` are better higher; `bypassRate` and
`unusedSurface` better lower. Flag everything that got worse:

- `diff.regressed`: an import now reaches past an entry file, or deeper than
  before. Recommend importing through the entry, or moving the symbol. A
  `priorImporters` count of 5 or more means other files already reach the
  same symbol the same way: an established convention, so note it in one
  line instead of flagging each one.
- `diff.removedWithoutMove`: an import vanished but the symbol stayed put:
  the code was likely copied or inlined (`grew: true` means the importer grew
  by at least the symbol's size). Cross-check against the duplication groups.
- `diff.surface` without `newModule`: an existing module's API widened. Ask
  whether each new export is used outside the module; unused ones lower
  `hiding` and raise `unusedSurface`. Surface on a `newModule` is expected;
  note it in one line.
- `diff.leaks.added`: an internal symbol gained an importer from outside.

Report the four ratios before and after. Improvements (`diff.improved`,
`leaks.removed`) get one line. Measure only; no threshold fails a PR.

## 4. Verify and report

Read both sides and relevant callers. Drop framework-required similarities,
intentional inverses/families, documented package boundaries, and tiny copies
whose consolidation would cost more. Scores nominate candidates; they do not
prove duplication. Prioritize copies whose behavior has drifted.

For the fix direction, follow `tag.copy → tag.owner` regardless of which side changed: if the change added the *owner*-side copy, the recommendation is that the existing downstream copy import the new one, or that the change reuse what's already there; never a dependency `tag` doesn't allow.

Report one line per verified finding: `diverged|reuse|merge|extract`, what to
change and why, and `[changed path:line <-> existing path:line]`. Coupling
findings use `reach|leak|surface` the same way, e.g.
`reach: import X via packages/foo (entry), not foo/internal/x [path:line]`. State the base,
`changedFiles`, each detector's status and `kept` count, and coverage limits. If none survive,
say "No actionable duplication" with that scope. For deliberate pairs, suggest
acknowledgements only for `bodies`/`types`; `names`/`imports` ignore them.
