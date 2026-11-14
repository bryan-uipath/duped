# duped CLI reference

Read the relevant section for installation, detector selection, dependency tags, acknowledgements, or tuning.

## Install

```bash
printf '%s\n' "duped 0.3.0" "$(duped --version 2>/dev/null)" | sort -V -C || cargo install --locked --force --git https://github.com/bryan-uipath/duped   # needs 0.3.0 or newer
```

## Commands

Every command takes `[PATH]` (default `.`) plus `--exclude <glob>` (repeatable), `--include-tests`, `--root`, `--config`, `--out`. Analyses also take `--json` and `--top N` (default 40). Only `report --json` honours `--top`; every other `--json` prints everything (`coupling`: megabytes on a monorepo), so write it to a file and select with `jq`. Exclude globs are anchored at `PATH`: `examples` skips only `PATH/examples`; use `**/examples` for every `examples/` directory.

| Command | Finds | Languages |
|---|---|---|
| `duped bodies` | functions whose bodies are near-copies under any names (identifiers abstracted, literals kept), `kind` `similar`; a body mostly inside another (`contained`) or a ≥15-line block in both (`block`), with `region` line ranges to extract; files linked through ≥2 `similar` functions | TS/JS, Rust |
| `duped helpers` | small top-level helpers (8–49 tokens, below `bodies`) copied with identical bodies, as N-way groups with a home; same-name helpers whose copies drifted (`isRecord`: 28 definitions, 6 bodies). Plain parameters renamed by position, other names (and destructured keys) kept | TS/JS, Rust |
| `duped imports` | files importing the same rare set of modules and symbols: ported copies whose declarations were all renamed | TS/JS |
| `duped names` | file pairs declaring ≥2 of the same top-level names; each name marked `=` same signature/shape, `~` differs, `?` unknown | all |
| `duped types` | types sharing most of their fields under any names: `exact`, `subset`, `superset`, `overlap` | all |
| `duped fns` | functions sharing most of their parameter names under any names; a prototype with a weak signal, not part of `report` | all |
| `duped report` | every analysis plus identical files, grouped by file pair and ranked by `removableLines`, then `helpers` groups ranked apart, then drifted `families` (see SKILL.md) | all |
| `duped coupling` | imports that bypass a directory's entry file, by depth; surface, hiding, leaks and a 0–1 score per boundary | TS/JS, Python, Rust |
| `duped index` | greppable markdown list of every top-level function and type | all |
| `duped extract` | the same, as JSON Lines records | all |

Tests, mocks, fixtures, `node_modules`, `dist`, `build` and `.gitignore`d paths are skipped by default. `report` says how many: `scannedFiles`, `skippedTestFiles` (line 2 of the text); excluded, ignored and build-output paths are in neither.

Flags that widen a scan:

| Flag | Commands | Adds |
|---|---|---|
| `--include-tests` | all | test, mock and fixture files (`*.test.*`, `tests/`, `__tests__/`, `fixtures/`, …) |
| `--include-same-file` | `bodies` | pairs whose two functions share a file |
| `--cross-language` | `bodies` | pairs across languages, e.g. a TypeScript helper and its Rust port |
| `--include-same-module` | `types`, `names`, `imports`, `fns` | pairs inside one module |
| `--include-acknowledged` | `bodies`, `types`, `fns` | pairs acknowledged as deliberate |
| `--include-wrappers` | `fns` | pairs where one function calls the other |
| `--include-implementations` | `fns` | same-name methods of classes sharing a base |

`report --json` `truncated` is true when any of three lists was cut: `filePairs` > `groups | length`, `helperGroups` > `helpers | length`, `helperFamilies` > `families | length`.

## Coupling

`duped coupling [PATH] [--base REF] [--rev REF] [--folder P] [--all-edges]` measures, never gates. `--folder` is relative to `PATH`, like the JSON's paths: `coupling packages --folder canvas`, not `--folder packages/canvas` (a path matching no boundary scores 1.000). A boundary is a directory with an entry file (`index.*`, `__init__.py`, `mod.rs`, `lib.rs`, `main.rs`), a manifest, or the name `internal`. A Rust module an entry declares `pub mod x;` is an entry too. `bypass` counts boundaries entered after the first, plus 1 when the target isn't the entry: `../foo` → `foo/index.ts` is 0, `../foo/bar.ts` 1, `../foo/internal/x.ts` 2.

- Headline: `totals.score` (0–1, higher is better), the mean of `1 − bypassRate` (bypassing ÷ cross-boundary imported symbols), `1 − unusedSurface` (entry exports nobody outside imports, boundaries with consumers only) and `hiding` (declarations neither surfaced by an entry nor imported from outside). `bypassCost` and `meanBypassDepth` are diagnostics only.
- Cross-package: a bypass whose importer is in another package than its target. Listed first; `edges[].importerPackage` / `targetPackage`, `totals.crossPackageBypassEdges` / `crossPackageBypassSymbols`, `boundaries[].crossPackageBypassIn`, and `crossPackage: true` on `diff` changes. Not part of the score. Rank boundaries by `crossPackageBypassIn`, then `bypassIn`: counts, since a score ranks a boundary with 5 imports above one with 400.
- `--base REF`: `diff.improved` (shallower, moved, or importer deleted), `diff.regressed` (`priorImporters`: files that already bypassed to that symbol at base), `diff.removedWithoutMove` (import gone, symbol still in place: likely copied or inlined; no credit), `diff.surface` (`newModule` marks boundaries absent at base), `diff.leaks`.
- `--rev REF` scans a commit; combine with `--json` over `git rev-list --first-parent` for a history.
- `package.json` `exports` (patterns too), `main`, `module` and `types` targets are entries; a missing `dist/` path is read under `src/`.
- `noConsumers: true`: nothing in the scan imports the boundary, so its unused surface doesn't lower its score.
- tsconfig `paths` and `baseUrl` resolve (nearest `tsconfig*.json`, relative `extends`). Not resolved: `require`, C# `using`.

## Dependency tags

- **`tag` says what the dependency graph allows.** In JSON it's an object, `null` when modules aren't tracked; `tag.kind` is one of:

| `tag.kind` | Meaning | Usual fix |
|---|---|---|
| `importable` | module `tag.copy` already depends on module `tag.owner` | the side in `tag.copy` deletes its version and imports `tag.owner`'s, whichever side the change touched |
| `importable-indirect` | `tag.copy` depends on `tag.owner` only through other modules | add the direct dependency, then as above |
| `move-down` | neither depends on the other; both depend on `tag.home` | move one copy down. `home` is the lowest common dependency declaring the files' package imports; `missing` lists any it lacks. Files' own local imports (`./x`, `@/x`) must move too or be injected |
| `boundary` | no dependency path either way | often deliberate; flag only real drift |
| `same-module` | both in `tag.module` (hidden by default except in `bodies`) | consolidate locally |

## Deliberate copies

`types` hides a type and its implementer, two types sharing only what a scanned common base declares, and a `.d.ts` `declare module '<m>'` typing of a type in module `<m>`. It also hides pairs across languages (a Rust struct and its TypeScript wire mirror; TypeScript and JavaScript count as one): `--cross-language` shows them, and `[types] cross_language = true` shows them in `report` too.

`bodies` and `types` hide a pair when a doc comment or file header says "mirrors", "mirror of", "structural twin", "kept in sync" or "copy of" and names the other side. Otherwise record it in `duped.toml` at the project root:

```toml
[[acknowledged]]
a = "@x/web:formatBytes"      # Name, Scope.Name, or module:Name
b = "@x/cli:formatBytes"
reason = "cli is published standalone and can't depend on web"
```

Acknowledgements apply to `bodies` and `types`, where `--include-acknowledged` shows hidden pairs, and to `helpers` groups (a copy acknowledged against its group's first member is dropped, with no flag to show it; families ignore them). `names` and `imports` keep reporting the pair.

## Tuning

Other `duped.toml` sections: `[scan] exclude = [...]`, `[types]` and `[names]` thresholds, `[modules."<name>"]` (`ignore`, `path`, `deps`), `[rules.<language>]` test patterns. Flags win over the file. Common adjustments:

- Noise from directories meant to repeat themselves (examples, demos, generated code): `--exclude '**/examples'`, or `[scan] exclude`.
- `bodies` too strict or too loose: `--threshold` (default 0.5, `similar` only), `--min-tokens` (default 50).
- `helpers` too noisy: raise `--min-tokens` (default 8; a lone `return x;` counts as `x`).
- `imports` missing small files: `--min-shared 2` (default 3).
- `names` noisy: raise `--min-score` (default 2.5).
