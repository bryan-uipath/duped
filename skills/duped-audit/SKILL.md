---
name: duped-audit
description: "Audit a whole codebase for duplicated code with duped: runs near-copy body, rare-import, shared-name and type-shape analyses (about a second each), groups candidates by file pair, then verifies each by reading both sides and produces a ranked cleanup list. Use when asked to audit a repo or package for duplication (\"where is the duplication in this codebase?\", \"find copy-pasted code\", \"what should we consolidate?\"). For checking a branch or pull request, use duped-review."
---

# duped-audit

Answers "what does this repo duplicate?" and returns a ranked, verified cleanup list. Requires the `duped` skill installed alongside this one (for `scripts/scan.mjs`) and the `duped` binary (`cargo install --locked --git https://github.com/bryan-uipath/duped`).

**Scope: duplication only.** Mention any bug you notice in one closing line; don't turn this into a general review. **Report only; don't apply fixes** unless separately asked.

## 1. Decide exclusions

The main source of noise is directories whose contents are *meant* to repeat each other: self-contained examples and demos, generated code, vendored code, fixtures, versioned snapshots (`v1/`, `v2/` copies kept on purpose). Exclude them up front with `--exclude <glob>`. If unsure, run once without, see what dominates the top, and rerun.

If the repo has no `duped.toml` and you exclude the same paths more than once, suggest adding `[scan] exclude = [...]` so future runs agree.

## 2. Run

```bash
node <duped skill directory>/scripts/scan.mjs --repo <root> --all --top 60 --exclude 'examples' > /tmp/duped-audit.json
```

(From this skill's directory, the script is at `../duped/scripts/scan.mjs`.) Check `.detectors`: each should be `"status": "ok"`. Report any failed detector rather than silently skipping it.

`groups` are file pairs, sorted by how many analyses agree, then by number of findings. For a wider net on one analysis, run it directly, e.g. `duped bodies <root> --threshold 0.4 --json`.

## 3. Verify, by reading both sides

Every group is a hypothesis until you've opened both files. Work through them in order; 3-analysis pairs first.

Discard without reporting:

- **Structurally forced**: interface implementations, overrides, framework boilerplate that must share a shape.
- **Intentional families and inverses**: `toX`/`fromX`, `migrate`/`downgrade`, `translateX`/`translateY`. Symmetry is the point.
- **Documented boundaries**: a header that explains *why* the copy exists (a package that must have no dependencies, a host that can't import another). "Ported from X" alone is not a reason.
- **Small shapes and helpers** where a shared home would cost more than the copy.
- **Test scaffolding**, unless the duplicated logic is substantial.

Keep and investigate: two implementations of one idea in different modules, especially when they **disagree**. Diff every near-copy (`bodies` similarity below ~0.95, `~` in `names`): a behaviour difference is evidence of a live bug, not just redundancy.

Use the pair's `tag` to choose the fix: `importable` means import the original; `move-down` means move one copy to a common lower module (judge whether `home` actually fits the code); `boundary` usually means leave it, unless drift is real.

## 4. Report, biggest cut first

Rank by payoff (lines removable × how many places must stay in sync), not by score. A 0.7 pair spanning two packages beats a 0.95 pair inside one module. One line per finding:

```
<tag> <what to cut>. <what to use instead>. [path:line <-> path:line] (<analyses>)
```

Tags: `merge` (two implementations of one idea; pick one), `reuse` (call the existing code), `extract` (both move to a shared home), `diverged` (copies behave differently; probable bug, list these first).

Close with:

```
net: -<N> lines across <M> sites; <K> pairs confirmed by 2+ analyses. Scanned: <files>, excluded: <globs>.
```

If nothing survives verification, say **"No actionable duplication."** with the scan sizes. A clean audit is a real result; don't pad it.

Offer to add `[[acknowledged]]` entries to `duped.toml` for deliberate pairs you confirmed, so the next audit skips them (see the `duped` skill).
