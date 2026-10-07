---
name: duped
description: "How to use the duped CLI to find duplicated functions and types: near-copy function bodies, files that import the same rare things, file pairs that declare the same names, and types with overlapping fields. Use before writing a new helper or type (\"does something like this already exist?\"), when you need the commands, flags, output fields or duped.toml settings, or when interpreting a duped finding. For a whole-repo audit use duped-audit; for checking a branch or pull request use duped-review."
---

# duped

`duped` finds duplicated code by parsing syntax with tree-sitter: no compiler, no language server, no embeddings. A 6,000-file monorepo takes about a second per analysis. Output is evidence for review; read both sides before acting on any of it.

## Install

```bash
command -v duped || cargo install --locked --git https://github.com/bryan-uipath/duped
```

## Commands

Every command takes `[PATH]` (default `.`) plus `--exclude <glob>` (repeatable, relative to `PATH`), `--include-tests`, `--root`, `--config`, `--out`. Analyses also take `--json` and `--top N`.

| Command | Finds | Languages |
|---|---|---|
| `duped bodies` | functions whose bodies are near-copies under any names (identifiers abstracted, literals kept); files linked through ≥2 such functions | TS/JS |
| `duped imports` | files importing the same rare set of modules and symbols: ported copies whose declarations were all renamed | TS/JS |
| `duped names` | file pairs declaring ≥2 of the same top-level names; each name marked `=` same signature/shape, `~` differs, `?` unknown | all |
| `duped types` | types sharing most of their fields under any names: `exact`, `subset`, `superset`, `overlap` | all |
| `duped index` | greppable markdown list of every top-level function and type | all |
| `duped extract` | the same, as JSON Lines records | all |

Tests, mocks, fixtures, `node_modules`, `dist`, `build` and `.gitignore`d paths are skipped by default.

To run all four analyses at once and group the results by file pair, use the bundled script (it's what duped-audit and duped-review run):

```bash
node <this skill's directory>/scripts/scan.mjs --repo <root> --all          # whole repo
node <this skill's directory>/scripts/scan.mjs --repo <root> --base <ref>   # pairs touching a branch's changed files
```

## Before writing new code

Check for prior art first; preventing a copy is cheaper than consolidating one.

```bash
duped index <root> --out /tmp/api-index.md
grep -i -E 'retry|backoff' /tmp/api-index.md        # domain words, not just the name you'd pick
```

Entries are grouped under a heading per file, each a signature with its line (`(L42)`) and doc summary. If something already does the job, use or extend it and say so. After writing, `duped bodies <root> --top 20` shows whether the new function copies an existing one.

## Reading a finding

- **Score is for ordering, not concluding.** `bodies` ranks by shared shingles (≈ duplicated tokens), so a large near-copy outranks a tiny exact one; `similarity` is Jaccard of normalised shingles.
- **Agreement between analyses is the strongest signal.** A file pair found by `bodies`, `imports` and `names` together is a ported module.
- **Drift matters more than duplication.** `~` in `names`, or `bodies` similarity below ~0.95, means the copies already differ; diff them. A behaviour difference can be a live bug.
- **The tag says what the dependency graph allows:**

| Tag | Meaning | Usual fix |
|---|---|---|
| `importable` | one side's module already depends on the other's | delete the copy, import the original |
| `importable-indirect` | it depends only through other modules | add the direct dependency, then import |
| `move-down` | neither depends on the other; both depend on a lower module (`home`) | move one copy down. `home` is *a* common dependency, not necessarily the best fit: pick the lowest module that suits the code |
| `boundary` | no dependency path either way | often deliberate; flag only real drift |
| `same-module` | both in one module (hidden by default except in `bodies`) | consolidate locally |

## Deliberate copies

`bodies` and `types` hide a pair when a doc comment or file header says "mirrors", "mirror of", "structural twin", "kept in sync" or "copy of" and names the other side. Otherwise record it in `duped.toml` at the project root:

```toml
[[acknowledged]]
a = "@x/web:formatBytes"      # Name, Scope.Name, or module:Name
b = "@x/cli:formatBytes"
reason = "cli is published standalone and can't depend on web"
```

`names` and `imports` don't apply acknowledgements yet. `--include-acknowledged` shows hidden pairs.

## Tuning

Other `duped.toml` sections: `[scan] exclude = [...]`, `[types]` and `[names]` thresholds, `[modules."<name>"]` (`ignore`, `path`, `deps`), `[rules.<language>]` test patterns. Flags win over the file. Common adjustments:

- Noise from directories meant to repeat themselves (examples, demos, generated code): `--exclude`, or `[scan] exclude`.
- `bodies` too strict or too loose: `--threshold` (default 0.5), `--min-tokens` (default 50).
- `imports` missing small files: `--min-shared 2` (default 3).
- `names` noisy: raise `--min-score` (default 2.5).
