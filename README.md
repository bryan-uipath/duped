# duped

Find duplicated functions and types across a codebase: same name, similar shape, or copied body. Supports TypeScript/JavaScript, Python, C# and Rust.

`duped` parses syntax with tree-sitter. It needs no compiler, no language server and no embeddings. Results are evidence for review: read both sides before refactoring.

> Early development. TypeScript and JavaScript are supported so far; see the roadmap in [DESIGN.md](DESIGN.md).

## Build

```sh
cargo build --release
```

## Usage

```sh
duped extract [DIR] [--out records.jsonl]   # every top-level function and type, as JSON Lines
duped index [DIR] [--out api-index.md]      # greppable markdown summary, one section per file
duped types [DIR]                           # types that share most of their properties
```

`DIR` can also be a single file.

### What gets scanned

- `.gitignore` rules apply, even outside a git checkout.
- Hidden files and directories (such as `.storybook/`) are skipped.
- `node_modules`, `dist`, `build`, `out`, `coverage` and `target` are always skipped.
- Test, mock and fixture files are skipped unless you pass `--include-tests`. That covers `*.test.*`, `*.spec.*`, `*.mock.*`, `*.fixture.*`, and directories such as `test/`, `__tests__/`, `mocks/` and `fixtures/`. The patterns are per language and can be changed in `duped.toml`.
- `--exclude <glob>` (repeatable) is relative to `DIR` (`[scan] exclude` in `duped.toml` is relative to the project root) and also matches directories. For example, `--exclude examples` skips everything under `examples/`. `*` stays within one path segment; `**` crosses segments.

### Records

Each line of `extract` output is one JSON object. Keys whose value would be empty, `null` or `false` are left out.

| Key | In | Meaning |
| --- | --- | --- |
| `record` | all | `function` or `type` |
| `language` | all | `typescript` or `javascript` |
| `name` | all | declared name; `default` for an anonymous default export |
| `scope` | all, optional | enclosing class or namespace, e.g. `Api.Client` |
| `exported` | all | part of the module's public API, including `export { x }` lists; private and protected class members are `false` |
| `file`, `start_line`, `end_line` | all | root-relative path and 1-based inclusive lines (from the `export` keyword or first decorator) |
| `doc` | all, optional | the adjacent `/** … */` comment as plain text |
| `params` | function | `[{ name, type?, optional? }]`; `this:` is omitted |
| `returns` | function, optional | return type as written |
| `kind` | type | `interface`, `type` (object alias), `class`, `enum`, `union` (literal members become fields) or `alias` (no fields) |
| `fields` | type | `[{ name, type?, optional?, kind }]`, where `kind` is `property`, `method` or `member`; quotes are stripped, so `'id'` and `id` match; overloads and getter/setter pairs share one field |
| `extends` | type, optional | base types and implemented interfaces, as written |

Overload signatures fold into their implementation, so each function appears once.

### Finding duplicate types

`duped types` compares every object-shaped type (interfaces, object type aliases, classes) with every other, and every enum or literal union with every other. It reports pairs whose field names overlap, whatever the types are called.

- **similarity:** Jaccard similarity of field names: shared names ÷ all names.
- **typed:** the same, but a shared field only counts when its types match, so it drops when shared fields have different types. Spacing doesn't matter, union order doesn't matter, and `T | undefined`, `T | null` and `T` count as the same type. A field with no type annotation matches any type.
- **relationship:**
  - `exact` (`A = B`): same names and types.
  - `superset` (`A ⊃ B`) or `subset` (`A ⊂ B`): one has every field of the other, and more. This is how a redeclared union that has fallen behind the original shows up.
  - `overlap` (`A ~ B`): anything else, including same names with different types.

By default it prints clusters, which are groups of types linked by qualifying pairs, largest first. Each cluster lists its members with `file:line`, the fields they all share, and its best pairs. `--pairs` prints a flat ranked list instead, and `--json` prints every pair and cluster (ignoring `--top` and `--pairs`).

| Flag | Default | Meaning |
| --- | --- | --- |
| `--threshold` | `0.7` | minimum similarity |
| `--min-fields` | `4` | skip types with fewer fields |
| `--min-shared` | `4` | a pair needs at least this many shared field names |
| `--exclude-name <glob>` | | skip types by name or `Scope.Name` (repeatable); `*Props` is always skipped unless `--no-default-excludes` |
| `--common-field-fraction` | `0.1` | field names on more than this fraction of types (and more than 50 of them), such as `id`, don't seed candidate pairs; they still count in scores, and a type made only of common fields is paired through its rarest one |
| `--top` | `40` | clusters (or pairs) to show |
| `--include-same-module` | off | also report pairs whose two types are in one module |
| `--include-acknowledged` | off | also report pairs acknowledged as deliberate |

Every pair is evidence, not a verdict. Read both types before consolidating them.

### Modules and what to do about a pair

`duped` finds the project's modules and tags every pair with what the dependency graph allows:

| Tag | Meaning |
| --- | --- |
| `importable` | one side's module already depends on the other's: `vsix can import @x/core` |
| `importable-indirect` | it depends on it only through other modules; add a direct dependency first |
| `move-down` | neither depends on the other, but both depend on a lower module that could hold one copy |
| `boundary` | no dependency path either way, or both sides are under no module; often deliberate |
| `same-module` | both in one module; hidden unless `--include-same-module` |

Clusters are ranked by their most actionable pair: `importable` first, then `importable-indirect`, `move-down` and `boundary`. Each cluster also names a home when there is one: a member's module that every other member can import, or else the lowest module they all depend on.

Modules come from manifests under the project root:

- `package.json` files with a `name`, limited to the workspace's packages when `pnpm-workspace.yaml` or a root `workspaces` field declares them; `workspace:` and `npm:` aliases resolve;
- Cargo crates, limited to `[workspace] members` and the crates their path dependencies point at, as Cargo does; path, renamed and `workspace = true` dependencies resolve;
- `pyproject.toml` projects (PEP 621 or Poetry; uv workspace members when declared), with names normalised;
- `.csproj` files, with `ProjectReference`s as dependencies.

Every dependency kind counts (`dependencies`, `devDependencies`, `peerDependencies`, `optionalDependencies`, dev and build dependencies), because bundled apps often list workspace packages as dev dependencies. Manifests in one directory (say a `package.json` beside a `Cargo.toml`) are one module, named after the first in that order; the other names still work in `duped.toml`. Each file belongs to the module with the deepest root containing it; files under none belong to `(root)`.

When the scanned types span fewer than two modules, for example when you scan one package of a workspace, pairs aren't tagged and none are hidden as `same-module`. Acknowledgements still apply.

The **project root** is found from `DIR`, without leaving its git checkout: the nearest directory with a `duped.toml`, else the nearest workspace root (`pnpm-workspace.yaml`, `package.json` with `workspaces`, a Cargo `[workspace]`, a uv workspace, or a `.sln`), else the checkout. Outside a checkout it is `DIR`. `--root` sets it explicitly.

A pair is **acknowledged**, and hidden unless `--include-acknowledged`, when `duped.toml` lists it, or when either type's doc comment, or the comment at the top of its file, calls it deliberate ("mirrors", "mirror of", "structural twin", "kept in sync", "copy of") and names the other side: the other type as a whole word (not when both types share the name), or the other module by its full name (`@x/unified-evals`), its last name segment (`unified-evals`) or its directory (`evals`), the last two as whole words. Licence and copyright headers are ignored.

### `duped.toml`

Read from the project root, or from `--config <file>`. Flags win over the file, and the file wins over the defaults; globs from both are combined.

```toml
[scan]
exclude = ["examples", "packages/legacy"]   # relative to the project root
include_tests = false

[types]
min_fields = 4
min_shared = 4
threshold = 0.7
common_field_fraction = 0.1
exclude_names = ["*Dto"]                   # in addition to *Props
default_excludes = true                    # false drops the *Props exclusion

[modules."@x/legacy"]
ignore = true                              # its files fall to the enclosing module; paths through it stay

[modules.shared]
path = "libs/shared"                       # define a module with no manifest

[modules."@x/app"]
deps = ["@x/core", "shared"]               # replace detected dependencies

[[acknowledged]]
a = "@x/canvas:DocumentAnnotationRow"      # Name, Scope.Name, or module:Name
b = "DocumentAnnotationRow"
reason = "canvas can't depend on evals"

[rules.typescript]                         # typescript or javascript
conventional_members = { add = ["dispose"], remove = ["toJSON"] }
test_dirs = ["test", "tests", "__tests__"] # an array replaces the defaults
```

**Conventional members** are names that don't count toward type similarity, so two unrelated classes don't look alike just because both define `toString`. The TypeScript and JavaScript defaults are `toString`, `toJSON`, `valueOf`, `constructor` and `[Symbol.iterator]`.

## Testing

`cargo test` runs the unit tests, the end-to-end tests and the regression suite in `tests/regression.rs`.

The regression suite scans `tests/fixtures/workspace`, a synthetic pnpm workspace in which each type is planted to produce one expected finding (an importable copy, drift, an edge-shape family, move-down, importable-indirect, boundary, acknowledged pairs) or to be filtered out as noise. The test asserts every expected pair, its tag and relationship, and that nothing else is reported, so a new false positive fails CI as surely as a lost finding.

### Checking a real repository

`real_repository_expectations` runs only when two environment variables are set:

```sh
DUPED_REGRESSION_ROOT=/path/to/repo \
DUPED_REGRESSION_EXPECT=/path/to/expected.toml \
cargo test --release --test regression real_repository
```

The expectations file belongs to the repository being checked, so keep it outside this one. It scans with `--include-acknowledged --include-same-module` and checks each `[[expect]]` pair; each `[[missing]]` pair must stay unreported, so a fix is noticed and the entry moved to `[[expect]]`.

```toml
path = "packages"                # what to scan, relative to the root [default: the root]
args = ["--min-fields", "3"]     # extra `duped types` arguments [optional]

[[expect]]
a = "@x/api:HostAuth"            # module:Name or module:Scope.Name
b = "@x/core:AuthContext"
tag = "importable"               # importable, importable-indirect, move-down, boundary or same-module
copy = "@x/api"                  # optional checks; omit any you don't care about
owner = "@x/core"
home = "@x/base"                 # for move-down
relationship = "exact"           # from a's side: exact, superset, subset or overlap
acknowledged = false             # true when hidden as deliberate by default

[[missing]]
a = "@x/api:Status"
b = "@x/core:Status"
reason = "the original is a const object, not a union"
```
