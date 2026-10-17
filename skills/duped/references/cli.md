# duped CLI reference

Read the relevant section for installation, detector selection, dependency tags, acknowledgements, or tuning.

## Install

```bash
duped report --help >/dev/null 2>&1 || cargo install --locked --force --git https://github.com/bryan-uipath/duped   # needs a build with `report`
```

## Commands

Every command takes `[PATH]` (default `.`) plus `--exclude <glob>` (repeatable), `--include-tests`, `--root`, `--config`, `--out`. Analyses also take `--json` and `--top N`. Exclude globs are anchored at `PATH`: `examples` skips only `PATH/examples`; use `**/examples` for every `examples/` directory.

| Command | Finds | Languages |
|---|---|---|
| `duped bodies` | functions whose bodies are near-copies under any names (identifiers abstracted, literals kept); files linked through ≥2 such functions | TS/JS, Rust |
| `duped imports` | files importing the same rare set of modules and symbols: ported copies whose declarations were all renamed | TS/JS |
| `duped names` | file pairs declaring ≥2 of the same top-level names; each name marked `=` same signature/shape, `~` differs, `?` unknown | all |
| `duped types` | types sharing most of their fields under any names: `exact`, `subset`, `superset`, `overlap` | all |
| `duped report` | all four analyses at once, grouped by file pair (see SKILL.md) | all |
| `duped index` | greppable markdown list of every top-level function and type | all |
| `duped extract` | the same, as JSON Lines records | all |

Tests, mocks, fixtures, `node_modules`, `dist`, `build` and `.gitignore`d paths are skipped by default.

## Dependency tags

- **`tag` says what the dependency graph allows.** In JSON it's an object, `null` when modules aren't tracked; `tag.kind` is one of:

| `tag.kind` | Meaning | Usual fix |
|---|---|---|
| `importable` | module `tag.copy` already depends on module `tag.owner` | the side in `tag.copy` deletes its version and imports `tag.owner`'s, whichever side the change touched |
| `importable-indirect` | `tag.copy` depends on `tag.owner` only through other modules | add the direct dependency, then as above |
| `move-down` | neither depends on the other; both depend on `tag.home` | move one copy down. `home` is *a* common dependency, not necessarily the best fit: pick the lowest module that suits the code |
| `boundary` | no dependency path either way | often deliberate; flag only real drift |
| `same-module` | both in `tag.module` (hidden by default except in `bodies`) | consolidate locally |

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
