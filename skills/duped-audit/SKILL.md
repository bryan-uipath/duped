---
name: duped-audit
description: "Audit a whole codebase or package for duplicated code with duped: runs near-copy body, rare-import, shared-name and type-shape analyses (about a second each), groups candidates by file pair, then verifies each by reading both sides and produces a ranked cleanup list. Use when asked to audit a repo or package for duplication (\"where is the duplication in this codebase?\", \"find copy-pasted code\", \"what should we consolidate?\"). For checking a branch or pull request, use duped-review."
---

# duped-audit

Answers "what does this codebase duplicate?" with a ranked, verified cleanup list. Uses the `duped` skill, installed next to this one: its **All analyses at once** (`duped report`), **Reading a finding**, **Verifying a finding** and **Reporting** sections.

**Scope: duplication only.** Mention any bug you notice in one closing line. **Report only; don't apply fixes** unless separately asked.

## 1. Decide exclusions

The main noise is directories whose contents are *meant* to repeat each other: examples and demos, generated or vendored code, fixtures, versioned snapshots kept on purpose. Exclude them up front; globs are anchored at the scanned directory, so use `**/examples` for nested ones. If unsure, run once without, see what dominates the top, and rerun.

## 2. Run

```bash
duped report <dir> --top 60 --exclude '**/examples' --json > /tmp/duped-audit.json
```

`<dir>` can be the repo root or one package. If `truncated` is true, raise `--top` before concluding anything about coverage. For a wider net on one analysis, run it directly, e.g. `duped bodies <dir> --threshold 0.4 --json`.

## 3. Verify

Work through `groups` in order (most analyses agreeing first), reading both sides of each, as in the `duped` skill. Diff every near-copy: copies that disagree are the most valuable findings.

## 4. Report, biggest cut first

Use the `duped` skill's report format, but rank by payoff (lines removable × how many places must stay in sync), not by score: a 0.7 pair spanning two packages beats a 0.95 pair inside one module. Close with:

```
net: -<N> lines across <M> sites; <K> pairs confirmed by 2+ analyses. Scanned: <dir>, excluded: <globs>.
```

Afterwards, offer to persist what you learned in `duped.toml`: repeated exclusions as `[scan] exclude = [...]`, and confirmed-deliberate `bodies`/`types` pairs as `[[acknowledged]]` entries (`names` and `imports` don't apply acknowledgements).
