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
- Test, mock and fixture files are skipped unless you pass `--include-tests`. That covers `*.test.*`, `*.spec.*`, `*.mock.*`, `*.fixture.*`, and directories such as `test/`, `__tests__/`, `mocks/` and `fixtures/`.
- `--exclude <glob>` (repeatable) is relative to `DIR` and also matches directories. For example, `--exclude examples` skips everything under `examples/`. `*` stays within one path segment; `**` crosses segments.

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

Every pair is evidence, not a verdict. Read both types before consolidating them.
