---
name: duped-coupling
description: "Measure and reduce coupling past module entry files with duped coupling: rank the most reached-into boundaries, propose per-edge fixes, apply them, and prove the improvement with a before/after delta. Use when asked where a codebase is most tangled, which modules leak internals, or to refactor toward better encapsulation. For duplication use duped-audit or duped-review."
---

# duped-coupling

Answers "what reaches into what, and how do we reduce it?" See the
`duped/references/cli.md` **Coupling** section for definitions and flags.
Numbers are evidence; the score never gates anything.

## 1. Measure

```bash
duped coupling <dir> --top 30 --json > <temporary-file>
```

Read `totals` (`score`, `bypassRate`, `unusedSurface`) and `boundaries`
(worst first). Skip `noConsumers` boundaries: nothing in the repo imports
them, so their surface can't be judged. `--folder <path>` scores one
subtree. For a trend, loop `--rev` over `git rev-list --first-parent`.

## 2. Pick a target

Take the boundary with the most incoming bypass edges plus leaks. Group its
`edges` by target file and its `leaks` by symbol. Three fixes, in order of
preference:

1. **Narrow the API.** The importer needs a capability, not the internal
   symbol: add or widen one function at the entry and call that. Lowers
   bypass and keeps hiding high.
2. **Move down.** A symbol with importers from several boundaries belongs
   in a lower shared module with its own entry. Use the dependency `tag`
   rules from `duped` so the move respects module direction.
3. **Re-export from the entry.** Cheapest, but it converts a leak into
   declared surface and lowers hiding. Acceptable when the symbol is
   genuinely part of the module's contract.

Never copy or inline the target into the importer: the edge disappears but
`--base` reports it as `removedWithoutMove` and gives no credit.

## 3. Apply and prove

Make the change on a branch, then:

```bash
duped coupling <dir> --base <ref> --json
```

Report `diff.totals` (score and bypassRate before and after), the
`improved` count, and confirm `regressed` and `removedWithoutMove` are
empty. Run `duped report <dir> --base <ref>` once to confirm no new
duplication. One line per fix:

```text
reach|leak|surface: what moved and why [importer:line -> target] (bypass d -> d')
```
