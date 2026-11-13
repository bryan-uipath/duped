---
name: duped-audit
description: "Audit a whole codebase or package for duplicated code with duped: runs near-copy body, rare-import, shared-name and type-shape analyses in one pass, groups candidates by file pair, then verifies each by reading both sides and produces a ranked cleanup list. Use when asked to audit a repo or package for duplication (\"where is the duplication in this codebase?\", \"find copy-pasted code\", \"what should we consolidate?\"). For checking a branch or pull request, use duped-review."
---

# duped-audit

Answers "what does this codebase duplicate?" with a ranked, verified cleanup list. Uses the `duped` skill, installed next to this one: its **All analyses at once** (`duped report`), **Reading a finding**, **Verifying a finding** and **Reporting** sections.

**Scope: duplication only.** Mention any bug you notice in one closing line. **Report only; don't apply fixes** unless separately asked.

## 1. Decide exclusions

The main noise is directories whose contents are *meant* to repeat each other: examples and demos, generated or vendored code, fixtures, versioned snapshots kept on purpose. Exclude them up front; globs are anchored at the scanned directory, so use `**/examples` for nested ones. If unsure, run once without, see what dominates the top, and rerun.

Tests, mocks and fixtures (`*.test.*`, `tests/`, `__tests__/`, …) are skipped by default. The report's `scannedFiles` and `skippedTestFiles` say how many: check them before concluding a repo is clean, and pass `--include-tests` when test code is in scope. State both counts in the closing line.

## 2. Run

```bash
REPORT=$(mktemp /tmp/duped-audit.XXXXXX)
duped report <dir> --top 60 --exclude '**/examples' --json > "$REPORT"; echo "$REPORT"
```

Use the printed literal path in later commands (shell variables don't persist between calls).

`<dir>` can be the repo root or one package. If `truncated` is true, raise `--top` before concluding anything about coverage. It doesn't say which list was cut: compare `filePairs` with `groups | length`, `helperGroups` with `helpers | length` and `helperFamilies` with `families | length`; `--top` at the largest count lifts every cut. For a wider net on one analysis, run it directly into a file, e.g. `duped bodies <dir> --threshold 0.4 --json > <temporary-file>`: only `report --json` honours `--top`, the others print every pair.

## 3. Verify

Start with `directories`: a directory pair with several file pairs is one port, and one cleanup item. Then work through `groups` in order (most removable lines first), reading both sides of each, as in the `duped` skill. Diff every near-copy: copies that disagree are the most valuable findings.

## 4. Coupling hotspots

```bash
duped coupling <dir> --json > <report path>.coupling
jq '{totals: (.totals | {score, bypassRate, unusedSurface, hiding, crossBoundaryEdges}), boundaries: [.boundaries[] | select(.noConsumers | not)][:10]}' <report path>.coupling
```

Coupling's `--json` ignores `--top` and can run to megabytes: select with `jq`, don't load the file. If `crossBoundaryEdges` is 0 or every boundary is `noConsumers` (a single-package repo), say "coupling: nothing crosses a boundary" in one line and skip this section.

`totals.score` (0–1, higher is better) averages `1 − bypassRate`,
`1 − unusedSurface` and `hiding`; report all four. `boundaries` is worst
first. Skip `noConsumers` ones (nothing in the scan imports them). For the
top few, say which term drags them down and name the evidence:

- low `1 − bypassRate`, many `leaks`: internal files others reach into.
  Group `edges` by target and `leaks` by symbol; the most-imported internals
  are the consolidation targets:
  `jq --arg d <boundary dir> '[.edges[] | select(.target | startswith($d + "/"))] | group_by(.target) | map({target: .[0].target, importers: length}) | sort_by(-.importers)[:10]'`
  and `jq --arg d <boundary dir> '[.leaks[] | select(.boundary == $d) | {symbol, file, importers: (.importers | length)}] | sort_by(-.importers)[:10]'`.
- high `unusedSurface`: the entry exports more than anyone uses; often a
  barrel. List the unused names.
- low `hiding`: most declarations are on the face or imported from outside.

The fix workflow lives in `duped-coupling`.

## 5. Report, biggest cut first

Use the `duped` skill's report format, but rank by payoff (lines removable × how many places must stay in sync), not by score: a 0.7 pair spanning two packages beats a 0.95 pair inside one module. Close with:

```
net: -<N> lines across <M> sites; <K> pairs confirmed by 2+ analyses. Scanned: <dir>, <scannedFiles> files, <skippedTestFiles> test files skipped, excluded: <globs>.
coupling: score <s> (bypassRate <b>, unusedSurface <u>, hiding <h>); hotspots: <boundary> (<n> bypass in, <m> leaks, <term>), …
```

Afterwards, offer to persist what you learned in `duped.toml`: repeated exclusions as `[scan] exclude = [...]`, and confirmed-deliberate `bodies`/`types` pairs as `[[acknowledged]]` entries (`names` and `imports` don't apply acknowledgements).
