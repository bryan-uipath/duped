# duped

Find duplicated functions and types across a codebase: same name, similar shape, or copied body. Supports TypeScript/JavaScript, Python, C# and Rust.

`duped` parses syntax with tree-sitter. It needs no compiler, no language server and no embeddings. Results are evidence for review: read both sides before refactoring.

> Early development. TypeScript, JavaScript, Rust, Python and C# are supported so far; see the roadmap in [DESIGN.md](DESIGN.md).

## Build

```sh
cargo build --release
```

## Usage

```sh
duped extract [DIR] [--out records.jsonl]   # every top-level function and type, as JSON Lines
duped index [DIR] [--out api-index.md]      # greppable markdown summary, one section per file
duped types [DIR]                           # types that share most of their properties
duped bodies [DIR]                          # functions whose bodies are near-copies (TypeScript/JavaScript)
duped imports [DIR]                         # TS/JS files that import the same rare things
duped names [DIR]                           # file pairs that declare the same names
duped report [DIR] [--base REF]             # every analysis, grouped by file pair
duped fns [DIR]                             # functions that share most of their parameter names
```

`DIR` can also be a single file.

### What gets scanned

- `.gitignore` rules apply, even outside a git checkout.
- Hidden files and directories (such as `.storybook/`) are skipped.
- `node_modules`, `dist`, `build`, `out`, `coverage`, `target`, `venv` and `site-packages` are always skipped.
- A `.pyi` stub next to a `.py` module of the same name is skipped, since both describe one API.
- Test, mock and fixture files are skipped unless you pass `--include-tests`. That covers `*.test.*`, `*.spec.*`, `*.mock.*`, `*.fixture.*`, pytest's `test_*.py`, `*_test.py` and `conftest.py` (and `.pyi` equivalents), Rust `tests.rs` / `test.rs` module files, C# `*Tests.cs`, `*Mock.cs`, `*Fake.cs` and `Fake*.cs`, directories such as `test/`, `__tests__/`, `mocks/`, `fixtures/`, `benches/` and `UnitTests/` (directory names match case-insensitively), and C# test projects ending `.Tests`, `.Test`, `.UnitTests` or `.IntegrationTests` (the C# rules apply to `.cs` files only). The patterns are per language and can be changed in `duped.toml`.
- C# files under `obj/` or `bin/` (build output, such as generated `*.g.cs`) are skipped.
- Files are read as UTF-8, or UTF-16 when they start with a byte-order mark. Invalid UTF-8 bytes, as in legacy single-byte files, become `�` rather than skipping the file.
- `--exclude <glob>` (repeatable) is relative to `DIR` (`[scan] exclude` in `duped.toml` is relative to the project root) and also matches directories. For example, `--exclude examples` skips everything under `examples/`. `*` stays within one path segment; `**` crosses segments.

### Records

Each line of `extract` output is one JSON object. Keys whose value would be empty, `null` or `false` are left out.

| Key | In | Meaning |
| --- | --- | --- |
| `record` | all | `function` or `type` |
| `language` | all | `typescript`, `javascript`, `rust`, `python` or `csharp` |
| `name` | all | declared name; `default` for an anonymous default export |
| `scope` | all, optional | enclosing class or namespace, e.g. `Api.Client` |
| `exported` | all | part of the module's public API, including `export { x }` lists; private and protected class members are `false` |
| `file`, `start_line`, `end_line` | all | root-relative path and 1-based inclusive lines (from the `export` keyword or first decorator) |
| `doc` | all, optional | the adjacent `/** … */` comment (TypeScript) or the docstring (Python), with whitespace collapsed and escapes left as written |
| `params` | function | `[{ name, type?, optional?, fields? }]`; TypeScript's `this:` and Python's method receiver (`self`, `cls`) are omitted. A destructured object (TypeScript/JavaScript) keeps its written `name` and `type`, and `fields` lists its keys as `[{ name, type?, optional? }]`: `{ id, force = false }` gives `id` and `force?`. Key types come from an inline object type, or from an interface or object type alias of that name in the same file; `...rest` is left out |
| `returns` | function, optional | return type as written |
| `kind` | type | `interface`, `type` (object alias), `class`, `enum`, `union` (literal members become fields), `alias` (no fields), `struct`, `trait` or `record` |
| `fields` | type | `[{ name, type?, optional?, kind }]`, where `kind` is `property`, `method` or `member`; quotes are stripped, so `'id'` and `id` match; overloads and getter/setter pairs share one field |
| `extends` | type, optional | base types and implemented interfaces, as written |

Overload signatures (TypeScript overloads, Python `@overload`) fold into their implementation, so each function appears once.

### Rust

- **Functions:** free `fn`s, `impl` methods, trait methods with a default body, and `extern` functions. Receivers (`self`, `&mut self`) are left out of `params`.
- **`scope`:** the inline `mod` chain plus the impl or trait type, e.g. `parser.Tokenizer`. `impl<T> Display for Foo<T>` is scoped to `Foo`. Impls without a named target are scoped to the trait: `impl<T> Shout for T`, `for (A, B)` and `for [u8; 4]` give `Shout`, and `dyn Any` gives `Any`.
- **Types:** structs and unions (`struct`; tuple fields are named `0`, `1`, …), enums (variants become members), traits (`trait`; methods, associated types and consts become fields; supertraits go in `extends`) and type aliases (`alias`). Impl methods are not added to a struct's fields.
- **`exported`:** plain `pub` only; `pub(crate)` and `pub(super)` count as internal. Methods in `impl Trait for X` have no `pub` of their own: they're exported unless `X` or the trait is private and defined in the same file (`super::`, `self::` and `crate::` paths are resolved within the file). Inherent methods also need `pub`.
- **`doc` and `start_line`:** `///` and `/** */` comments above the item, skipping attributes such as `#[derive]`, blank lines and plain `//` comments, as rustc does; `start_line` is the first attribute.
- **Skipped:** files starting with `#![cfg(test)]`, items under `#[cfg(test)]` or `#[cfg(all(test, …))]`, `#[test]` and `#[…::test(…)]` functions, and macros. These are skipped even with `--include-tests`, which only affects which files are walked.

### Python

- **Exported:** if the module assigns `__all__` a literal list or tuple of strings, and doesn't change it later (`+=`, `.extend()`, …), a top-level name is exported exactly when it's listed. Otherwise a name is exported when it doesn't start with `_`; dunder names like `__eq__` count as public. Methods are exported when their class is and their own name passes the same rule.
- **Classes:** fields come from annotated or plain class-body assignments, `self.x = …` assignments in `__init__`, `@property` / `@cached_property` methods (as properties, typed by the getter's return or else the setter's parameter) and other methods (as methods). Dunder names (`__slots__`, `__repr__`) are not fields, though dunder methods still get function records. This covers dataclasses, pydantic and attrs models, `TypedDict` and `NamedTuple` the same way. `__init__` itself is not a field or a function record.
- **Enums:** subclasses of `Enum`, `IntEnum`, `StrEnum`, `Flag` and `IntFlag` have kind `enum`, and their class-body assignments are the members, except sunder (`_order_`) and private (`__x`) names. Subclasses of enums defined earlier in the same file are enums too.
- **Aliases:** `type X = …` and `X: TypeAlias = …`.
- **Annotations:** quoted forward references are unquoted, so `"Base"` and `Base` match.
- **Scope:** module level and `if TYPE_CHECKING:` blocks; functions nested in functions are skipped.

### C#

- Every class, struct, interface, record, enum and delegate is extracted, including nested types. `scope` is the enclosing type chain (`Outer.Inner`); namespaces are not part of it.
- Fields are properties, fields (one per declarator), events and methods; all but methods have `kind: "property"`. A record's positional parameters are properties. Constructors, finalizers, operators and indexers are left out.
- Function records are methods with a body, including default interface methods; abstract and interface signatures are fields only. Overloads with bodies stay separate.
- `exported` means `public` (or an interface member without an access modifier, or an explicit interface implementation) on an exported type. `internal` is `false`.
- Parameter modifiers stay in the type: `this string`, `out int`, `params int[]`.
- `doc` is the `///` XML comment with tags stripped (`<see cref="T:X"/>` keeps `X`); plain `//` lines in between are skipped and `////` is not a doc comment. `start_line` includes attributes.
- Enum members inside `#if` / `#else` are all kept. An enum's storage type (`enum E : byte`) is not listed in `extends`.
- Each `partial` declaration is its own record.

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

### Finding copied function bodies

`duped bodies` finds functions whose bodies are near-copies, whatever they are called, such as a module ported to another package with renamed locals and a few added guards. TypeScript and JavaScript only for now.

- **Tokens:** each body's tokens, with comments dropped and every identifier (local or not) replaced by one placeholder, including those inside template `${…}`. Keywords, operators, property names (shorthand `{ id }` too) and literals are kept, so lookup tables with the same `switch` shape but different labels don't match.
- **similarity:** Jaccard similarity of the two bodies' token shingles (runs of `--shingle` tokens). Repeated shingles count separately, so a ten-case `switch` doesn't match a three-case one.
- **shared:** shingles both bodies have, roughly the number of duplicated tokens. Pairs are ranked by it, so a large near-copy outranks a small exact one.
- **Files sharing several functions:** two files linked by pairs between at least two functions on each side, ranked by total shared. These are usually copied modules rather than one copied helper.

Candidate pairs come from MinHash signatures with LSH banding, tuned to find a pair at the threshold 99% of the time; each candidate's similarity is then computed exactly. Pairs are tagged and acknowledged as for `types` (below), except that same-module pairs are shown; same-file pairs are hidden instead.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--threshold` | `0.5` | minimum similarity |
| `--shingle` | `5` | tokens per shingle |
| `--min-tokens` | `50` | skip smaller bodies |
| `--top` | `40` | file pairs and function pairs to show |
| `--include-same-file` | off | also report pairs within one file |
| `--include-acknowledged` | off | also report pairs acknowledged as deliberate |

`--json` prints every pair and file pair.

### Finding ported files by their imports

A file copied into another package keeps importing the same unusual things, even when every declaration in it was renamed. `duped imports` compares the `import` statements of TypeScript and JavaScript files.

- **Items:** each imported name is one item, `specifier#name` (`default` for a default import, `*` for a namespace); a side-effect import such as a stylesheet is its specifier. Type-only imports count. Re-exports, `require` and dynamic `import()` don't.
- **Relative imports** keep only the target's last segment: `./a.js`, `../x/a` and `./a/index` are all `./a`. A copied folder imports its own copied siblings, so their names still match across packages; a resolved path never would.
- **score:** Jaccard similarity weighted by IDF (`ln((files + 1) / files with the item)`), so an item in nearly every file, like `react#useState`, weighs almost nothing.
- **rare:** an item in at most 10 files, or at most `--rare-fraction` of them, and never in more than half. Only rare items seed candidate pairs, through an inverted index, so files aren't compared all against all.
- A pair needs rare items from at least `--min-shared` specifiers. Specifiers rather than items, so the parts of one compound component (`Dialog`, `DialogTitle`, …) count once.

Pairs are tagged by module like `types` pairs, ranked by score, and same-module pairs (mostly sibling files of one feature) are hidden by default.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--threshold` | `0.5` | minimum weighted similarity |
| `--min-shared` | `3` | rare shared items must come from at least this many specifiers |
| `--rare-fraction` | `0.005` | items in more than this fraction of files (and more than 10) are not rare; they still score |
| `--top` | `40` | pairs to show |
| `--include-same-module` | off | also report pairs whose two files are in one module |

`--json` prints every pair with its shared items, rarest first. Acknowledgements and `duped.toml` thresholds don't apply yet.

### Finding files that share names

`duped names` reports file pairs, not single names: "these two files declare N of the same top-level names" is how a ported or forked module shows up, even after the copy renamed its main export. Each pair lists its shared names, marked `=` when the signature (functions) or shape (types) matches, `~` when it differs and `?` when a side is an alias, whose target isn't recorded.

- Only top-level declarations count. Methods belong to their type's shape, and `default` (an anonymous default export) is not a name. TypeScript and JavaScript names match each other; other languages match only themselves. A type and a function with the same name are different names.
- **score:** the sum of the shared names' weights. A name weighs `ln(files / n) / ln(files / 2)` when `n` of the `files` with top-level declarations declare it, so a name in two files weighs 1, then ×1.5 when both sides export it and ×1.5 when its signature or shape matches. Function signatures compare parameter and return types as written (spacing aside), so `T | null` and `T` differ. Field optionality doesn't count, as in `types`.
- A single shared name is a collision, not a copied file, so a pair needs at least two. Two private names that differ score 2, below the default minimum of 2.5; an identical exported name plus another rare name clears it.
- Pairs are ranked by score and tagged like `types` pairs. Same-module pairs are hidden unless `--include-same-module`. Acknowledgements don't apply yet.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--min-shared` | `2` | a pair needs at least this many shared names |
| `--min-score` | `2.5` | minimum pair score |
| `--max-files` | `10` | names declared in more files than this, such as `render`, don't count |
| `--top` | `40` | pairs to show |
| `--include-same-module` | off | also report pairs whose two files are in one module |
| `--json` | off | every pair, with each shared name's weight and both signatures |

### Running every analysis at once

`duped report` runs `bodies`, `imports`, `names` and `types` with their defaults (and `duped.toml`), over one walk and one module graph, and groups their pairs by file pair. A file pair found by several analyses is most likely a ported module, so groups are ranked by how many analyses found them, then by how many findings they have. Findings within a group are ordered by their analysis's score (shared shingles, rare imports, name score or field similarity).

- `--base <ref>` keeps only file pairs with a side added, modified or renamed since the merge base of `<ref>` and `HEAD`, including uncommitted and untracked files under `DIR`. It needs git; without it, git isn't used.
- `--top` (default `40`) limits the file pairs shown, in `--json` too; `filePairs` and `truncated` say how many there were.
- An analysis that fails is reported as `failed`, with its error, and the others still run.

`--json` prints `{ directory, mode, base, changedFiles, detectors, filePairs, truncated, groups }`. `detectors.<name>` is `{ status: "ok", total, kept }` (`total` across `DIR`, `kept` touching the change) or `{ status: "failed", error }`. Each group has `a`, `b`, `changed` (with `--base`), `detectors`, `tag` and `findings`, each `{ detector, kind, tag, a, b, evidence }` where `kind` is `function`, `type` or `file`.

### Finding duplicate functions

`duped fns` compares parameter lists, whatever the functions are called. A destructured object's keys count as parameters, so `Panel({ user, compact })` matches `open(user, compact)`. It is a prototype: expect a weak signal, and read both sides.

- **similarity:** Jaccard similarity of parameter names (case-insensitive), with each name weighted by how rare it is, so `props` or `value` count for little and `retryPolicy` for a lot.
- **typed:** the same, but a shared name only counts when its types match (as in `types`), plus the return type when both sides declare one.
- **sharing:** how many functions have every shared name. When more than `--max-sharing` do, the pair is a convention, such as `(a, b)` comparators, and is skipped.

Pairs in one file are skipped. These are hidden, each with a flag to show them:

- **same module** (`--include-same-module`) and **acknowledged** (`--include-acknowledged`), as for types; `[[acknowledged]]` entries can name functions.
- **implementations** (`--include-implementations`): same-name methods whose classes share a base class or interface, or where one class extends the other.
- **wrappers** (`--include-wrappers`): one function calls the other, directly or through an import alias (`import { load as loadShared }`), passing every one of its own parameters; or it is a bodiless `declare` (or `.d.ts`) signature of the same name. A bare call to a function's own name is recursion, not a wrapper.

Pairs are ranked like type pairs: most actionable tag first, then most similar. `--json` prints every pair.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--threshold` | `0.7` | minimum weighted similarity |
| `--min-params` | `2` | skip functions with fewer parameter names |
| `--min-shared` | `2` | a pair needs at least this many shared names |
| `--max-sharing` | `5` | skip pairs whose shared names all appear together on more functions than this |
| `--top` | `40` | pairs to show |

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

When the scanned types (for `names`, the scanned declarations) span fewer than two modules, for example when you scan one package of a workspace, pairs aren't tagged and none are hidden as `same-module`. Acknowledgements still apply to `types`.

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

[names]
min_shared = 2
min_score = 2.5
max_files = 10

[modules."@x/legacy"]
ignore = true                              # its files fall to the enclosing module; paths through it stay

[modules.shared]
path = "libs/shared"                       # define a module with no manifest

[modules."@x/app"]
deps = ["@x/core", "shared"]               # replace detected dependencies

[[acknowledged]]
a = "@x/canvas:DocumentAnnotationRow"      # type or function: Name, Scope.Name, or module:Name
b = "DocumentAnnotationRow"
reason = "canvas can't depend on evals"

[rules.typescript]                         # typescript, javascript, python, rust or csharp
conventional_members = { add = ["dispose"], remove = ["toJSON"] }
test_dirs = ["test", "tests", "__tests__"] # an array replaces the defaults
```

**Conventional members** are names that don't count toward type similarity, so two unrelated classes don't look alike just because both define `toString`. The TypeScript and JavaScript defaults are `toString`, `toJSON`, `valueOf`, `constructor` and `[Symbol.iterator]`; other languages have none.

**Test patterns** match a file name (`test_files`) or any directory name above it (`test_dirs`, ignoring case). `*` matches any run of characters, so `*.test.*` matches `a.test.ts`; a pattern without `*` is an exact name.

## Agent skills

`skills/` holds three skills for coding agents:

- **duped**: using the CLI, checking for existing code before writing new code, reading findings.
- **duped-audit**: auditing a whole codebase and ranking what to consolidate.
- **duped-review**: checking a branch or pull request for copies of existing code.

They need the `duped` binary on `PATH`.

Claude Code, as a plugin:

```text
/plugin marketplace add bryan-uipath/duped
/plugin install duped@duped
```

Any agent that reads skill directories (Claude Code, Codex): link or copy all three, keeping them side by side.

```sh
for s in duped duped-audit duped-review; do ln -s "$PWD/skills/$s" ~/.claude/skills/$s; done   # or ~/.codex/skills
```

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
