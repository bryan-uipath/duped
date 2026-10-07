---
name: duped
description: "How to use the duped CLI to find duplicated functions and types: near-copy function bodies, files that import the same rare things, file pairs that declare the same names, and types with overlapping fields. Use before writing a new helper or type (\"does something like this already exist?\"), when you need the commands, flags, output fields or duped.toml settings, or when verifying and reporting a duped finding. For a whole-repo audit use duped-audit; for checking a branch or pull request use duped-review."
---

# duped

`duped` finds duplicated code by parsing syntax with tree-sitter: no compiler, no language server, no embeddings. A 6,000-file monorepo takes about a second per analysis. Output is evidence for review; read both sides before acting on any of it.

## Install

```bash
command -v duped || cargo install --locked --git https://github.com/bryan-uipath/duped
```

## Commands

Every command takes `[PATH]` (default `.`) plus `--exclude <glob>` (repeatable), `--include-tests`, `--root`, `--config`, `--out`. Analyses also take `--json` and `--top N`. Exclude globs are anchored at `PATH`: `examples` skips only `PATH/examples`; use `**/examples` for every `examples/` directory.

| Command | Finds | Languages |
|---|---|---|
| `duped bodies` | functions whose bodies are near-copies under any names (identifiers abstracted, literals kept); files linked through ≥2 such functions | TS/JS |
| `duped imports` | files importing the same rare set of modules and symbols: ported copies whose declarations were all renamed | TS/JS |
| `duped names` | file pairs declaring ≥2 of the same top-level names; each name marked `=` same signature/shape, `~` differs, `?` unknown | all |
| `duped types` | types sharing most of their fields under any names: `exact`, `subset`, `superset`, `overlap` | all |
| `duped index` | greppable markdown list of every top-level function and type | all |
| `duped extract` | the same, as JSON Lines records | all |

Tests, mocks, fixtures, `node_modules`, `dist`, `build` and `.gitignore`d paths are skipped by default.

### All analyses at once

`scripts/scan.mjs` (in this skill's directory; duped-audit and duped-review run it) runs the four analyses on a directory and groups the results by file pair:

```bash
node <this skill's directory>/scripts/scan.mjs --repo <dir> --all            # everything
node <this skill's directory>/scripts/scan.mjs --repo <dir> --base <ref>     # pairs touching files changed since <ref>
```

It also takes `--exclude <glob>` (forwarded) and `--top N` (default 40). Paths in the output are relative to `<dir>`. In the output:

- `detectors.<name>.status` is `ok` or `failed` (with `error`). Report a failed analysis; never treat it as "nothing found". `total` counts the analysis's pairs across the whole directory; `kept` counts those touching the change.
- `filePairs` is the number of groups before `--top`; `truncated` says whether some weren't printed.
- `groups` are sorted by how many analyses agree, then by number of findings. Each has `a`, `b`, `changed` (diff mode: which sides changed), `detectors`, `tag`, and `findings` with `evidence`.

## Before writing new code

Check for prior art first; preventing a copy is cheaper than consolidating one.

```bash
duped index <root> --out /tmp/api-index.md
grep -i -E 'retry|backoff' /tmp/api-index.md        # domain words, not just the name you'd pick
```

Entries are grouped under a heading per file, each a signature with its line (`(L42)`) and doc summary. If something already does the job, use or extend it and say so. After writing, `duped bodies <root> --top 20` shows whether the new function copies an existing one.

## Reading a finding

- **Scores order your reading; they don't conclude.** `bodies` ranks by shared shingles (≈ duplicated tokens), so a large near-copy outranks a tiny exact one; `similarity` is Jaccard of normalised shingles.
- **Agreement between analyses is the strongest signal.** A file pair found by `bodies`, `imports` and `names` together is a ported module.
- **Drift matters more than duplication.** `~` in `names`, or `bodies` similarity below ~0.95, means the copies already differ; diff them. A behaviour difference can be a live bug.
- **`tag` says what the dependency graph allows.** In JSON it's an object, `null` when modules aren't tracked; `tag.kind` is one of:

| `tag.kind` | Meaning | Usual fix |
|---|---|---|
| `importable` | module `tag.copy` already depends on module `tag.owner` | the side in `tag.copy` deletes its version and imports `tag.owner`'s, whichever side the change touched |
| `importable-indirect` | `tag.copy` depends on `tag.owner` only through other modules | add the direct dependency, then as above |
| `move-down` | neither depends on the other; both depend on `tag.home` | move one copy down. `home` is *a* common dependency, not necessarily the best fit: pick the lowest module that suits the code |
| `boundary` | no dependency path either way | often deliberate; flag only real drift |
| `same-module` | both in `tag.module` (hidden by default except in `bodies`) | consolidate locally |

## Verifying a finding

Open both sides and the nearby callers before calling anything a duplicate.

Not worth reporting:

- **Structurally forced**: interface implementations, overrides, framework boilerplate that must share a shape.
- **Intentional families and inverses**: `toX`/`fromX`, `migrate`/`downgrade`, `translateX`/`translateY`. Symmetry is the point.
- **Documented boundaries**: a comment explaining *why* the copy exists (a package that must stay dependency-free, a host that can't import another). "Ported from X" alone is not a reason.
- **Small helpers and shapes** where a shared home would cost more than the copy.
- **Test scaffolding**, unless the duplicated logic is substantial.

Worth reporting: two implementations of one idea in different places, and above all copies that **disagree** (one validates, scopes or handles an error that the other doesn't).

Blind spot: all of this is syntax. A rewrite that shares no structure, names or imports won't appear.

## Reporting

One line per finding, strongest first:

```
<tag> <what to cut>. <what to use instead>. [path:line <-> path:line] (<analyses and evidence>)
```

Tags: `diverged` (copies behave differently; probable bug, list first), `reuse` (call the existing code), `merge` (two implementations of one idea; pick one), `extract` (both move to a shared home).

Say what ran: the directory, the base ref if any, and per-analysis counts. If nothing survives verification, say **"No actionable duplication."** with those numbers. Never pad a clean result.

## Deliberate copies

`bodies` and `types` hide a pair when a doc comment or file header says "mirrors", "mirror of", "structural twin", "kept in sync" or "copy of" and names the other side. Otherwise record it in `duped.toml` at the project root:

```toml
[[acknowledged]]
a = "@x/web:formatBytes"      # Name, Scope.Name, or module:Name
b = "@x/cli:formatBytes"
reason = "cli is published standalone and can't depend on web"
```

Acknowledgements apply to `bodies` and `types` only; `names` and `imports` keep reporting the pair. `--include-acknowledged` shows hidden pairs.

## Tuning

Other `duped.toml` sections: `[scan] exclude = [...]`, `[types]` and `[names]` thresholds, `[modules."<name>"]` (`ignore`, `path`, `deps`), `[rules.<language>]` test patterns. Flags win over the file. Common adjustments:

- Noise from directories meant to repeat themselves (examples, demos, generated code): `--exclude '**/examples'`, or `[scan] exclude`.
- `bodies` too strict or too loose: `--threshold` (default 0.5), `--min-tokens` (default 50).
- `imports` missing small files: `--min-shared 2` (default 3).
- `names` noisy: raise `--min-score` (default 2.5).
