---
name: duped
description: "Use the duped CLI for requested duplication or existing-code searches, concrete suspected copies, and duped command/configuration help. Ordinary helper or type creation alone does not trigger this skill. For branch reviews use duped-review; for whole-codebase audits use duped-audit."
---

# duped

Syntax-based duplication detection. Use a targeted `rg` search for routine
reuse checks; do not run a repository-wide scan merely because code is being
written. Use duped when the task calls for duplication analysis or an existing
code search needs structural evidence.

## Choose the scope

- Branch/PR duplication: use the sibling `duped-review` skill alone.
- Whole-codebase duplication: use `duped-audit` and the essentials below.
- Coupling, encapsulation, or "what reaches into what": use `duped-coupling`.
- A specific reuse question: search names/domain terms with `rg` first. If
  needed, save `duped index <root> --out <temporary-file>` and search that file;
  do not load the entire index or automatically run a follow-up scan.
- CLI installation, individual detectors, flags, dependency tags, exclusions,
  or acknowledgements: read the relevant section of [CLI reference](references/cli.md).

## All analyses at once

```bash
duped report <root> --base <ref> --json > <temporary-file>  # changed-file pairs
duped report <root> --json > <temporary-file>               # whole codebase
```

The report combines bodies, imports, names, and types, grouped by file pair.
Tests, fixtures, build output (`node_modules`, `dist`, `build`, `out`,
`target`, …) and `.gitignore`d paths are skipped by default; checked-in
generated code is not. Paths are relative to the scan root. Use a unique
temporary file per run and pass its literal path to later commands. Reuse a
report only when the caller states it matches the current tree; else rerun.

Inspect `detectors` statuses and counts (`kept` touches the change, `total` is
repo-wide), `filePairs`, and `truncated`, then select
relevant `groups`; avoid dumping the full JSON. Failed detectors are gaps,
not clean results. `--top` defaults to 40 even for JSON. Expand it when needed
for coverage or disclose that only the selected groups were reviewed.

## Reading a finding

Scores prioritize inspection, not conclusions. Agreement between detectors
is stronger evidence. Prioritize behavioral drift: `~` in `names`, or `bodies`
similarity below ~0.95, means the copies already differ; diff them. With
`--base`, `changed` identifies changed files, not changed lines: confirm the
duplicated code belongs to the change. Respect dependency direction (`tag.copy` imports
`tag.owner`); consult the reference for other tag kinds.

## Verifying a finding

Read both implementations and relevant callers. Exclude framework-required
shapes, intentional inverses/families, documented dependency boundaries,
small copies whose abstraction costs more, and trivial test scaffolding.
Report shared logic that should have one owner or copies that disagree.
Syntax-based scans cannot rule out structurally different semantic duplicates.

## Reporting

One line per verified finding, strongest first:

```text
diverged|reuse|merge|extract: what to change and why [path:line <-> path:line] (detectors, evidence)
```

`diverged`: copies behave differently, probable bug, list first. `reuse`: call
the existing code. `merge`: two implementations of one idea, keep one.
`extract`: both move to a shared home.

State scan root/base, detector statuses/counts, and coverage limits. If nothing
survives verification, say "No actionable duplication" within that scope.
